//! Bounded memory SELECT grammar. This module never accepts a physical relation
//! from a query. The execution gateway supplies an independently restricted view.
use sha2::{Digest, Sha256};
use sqlparser::{
    ast::{
        BinaryOperator, DataType, Expr, FunctionArg, FunctionArgExpr, FunctionArguments,
        GroupByExpr, Ident, ObjectName, SelectItem, SetExpr, Statement, TableFactor, UnaryOperator,
        Value,
    },
    dialect::MySqlDialect,
    parser::Parser,
    tokenizer::{Token, Tokenizer, Whitespace},
};

mod execution;
pub use execution::{MemorySqlGateway, MemorySqlResult};

pub const MEMORY_SQL_MAX_BYTES: usize = 32768;
pub const MEMORY_SQL_MAX_ROWS: usize = 100;
const COLUMNS: &[&str] = &[
    "memory_id",
    "subject_id",
    "content",
    "memory_type",
    "trust_tier",
    "initial_confidence",
    "session_id",
    "observed_at",
    "created_at",
    "extra_metadata",
    "embedding",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySqlError {
    Invalid,
    Unsupported,
    Disabled,
    Forbidden,
    Busy,
    Timeout,
    Unavailable,
    ResultLimit,
}

// Intentionally not Debug: neither SQL literals nor query parameters belong in logs.
pub struct PreparedMemoryQuery {
    statement: Statement,
    pub fingerprint: String,
    pub parameter_count: usize,
    bindings: Vec<Binding>,
}

enum Binding {
    Caller(usize),
    Literal(serde_json::Value),
}

impl PreparedMemoryQuery {
    pub fn parse(sql: &str) -> Result<Self, MemorySqlError> {
        if sql.is_empty() || sql.len() > MEMORY_SQL_MAX_BYTES {
            return Err(MemorySqlError::Invalid);
        }
        // Comments/hints are outside the first-version grammar. In particular,
        // never silently discard MySQL executable comments before execution.
        let tokens = Tokenizer::new(&MySqlDialect {}, sql)
            .tokenize()
            .map_err(|_| MemorySqlError::Invalid)?;
        if tokens.iter().any(|token| {
            matches!(
                token,
                Token::Whitespace(
                    Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
                )
            )
        }) {
            return Err(MemorySqlError::Unsupported);
        }
        let mut statements = Parser::new(&MySqlDialect {})
            .with_recursion_limit(48)
            .try_with_sql(sql)
            .map_err(|_| MemorySqlError::Invalid)?
            .parse_statements()
            .map_err(|_| MemorySqlError::Invalid)?;
        if statements.len() != 1 {
            return Err(MemorySqlError::Unsupported);
        }
        let mut statement = statements.remove(0);
        let mut validator = Validator {
            parameters: 0,
            nodes: 0,
            aliases: Vec::new(),
            bindings: Vec::new(),
        };
        validate_query(&mut statement, &mut validator, false)?;
        let mut normalized = statement.clone();
        // Parse and redact a second AST, not regex substitution of SQL text.
        validate_query(
            &mut normalized,
            &mut Validator {
                parameters: 0,
                nodes: 0,
                aliases: Vec::new(),
                bindings: Vec::new(),
            },
            true,
        )?;
        Ok(Self {
            fingerprint: format!("{:x}", Sha256::digest(normalized.to_string().as_bytes())),
            parameter_count: validator.parameters,
            statement,
            bindings: validator.bindings,
        })
    }

    pub fn bound_parameters(
        &self,
        parameters: &[serde_json::Value],
    ) -> Result<Vec<serde_json::Value>, MemorySqlError> {
        if parameters.len() != self.parameter_count {
            return Err(MemorySqlError::Invalid);
        }
        Ok(self
            .bindings
            .iter()
            .map(|binding| match binding {
                Binding::Caller(index) => parameters[*index].clone(),
                Binding::Literal(value) => value.clone(),
            })
            .collect())
    }

    /// Only server-owned binding identifiers can be used here. The client
    /// grammar requires the sole logical relation `memories` without aliases.
    pub fn sql_for_view(&self, database: &str, view: &str) -> Result<String, MemorySqlError> {
        if !valid_identifier(database) || !valid_identifier(view) {
            return Err(MemorySqlError::Invalid);
        }
        let mut statement = self.statement.clone();
        // Execute literals through the same parameter protocol as caller values.
        // This avoids parser/server string-escape differences, rather than
        // relying on SQL quoting as an injection defence.
        validate_query(
            &mut statement,
            &mut Validator {
                parameters: 0,
                nodes: 0,
                aliases: Vec::new(),
                bindings: Vec::new(),
            },
            true,
        )?;
        let Statement::Query(query) = &mut statement else {
            unreachable!()
        };
        let SetExpr::Select(select) = query.body.as_mut() else {
            unreachable!()
        };
        let TableFactor::Table { name, .. } = &mut select.from[0].relation else {
            unreachable!()
        };
        *name = ObjectName(vec![
            Ident::with_quote('`', database),
            Ident::with_quote('`', view),
        ]);
        Ok(statement.to_string())
    }
}

pub fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && value.as_bytes()[0].is_ascii_alphabetic()
}

