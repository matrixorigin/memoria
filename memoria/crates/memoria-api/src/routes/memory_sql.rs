use crate::{
    auth::{AuthUser, SCOPE_MEMORY_SQL},
    AppState,
};
use axum::{extract::State, http::StatusCode, Json};
use memoria_storage::readonly_sql::{
    MemorySqlError, MemorySqlGateway, MemorySqlResult, PreparedMemoryQuery,
};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySqlRequest {
    pub sql: String,
    pub parameters: Vec<Value>,
    pub branch: String,
}

fn failure(error: MemorySqlError) -> (StatusCode, Json<Value>) {
    let (status, code) = match error {
        MemorySqlError::Disabled => (StatusCode::SERVICE_UNAVAILABLE, "memory_sql_disabled"),
        MemorySqlError::Invalid => (StatusCode::BAD_REQUEST, "memory_sql_invalid"),
        MemorySqlError::Unsupported => (StatusCode::UNPROCESSABLE_ENTITY, "memory_sql_unsupported"),
        MemorySqlError::Forbidden => (StatusCode::FORBIDDEN, "memory_sql_forbidden"),
        MemorySqlError::Busy => (StatusCode::TOO_MANY_REQUESTS, "memory_sql_busy"),
        MemorySqlError::Timeout => (StatusCode::GATEWAY_TIMEOUT, "memory_sql_timeout"),
        MemorySqlError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "memory_sql_unavailable"),
        MemorySqlError::ResultLimit => {
            (StatusCode::UNPROCESSABLE_ENTITY, "memory_sql_result_limit")
        }
    };
    (
        status,
        Json(json!({"code":code,"message":"The memory SQL request could not be completed."})),
    )
}

/// Administrative workspace data access, not an implicit Agent entitlement.
/// Authentication chooses scope; the query cannot choose a database or identity.
pub async fn query(
    State(state): State<AppState>,
    auth: AuthUser,
    body: Result<Json<MemorySqlRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<MemorySqlResult>, (StatusCode, Json<Value>)> {
    let gateway = state
        .memory_sql
        .as_ref()
        .ok_or_else(|| failure(MemorySqlError::Disabled))?;
    if state.master_key.is_empty()
        || auth.is_master
        || auth.key_id.is_none()
        || !auth.has_scope(SCOPE_MEMORY_SQL)
    {
        return Err(failure(MemorySqlError::Forbidden));
    }
    let Json(request) = body.map_err(|_| failure(MemorySqlError::Invalid))?;
    if request.branch != "main" {
        return Err(failure(MemorySqlError::Unsupported));
    }
    let plan = PreparedMemoryQuery::parse(&request.sql).map_err(failure)?;
    MemorySqlGateway::validate_parameters(&plan, &request.parameters).map_err(failure)?;
    let scope = auth.scope_id().to_owned();
    let service = state.service.clone();
    let target_scope = scope.clone();
    let store = async move {
        service
            .user_sql_store(&target_scope)
            .await
            .map_err(|_| MemorySqlError::Unavailable)
    };
    gateway
        .execute(store, scope, plan, request.parameters)
        .await
        .map(Json)
        .map_err(failure)
}
