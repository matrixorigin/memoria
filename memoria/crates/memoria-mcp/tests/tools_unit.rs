/// MCP tools unit tests — verifies JSON-RPC dispatch and tool responses.
use async_trait::async_trait;
use memoria_core::{
    interfaces::{EmbeddingProvider, MemoryStore},
    MemoriaError, Memory,
};
use memoria_service::MemoryService;
use serde_json::json;
use std::sync::{Arc, Mutex};

// Reuse same mock as service tests
#[derive(Default)]
struct MockStore {
    memories: Mutex<Vec<Memory>>,
}

#[async_trait]
impl MemoryStore for MockStore {
    async fn insert(&self, m: &Memory) -> Result<(), MemoriaError> {
        self.memories.lock().unwrap().push(m.clone());
        Ok(())
    }
    async fn get(&self, id: &str) -> Result<Option<Memory>, MemoriaError> {
        Ok(self
            .memories
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.memory_id == id && m.is_active)
            .cloned())
    }
    async fn get_including_inactive(&self, id: &str) -> Result<Option<Memory>, MemoriaError> {
        Ok(self
            .memories
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.memory_id == id)
            .cloned())
    }
    async fn update(&self, memory: &Memory) -> Result<(), MemoriaError> {
        let mut s = self.memories.lock().unwrap();
        if let Some(m) = s.iter_mut().find(|m| m.memory_id == memory.memory_id) {
            *m = memory.clone();
        }
        Ok(())
    }
    async fn soft_delete(&self, id: &str) -> Result<(), MemoriaError> {
        let mut s = self.memories.lock().unwrap();
        if let Some(m) = s.iter_mut().find(|m| m.memory_id == id) {
            m.is_active = false;
        }
        Ok(())
    }
    async fn list_active(&self, user_id: &str, limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        Ok(self
            .memories
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.user_id == user_id && m.is_active)
            .take(limit as usize)
            .cloned()
            .collect())
    }
    async fn search_fulltext(
        &self,
        user_id: &str,
        q: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        Ok(self
            .memories
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.user_id == user_id && m.is_active && m.content.contains(q))
            .take(limit as usize)
            .cloned()
            .collect())
    }
    async fn search_vector(&self, _: &str, _: &[f32], _: i64) -> Result<Vec<Memory>, MemoriaError> {
        Ok(vec![])
    }
}

struct MockEmbedder;
#[async_trait]
impl EmbeddingProvider for MockEmbedder {
    async fn embed(&self, _: &str) -> Result<Vec<f32>, MemoriaError> {
        Ok(vec![0.1; 4])
    }
    fn dimension(&self) -> usize {
        4
    }
}

fn make_service() -> Arc<MemoryService> {
    Arc::new(MemoryService::new(
        Arc::new(MockStore::default()),
        Some(Arc::new(MockEmbedder)),
        None,
    ))
}

#[tokio::test]
async fn test_tools_list() {
    let tools = memoria_mcp::tools::list();
    let arr = tools.as_array().unwrap();
    assert_eq!(arr.len(), 13);
    let names: Vec<&str> = arr.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"memory_store"));
    assert!(names.contains(&"memory_retrieve"));
    assert!(names.contains(&"memory_correct"));
    assert!(names.contains(&"memory_purge"));
    assert!(names.contains(&"memory_governance"));
    assert!(names.contains(&"memory_feedback"));
    assert!(names.contains(&"memory_observe"));
    // 5 tools hidden from listing: rebuild_index, get_retrieval_params, tune_params, extract_entities, link_entities
    assert!(!names.contains(&"memory_rebuild_index"));
    assert!(!names.contains(&"memory_get_retrieval_params"));
    assert!(!names.contains(&"memory_tune_params"));
    assert!(!names.contains(&"memory_extract_entities"));
    assert!(!names.contains(&"memory_link_entities"));
    let store = arr
        .iter()
        .find(|t| t["name"].as_str() == Some("memory_store"))
        .expect("memory_store tool");
    assert!(
        store["description"]
            .as_str()
            .unwrap_or("")
            .contains("prefer T3 if unsure"),
        "memory_store description should expose tier choice guidance"
    );
    let trust_tier = &store["inputSchema"]["properties"]["trust_tier"];
    assert_eq!(
        trust_tier["enum"].as_array().map(|v| v.len()),
        Some(4),
        "trust_tier should expose the four exact enum values"
    );
    assert!(
        trust_tier["description"]
            .as_str()
            .unwrap_or("")
            .contains("Use exact values T1/T2/T3/T4"),
        "trust_tier description should explain exact token usage"
    );
    println!("✅ tools_list: 13 tools (5 hidden)");
}