struct Validator {
    parameters: usize,
    nodes: usize,
    aliases: Vec<String>,
    bindings: Vec<Binding>,
}

fn validate_query(
    statement: &mut Statement,
    validator: &mut Validator,
    redact: bool,
) -> Result<(), MemorySqlError> {
    let Statement::Query(query) = statement else {
        return Err(MemorySqlError::Unsupported);
    };
    if query.with.is_some()
        || query.offset.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.limit_by.is_empty()
    {
        return Err(MemorySqlError::Unsupported);
    }
    match query.limit.as_ref() {
        Some(Expr::Value(Value::Number(n, false)))
            if n.parse::<usize>()
                .is_ok_and(|v| v > 0 && v <= MEMORY_SQL_MAX_ROWS) =>
        {
            ()
        }
        _ => return Err(MemorySqlError::Invalid),
    }
    let SetExpr::Select(select) = query.body.as_mut() else {
        return Err(MemorySqlError::Unsupported);
    };
    if select.distinct.is_some()
        || select.top.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !matches!(&select.group_by, GroupByExpr::Expressions(exprs, modifiers) if exprs.is_empty() && modifiers.is_empty())
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.value_table_mode.is_some()
        || select.connect_by.is_some()
        || select.from.len() != 1
        || !select.from[0].joins.is_empty()
        || select.projection.is_empty()
        || select.projection.len() > 32
    {
        return Err(MemorySqlError::Unsupported);
    }
    match &select.from[0].relation {
        TableFactor::Table {
            name,
            alias: None,
            args: None,
            with_hints,
            version: None,
            with_ordinality: false,
            partitions,
            json_path: None,
        } if name.0.len() == 1
            && name.0[0].value == "memories"
            && with_hints.is_empty()
            && partitions.is_empty() =>
        {
            ()
        }
        _ => return Err(MemorySqlError::Unsupported),
    }
    for item in &mut select.projection {
        match item {
            SelectItem::UnnamedExpr(expr) => validator.expression(expr, redact, false)?,
            SelectItem::ExprWithAlias { expr, alias }
                if valid_identifier(&alias.value) && alias.quote_style.is_none() =>
            {
                validator.expression(expr, redact, false)?;
                validator.aliases.push(alias.value.clone());
            }
            _ => return Err(MemorySqlError::Unsupported),
        }
    }
    if let Some(expr) = &mut select.selection {
        validator.expression(expr, redact, false)?;
    }
    if let Some(order) = &mut query.order_by {
        if order.interpolate.is_some() || order.exprs.len() > 8 {
            return Err(MemorySqlError::Unsupported);
        }
        for item in &mut order.exprs {
            if item.with_fill.is_some() {
                return Err(MemorySqlError::Unsupported);
            }
            validator.expression(&mut item.expr, redact, true)?;
        }
    }
    Ok(())
}

