use memoria_core::{MemoriaError, MemoryFilter, MetadataOperator};
use sqlx::{MySql, QueryBuilder};

/// Add the same bound predicates before candidate ranking in every SQL path.
pub(crate) fn append_memory_filter(
    query: &mut QueryBuilder<'_, MySql>,
    filter: &MemoryFilter,
) -> Result<(), MemoriaError> {
    filter.validate()?;
    for predicate in &filter.metadata {
        let path = format!("$.{}", predicate.path);
        let value = serde_json::to_string(&predicate.value)?;
        match predicate.operator {
            MetadataOperator::Equal => {
                query
                    .push(" AND json_extract(extra_metadata, ")
                    .push_bind(path)
                    .push(") = CAST(")
                    .push_bind(value)
                    .push(" AS JSON)");
            }
            MetadataOperator::Contains => {
                query
                    .push(" AND json_type(json_extract(extra_metadata, ")
                    .push_bind(path.clone())
                    .push(")) = 'ARRAY' AND json_contains(json_extract(extra_metadata, ")
                    .push_bind(path)
                    .push("), CAST(")
                    .push_bind(value)
                    .push(" AS JSON))");
            }
        }
    }
    if let Some(from) = filter.observed_at_gte {
        query
            .push(" AND observed_at >= ")
            .push_bind(from.naive_utc());
    }
    if let Some(to) = filter.observed_at_lte {
        query.push(" AND observed_at <= ").push_bind(to.naive_utc());
    }
    if let Some(confidence) = filter.min_initial_confidence {
        query
            .push(" AND initial_confidence >= ")
            .push_bind(confidence);
    }
    Ok(())
}
