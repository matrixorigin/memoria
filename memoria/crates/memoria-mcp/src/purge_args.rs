use crate::tool_result::input_error;
use anyhow::Result;
use memoria_core::MemoryType;
use serde_json::Value;
use std::str::FromStr;

pub(crate) struct MemoryPurgeArgs {
    pub(crate) memory_id: Option<String>,
    pub(crate) topic: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) memory_types: Option<Vec<MemoryType>>,
    pub(crate) branch: Option<String>,
}

pub(crate) fn parse_memory_purge_args(args: &Value) -> Result<MemoryPurgeArgs> {
    let memory_id = args["memory_id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let topic = args["topic"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let session_id = args["session_id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let branch = args["branch"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let memory_types = match args.get("memory_types") {
        None | Some(Value::Null) => None,
        Some(Value::Array(values)) => Some(
            values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .ok_or_else(|| input_error("memory_types entries must be strings"))
                        .and_then(|value| MemoryType::from_str(value).map_err(input_error))
                })
                .collect::<Result<Vec<_>>>()?,
        ),
        Some(_) => return Err(input_error("memory_types must be an array of strings")),
    }
    .filter(|types| !types.is_empty());
    if memory_types.is_some() && session_id.is_none() {
        return Err(input_error("memory_types requires session_id"));
    }
    let selector_count = usize::from(memory_id.is_some())
        + usize::from(topic.is_some())
        + usize::from(session_id.is_some());
    if selector_count > 1 {
        return Err(input_error(
            "Provide only one of memory_id, topic, or session_id",
        ));
    }
    Ok(MemoryPurgeArgs {
        memory_id,
        topic,
        session_id,
        memory_types,
        branch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn err(args: Value) -> String {
        match parse_memory_purge_args(&args) {
            Ok(_) => panic!("accepted {args}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn empty_arguments_select_nothing() {
        for args in [json!({}), Value::Null, json!([]), json!("x")] {
            let parsed = parse_memory_purge_args(&args).unwrap();
            assert!(parsed.memory_id.is_none());
            assert!(parsed.topic.is_none());
            assert!(parsed.session_id.is_none());
            assert!(parsed.memory_types.is_none());
            assert!(parsed.branch.is_none());
        }
    }

    #[test]
    fn selectors_are_trimmed_and_blank_values_are_absent() {
        let parsed = parse_memory_purge_args(&json!({
            "memory_id": "  abc  ",
            "topic": "   ",
            "session_id": "",
            "branch": " feature ",
        }))
        .unwrap();
        assert_eq!(parsed.memory_id.as_deref(), Some("abc"));
        assert!(parsed.topic.is_none());
        assert!(parsed.session_id.is_none());
        assert_eq!(parsed.branch.as_deref(), Some("feature"));

        let parsed = parse_memory_purge_args(&json!({"topic": "\tcoffee\n"})).unwrap();
        assert_eq!(parsed.topic.as_deref(), Some("coffee"));
        // Non-string selectors are ignored rather than stringified.
        let parsed = parse_memory_purge_args(&json!({"memory_id": 42, "topic": true})).unwrap();
        assert!(parsed.memory_id.is_none());
        assert!(parsed.topic.is_none());
    }

    #[test]
    fn at_most_one_selector_is_allowed() {
        for args in [
            json!({"memory_id": "a", "topic": "t"}),
            json!({"memory_id": "a", "session_id": "s"}),
            json!({"topic": "t", "session_id": "s"}),
            json!({"memory_id": "a", "topic": "t", "session_id": "s"}),
        ] {
            assert_eq!(
                err(args),
                "Provide only one of memory_id, topic, or session_id"
            );
        }
        // Blank values do not count as a selector.
        let parsed =
            parse_memory_purge_args(&json!({"memory_id": "a", "topic": " ", "session_id": ""}))
                .unwrap();
        assert_eq!(parsed.memory_id.as_deref(), Some("a"));
    }

    #[test]
    fn memory_types_parse_and_require_session() {
        let parsed = parse_memory_purge_args(&json!({
            "session_id": "s1",
            "memory_types": ["working", "tool_result"],
        }))
        .unwrap();
        assert_eq!(parsed.session_id.as_deref(), Some("s1"));
        assert_eq!(
            parsed.memory_types,
            Some(vec![MemoryType::Working, MemoryType::ToolResult])
        );

        assert_eq!(
            err(json!({"memory_types": ["working"]})),
            "memory_types requires session_id"
        );
        assert_eq!(
            err(json!({"topic": "t", "memory_types": ["working"]})),
            "memory_types requires session_id"
        );
        assert_eq!(
            err(json!({"session_id": " ", "memory_types": ["working"]})),
            "memory_types requires session_id"
        );
    }

    #[test]
    fn empty_or_null_memory_types_mean_no_filter() {
        for value in [json!([]), Value::Null] {
            let parsed = parse_memory_purge_args(&json!({"memory_types": value.clone()})).unwrap();
            assert!(parsed.memory_types.is_none(), "{value}");
            let parsed =
                parse_memory_purge_args(&json!({"session_id": "s", "memory_types": value}))
                    .unwrap();
            assert!(parsed.memory_types.is_none());
        }
    }

    #[test]
    fn invalid_memory_types_are_input_errors() {
        assert_eq!(
            err(json!({"session_id": "s", "memory_types": "working"})),
            "memory_types must be an array of strings"
        );
        assert_eq!(
            err(json!({"session_id": "s", "memory_types": {"t": "working"}})),
            "memory_types must be an array of strings"
        );
        assert_eq!(
            err(json!({"session_id": "s", "memory_types": ["working", 1]})),
            "memory_types entries must be strings"
        );
        assert!(err(json!({"session_id": "s", "memory_types": ["Working"]}))
            .contains("Invalid memory type: Working"));
        assert!(err(json!({"session_id": "s", "memory_types": ["bogus"]}))
            .contains("Invalid memory type: bogus"));
        for args in [
            json!({"session_id": "s", "memory_types": 1}),
            json!({"session_id": "s", "memory_types": [null]}),
        ] {
            let error = parse_memory_purge_args(&args).err().unwrap();
            let result = crate::tool_result::execution_error("memory_purge", error);
            assert_eq!(
                crate::tool_result::error_kind(&result),
                Some(crate::tool_result::ErrorKind::Input),
                "{args}"
            );
        }
    }
}