impl Validator {
    fn expression(
        &mut self,
        expr: &mut Expr,
        redact: bool,
        allow_alias: bool,
    ) -> Result<(), MemorySqlError> {
        self.nodes += 1;
        if self.nodes > 512 {
            return Err(MemorySqlError::Invalid);
        }
        match expr {
            Expr::Identifier(name)
                if name.quote_style.is_none()
                    && (COLUMNS.contains(&name.value.as_str())
                        || allow_alias && self.aliases.contains(&name.value)) =>
            {
                ()
            }
            Expr::Value(Value::Placeholder(marker)) if marker == "?" => {
                self.bindings.push(Binding::Caller(self.parameters));
                self.parameters += 1;
            }
            Expr::Value(
                value @ (Value::Number(_, _)
                | Value::SingleQuotedString(_)
                | Value::Boolean(_)
                | Value::Null),
            ) => {
                let bound = match value {
                    Value::Number(number, _) => {
                        serde_json::from_str(number).map_err(|_| MemorySqlError::Invalid)?
                    }
                    Value::SingleQuotedString(string) => serde_json::Value::String(string.clone()),
                    Value::Boolean(value) => serde_json::Value::Bool(*value),
                    Value::Null => serde_json::Value::Null,
                    _ => unreachable!(),
                };
                self.bindings.push(Binding::Literal(bound));
                if redact {
                    *value = Value::Placeholder("?".into());
                }
            }
            Expr::Nested(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
                self.expression(inner, redact, allow_alias)?
            }
            Expr::UnaryOp {
                op: UnaryOperator::Not | UnaryOperator::Minus | UnaryOperator::Plus,
                expr,
            } => self.expression(expr, redact, allow_alias)?,
            Expr::BinaryOp { left, op, right }
                if matches!(
                    op,
                    BinaryOperator::Eq
                        | BinaryOperator::NotEq
                        | BinaryOperator::Gt
                        | BinaryOperator::GtEq
                        | BinaryOperator::Lt
                        | BinaryOperator::LtEq
                        | BinaryOperator::And
                        | BinaryOperator::Or
                        | BinaryOperator::Plus
                        | BinaryOperator::Minus
                        | BinaryOperator::Multiply
                        | BinaryOperator::Divide
                ) =>
            {
                self.expression(left, redact, allow_alias)?;
                self.expression(right, redact, allow_alias)?;
            }
            Expr::InList { expr, list, .. } if list.len() <= 32 => {
                self.expression(expr, redact, allow_alias)?;
                for value in list {
                    self.expression(value, redact, allow_alias)?;
                }
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.expression(expr, redact, allow_alias)?;
                self.expression(low, redact, allow_alias)?;
                self.expression(high, redact, allow_alias)?;
            }
            Expr::Like {
                expr,
                pattern,
                any: false,
                escape_char: None,
                ..
            } => {
                self.expression(expr, redact, allow_alias)?;
                self.expression(pattern, redact, allow_alias)?;
            }
            Expr::Cast {
                kind: sqlparser::ast::CastKind::Cast,
                expr,
                data_type,
                format: None,
            } => {
                match data_type {
                    DataType::Char(None)
                    | DataType::Varchar(None)
                    | DataType::Double
                    | DataType::Float(None) => (),
                    DataType::Custom(name, modifiers)
                        if name.0.len() == 1
                            && name.0[0].value.eq_ignore_ascii_case("vecf32")
                            && modifiers.len() == 1
                            && modifiers[0]
                                .parse::<usize>()
                                .is_ok_and(|n| n > 0 && n <= 16384) =>
                    {
                        ()
                    }
                    _ => return Err(MemorySqlError::Unsupported),
                }
                self.expression(expr, redact, allow_alias)?;
            }
            Expr::Function(function) => {
                if function.name.0.len() != 1
                    || function.name.0[0].quote_style.is_some()
                    || !matches!(
                        function.name.0[0].value.to_ascii_lowercase().as_str(),
                        "json_extract"
                            | "json_unquote"
                            | "lower"
                            | "upper"
                            | "length"
                            | "l2_distance"
                            | "cosine_distance"
                            | "inner_product"
                    )
                    || function.uses_odbc_syntax
                    || !matches!(function.parameters, FunctionArguments::None)
                    || function.filter.is_some()
                    || function.null_treatment.is_some()
                    || function.over.is_some()
                    || !function.within_group.is_empty()
                {
                    return Err(MemorySqlError::Unsupported);
                }
                let FunctionArguments::List(args) = &mut function.args else {
                    return Err(MemorySqlError::Unsupported);
                };
                if args.duplicate_treatment.is_some()
                    || !args.clauses.is_empty()
                    || args.args.is_empty()
                    || args.args.len() > 4
                {
                    return Err(MemorySqlError::Unsupported);
                }
                for argument in &mut args.args {
                    let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = argument else {
                        return Err(MemorySqlError::Unsupported);
                    };
                    self.expression(expr, redact, allow_alias)?;
                }
            }
            _ => return Err(MemorySqlError::Unsupported),
        }
        if self.parameters > 32 || self.bindings.len() > 64 {
            return Err(MemorySqlError::Invalid);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_selects_and_vector_sql_preserve_parameters() {
        for (sql, params) in [
            ("SELECT memory_id, content FROM memories WHERE subject_id = ? LIMIT 10", 1),
            ("SELECT memory_id, l2_distance(embedding, CAST(? AS vecf32(1024))) AS distance FROM memories WHERE JSON_UNQUOTE(JSON_EXTRACT(extra_metadata, '$.scope.agent_id')) = ? ORDER BY distance ASC LIMIT 10", 2),
        ] {
            let plan = PreparedMemoryQuery::parse(sql).unwrap();
            assert_eq!(plan.parameter_count, params);
            let rendered = plan.sql_for_view("server_owned_database", "memory_sql_view").unwrap();
            assert!(rendered.contains("`server_owned_database`.`memory_sql_view`"));
            assert!(!rendered.contains("FROM memories"));
        }
    }

    #[test]
    fn unsupported_sql_is_rejected_not_repaired() {
        for sql in [
            "SELECT * FROM memories LIMIT 10",
            "SELECT content FROM memories",
            "SELECT content FROM memories LIMIT 101",
            "SELECT content FROM memories LIMIT 10 OFFSET 1",
            "SELECT content FROM memories LIMIT ?",
            "SELECT content FROM other.memories LIMIT 10",
            "SELECT content FROM mem_memories LIMIT 10",
            "SELECT content FROM memories; DELETE FROM memories",
            "DELETE FROM memories",
            "SELECT content INTO OUTFILE '/tmp/leak' FROM memories LIMIT 10",
            "SELECT content FROM memories FOR UPDATE",
            "WITH data AS (SELECT content FROM memories) SELECT content FROM data LIMIT 10",
            "SELECT (SELECT secret FROM auth) FROM memories LIMIT 10",
            "SELECT content FROM memories UNION SELECT content FROM memories LIMIT 10",
            "SELECT a.content FROM memories a JOIN memories b ON 1=1 LIMIT 10",
            "SELECT SLEEP(5) FROM memories LIMIT 10",
            "SELECT LOAD_FILE('/tmp/leak') FROM memories LIMIT 10",
            "SELECT @@version FROM memories LIMIT 10",
            "SELECT @x := content FROM memories LIMIT 10",
            "SELECT mo_ctl('dn','flush','x') FROM memories LIMIT 10",
            "SELECT content FROM memories /*! INTO OUTFILE '/tmp/leak' */ LIMIT 10",
        ] {
            let outcome = PreparedMemoryQuery::parse(sql);
            assert!(outcome.is_err(), "unexpected allowed query: {sql}");
        }
    }

    #[test]
    fn audit_fingerprint_does_not_depend_on_literal_values() {
        let first = PreparedMemoryQuery::parse(
            "SELECT content FROM memories WHERE subject_id = 'synthetic-a' LIMIT 10",
        )
        .unwrap();
        let second = PreparedMemoryQuery::parse(
            "SELECT content FROM memories WHERE subject_id = 'synthetic-b' LIMIT 10",
        )
        .unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn executable_sql_never_contains_caller_string_literals() {
        let plan = PreparedMemoryQuery::parse("SELECT content FROM memories WHERE content = 'synthetic''quote\\\\payload' AND subject_id = ? LIMIT 10").unwrap();
        let sql = plan.sql_for_view("server_db", "server_view").unwrap();
        assert!(!sql.contains("synthetic") && !sql.contains("payload"));
        let values = plan
            .bound_parameters(&[serde_json::json!("consumer")])
            .unwrap();
        assert_eq!(values.len(), 2);
        assert!(values[0].as_str().unwrap().starts_with("synthetic'quote"));
        assert_eq!(values[1], "consumer");
    }
}