#[tokio::test]
async fn test_tool_memory_store() {
    let svc = make_service();
    let result = memoria_mcp::tools::call(
        "memory_store",
        json!({"content": "test memory", "memory_type": "semantic"}),
        &svc,
        "u1",
    )
    .await
    .unwrap();
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Stored memory"));
    println!("✅ tool memory_store: {text}");
}

#[tokio::test]
async fn test_tool_memory_store_rejects_empty_content() {
    let svc = make_service();
    for args in [json!({}), json!({"content": ""}), json!({"content": "   "})] {
        let result = memoria_mcp::tools::call("memory_store", args, &svc, "u1")
            .await
            .unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("content is required"),
            "unexpected response: {text}"
        );
    }
    assert!(
        svc.list_active("u1", 10).await.unwrap().is_empty(),
        "empty content must not create memories"
    );
    println!("✅ tool memory_store rejects empty content");
}

#[tokio::test]
async fn test_tool_memory_retrieve_empty() {
    let svc = make_service();
    let result = memoria_mcp::tools::call(
        "memory_retrieve",
        json!({"query": "nothing here"}),
        &svc,
        "u1",
    )
    .await
    .unwrap();
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("No relevant memories"));
    println!("✅ tool memory_retrieve empty: {text}");
}

#[tokio::test]
async fn test_tool_memory_retrieve_rejects_missing_query() {
    let svc = make_service();

    for (tool, args) in [
        ("memory_retrieve", json!({})),
        ("memory_retrieve", json!({"query": ""})),
        ("memory_retrieve", json!({"query": "   "})),
        ("memory_search", json!({})),
        ("memory_search", json!({"query": ""})),
        ("memory_search", json!({"query": "   "})),
    ] {
        let result = memoria_mcp::tools::call(tool, args, &svc, "u1")
            .await
            .unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("query is required"),
            "{tool} unexpected response: {text}"
        );
    }
    println!("✅ tool memory_retrieve/search reject missing query");
}

#[tokio::test]
async fn test_tool_memory_retrieve_finds() {
    let svc = make_service();
    // Store first
    memoria_mcp::tools::call(
        "memory_store",
        json!({"content": "rust programming"}),
        &svc,
        "u1",
    )
    .await
    .unwrap();
    // Retrieve
    let result = memoria_mcp::tools::call("memory_retrieve", json!({"query": "rust"}), &svc, "u1")
        .await
        .unwrap();
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("rust programming"));
    println!("✅ tool memory_retrieve finds: {text}");
}

#[tokio::test]
async fn test_tool_memory_correct() {
    let svc = make_service();
    let stored = memoria_mcp::tools::call("memory_store", json!({"content": "old"}), &svc, "u1")
        .await
        .unwrap();
    let text = stored["content"][0]["text"].as_str().unwrap();
    // Extract memory_id from "Stored memory <id>: old"
    let memory_id = text
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':');

    let result = memoria_mcp::tools::call(
        "memory_correct",
        json!({"memory_id": memory_id, "new_content": "new content"}),
        &svc,
        "u1",
    )
    .await
    .unwrap();
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("new content"));
    println!("✅ tool memory_correct: {text}");
}

#[tokio::test]
async fn test_tool_memory_purge() {
    let svc = make_service();
    let stored =
        memoria_mcp::tools::call("memory_store", json!({"content": "to delete"}), &svc, "u1")
            .await
            .unwrap();
    let text = stored["content"][0]["text"].as_str().unwrap();
    let memory_id = text
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':');

    let result =
        memoria_mcp::tools::call("memory_purge", json!({"memory_id": memory_id}), &svc, "u1")
            .await
            .unwrap();
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Purged"));
    println!("✅ tool memory_purge: {text}");
}

#[tokio::test]
async fn test_tool_unknown() {
    let svc = make_service();
    let result = memoria_mcp::tools::call("nonexistent_tool", json!({}), &svc, "u1").await;
    assert!(result.is_err());
    println!("✅ tool unknown returns error");
}
