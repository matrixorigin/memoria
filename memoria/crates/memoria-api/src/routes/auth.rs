//! API key management: POST/GET/DELETE /auth/keys, PUT /auth/keys/:id/rotate
//! Master key required for create. Users can list/revoke their own keys.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::{
    auth::{
        is_reserved_group_id, parse_scopes, AuthUser, DEFAULT_API_KEY_SCOPES, SCOPE_IDENTITY_READ,
        SCOPE_KEYS_MANAGE, SCOPE_MEMORY_READ, SCOPE_MEMORY_SQL, SCOPE_MEMORY_WRITE,
    },
    routes::memory::api_err,
    state::AppState,
};

fn auth_pool(state: &AppState) -> Result<&sqlx::MySqlPool, (StatusCode, String)> {
    state
        .auth_pool
        .as_ref()
        .or_else(|| state.service.sql_store.as_ref().map(|s| s.pool()))
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Shared auth store required".to_string(),
            )
        })
}

// ── Key generation ────────────────────────────────────────────────────────────

fn generate_key() -> (String, String, String) {
    use sha2::{Digest, Sha256};
    let raw = format!("sk-{}", uuid::Uuid::new_v4().simple());
    let prefix = raw[..12].to_string();
    let hash = format!("{:x}", Sha256::digest(raw.as_bytes()));
    (raw, hash, prefix)
}

fn normalize_scopes(requested: Option<Vec<String>>) -> Result<Vec<String>, (StatusCode, String)> {
    let requested = requested.unwrap_or_else(|| parse_scopes(DEFAULT_API_KEY_SCOPES));
    let supported = [
        SCOPE_IDENTITY_READ,
        SCOPE_MEMORY_READ,
        SCOPE_MEMORY_WRITE,
        SCOPE_MEMORY_SQL,
        SCOPE_KEYS_MANAGE,
    ];
    if let Some(scope) = requested
        .iter()
        .map(|scope| scope.trim())
        .find(|scope| !supported.contains(scope))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Unsupported API key scope: {scope}"),
        ));
    }

    let scopes: Vec<String> = supported
        .iter()
        .filter(|scope| requested.iter().any(|item| item.trim() == **scope))
        .map(|scope| (*scope).to_string())
        .collect();
    if !scopes.iter().any(|scope| scope == SCOPE_IDENTITY_READ) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("API key scope {SCOPE_IDENTITY_READ} is required"),
        ));
    }
    if scopes.iter().any(|scope| scope == SCOPE_MEMORY_WRITE)
        && !scopes.iter().any(|scope| scope == SCOPE_MEMORY_READ)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("API key scope {SCOPE_MEMORY_WRITE} requires {SCOPE_MEMORY_READ}"),
        ));
    }
    Ok(scopes)
}

// ── Request / Response ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateKeyRequest {
    pub user_id: String,
    pub name: String,
    pub expires_at: Option<String>,
    pub group_id: Option<String>,
    pub scopes: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct KeyResponse {
    pub key_id: String,
    pub user_id: String,
    pub group_id: Option<String>,
    pub name: String,
    pub key_prefix: String,
    pub scopes: Vec<String>,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_key: Option<String>,
}

#[derive(Serialize)]
pub struct WhoAmIScope {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
}

#[derive(Serialize)]
pub struct WhoAmIResponse {
    pub user_id: String,
    pub key_id: Option<String>,
    pub key_prefix: Option<String>,
    pub scope: WhoAmIScope,
    pub granted_scopes: Vec<String>,
    pub api_version: &'static str,
    pub capabilities: [&'static str; 2],
    pub is_active: bool,
    pub is_master: bool,
}

async fn ensure_group_membership(
    pool: &sqlx::MySqlPool,
    user_id: &str,
    group_id: &str,
) -> Result<(), (StatusCode, String)> {
    let cnt: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_groups g \
         JOIN mem_group_members m ON g.group_id = m.group_id \
         WHERE g.group_id = ? AND g.status = 'active' \
         AND m.user_id = ? AND m.is_active = 1",
    )
    .bind(group_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .map_err(api_err)?;
    if cnt == 0 {
        return Err((
            StatusCode::FORBIDDEN,
            format!("User {user_id} is not an active member of group {group_id}"),
        ));
    }
    Ok(())
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /auth/whoami — resolve the authenticated principal and granted scopes.
pub async fn whoami(auth: AuthUser) -> Result<Json<WhoAmIResponse>, (StatusCode, String)> {
    auth.require_scope(SCOPE_IDENTITY_READ)?;
    let (kind, id) = match auth.group_id.clone() {
        Some(group_id) => ("group", group_id),
        None => ("personal", auth.user_id.clone()),
    };
    Ok(Json(WhoAmIResponse {
        user_id: auth.user_id,
        key_id: auth.key_id,
        key_prefix: auth.key_prefix,
        scope: WhoAmIScope { kind, id },
        granted_scopes: auth.scopes,
        api_version: "1",
        capabilities: ["api_key_scopes", "memory_filters_v1"],
        is_active: true,
        is_master: auth.is_master,
    }))
}

/// POST /auth/keys — create API key
///
/// Access: master key can create any key. Group owners can create keys
/// scoped to their own groups (group_id must be set, target user must be a member).
pub async fn create_key(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateKeyRequest>,
) -> Result<(StatusCode, Json<KeyResponse>), (StatusCode, String)> {
    auth.require_scope(SCOPE_KEYS_MANAGE)?;
    let pool = auth_pool(&state)?;
    let scopes = normalize_scopes(req.scopes.clone())?;
    if scopes.iter().any(|scope| scope == SCOPE_MEMORY_SQL) && !auth.has_scope(SCOPE_MEMORY_SQL) {
        return Err((
            StatusCode::FORBIDDEN,
            "The caller cannot grant the memory SQL scope".into(),
        ));
    }
    let scopes_csv = scopes.join(",");

    match req.group_id.as_deref() {
        Some(group_id) => {
            // Group-scoped key: master OR group owner may create
            if !auth.is_master {
                // Verify caller is the group owner
                let owner_row = sqlx::query(
                    "SELECT owner_user_id FROM mem_groups WHERE group_id = ? AND status = 'active' LIMIT 1",
                )
                .bind(group_id)
                .fetch_optional(pool)
                .await
                .map_err(api_err)?
                .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Group not found: {group_id}")))?;

                let owner: String = owner_row.try_get("owner_user_id").map_err(api_err)?;
                if owner != auth.user_id {
                    return Err((
                        StatusCode::FORBIDDEN,
                        "Only the group owner or master can create group keys".into(),
                    ));
                }
            }
            ensure_group_membership(pool, &req.user_id, group_id).await?;
        }
        None => {
            // Personal key: master only
            auth.require_master()?;
            if is_reserved_group_id(&req.user_id) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Personal user_id must not start with the reserved 'grp_' prefix".into(),
                ));
            }
        }
    }

    let (raw_key, key_hash, key_prefix) = generate_key();
    let key_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().naive_utc();

    sqlx::query(
        "INSERT INTO mem_api_keys (key_id, user_id, group_id, name, key_hash, key_prefix, scopes, is_active, created_at, expires_at) \
         VALUES (?,?,?,?,?,?,?,1,?,?)"
    )
    .bind(&key_id).bind(&req.user_id).bind(req.group_id.as_deref()).bind(&req.name)
    .bind(&key_hash).bind(&key_prefix).bind(&scopes_csv).bind(now)
    .bind(req.expires_at.as_deref())
    .execute(pool).await.map_err(api_err)?;

    Ok((
        StatusCode::CREATED,
        Json(KeyResponse {
            key_id,
            user_id: req.user_id,
            group_id: req.group_id,
            name: req.name,
            key_prefix,
            scopes,
            created_at: now.to_string(),
            expires_at: req.expires_at,
            last_used_at: None,
            raw_key: Some(raw_key),
        }),
    ))
}

/// GET /auth/keys — list keys for current user
pub async fn list_keys(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<KeyResponse>>, (StatusCode, String)> {
    auth.require_scope(SCOPE_KEYS_MANAGE)?;
    let user_id = auth.user_id;
    let pool = auth_pool(&state)?;

    let rows = sqlx::query(
        "SELECT key_id, user_id, group_id, name, key_prefix, scopes, created_at, expires_at, last_used_at \
         FROM mem_api_keys WHERE user_id = ? AND is_active = 1 ORDER BY created_at DESC",
    )
    .bind(&user_id)
    .fetch_all(pool)
    .await
    .map_err(api_err)?;

    let keys = rows
        .iter()
        .map(|r| KeyResponse {
            key_id: r.try_get("key_id").unwrap_or_default(),
            user_id: r.try_get("user_id").unwrap_or_default(),
            group_id: r.try_get("group_id").ok(),
            name: r.try_get("name").unwrap_or_default(),
            key_prefix: r.try_get("key_prefix").unwrap_or_default(),
            scopes: parse_scopes(
                &r.try_get::<String, _>("scopes")
                    .unwrap_or_else(|_| DEFAULT_API_KEY_SCOPES.to_string()),
            ),
            created_at: r
                .try_get::<chrono::NaiveDateTime, _>("created_at")
                .map(|d| d.to_string())
                .unwrap_or_default(),
            expires_at: r
                .try_get::<Option<chrono::NaiveDateTime>, _>("expires_at")
                .ok()
                .flatten()
                .map(|d| d.to_string()),
            last_used_at: r
                .try_get::<Option<chrono::NaiveDateTime>, _>("last_used_at")
                .ok()
                .flatten()
                .map(|d| d.to_string()),
            raw_key: None,
        })
        .collect();

    Ok(Json(keys))
}

/// GET /auth/keys/:id — get a single API key by ID
pub async fn get_key(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(key_id): Path<String>,
) -> Result<Json<KeyResponse>, (StatusCode, String)> {
    auth.require_scope(SCOPE_KEYS_MANAGE)?;
    let user_id = auth.user_id;
    let is_master = auth.is_master;
    let pool = auth_pool(&state)?;

    let row = sqlx::query(
        "SELECT key_id, user_id, group_id, name, key_prefix, scopes, created_at, expires_at, last_used_at \
         FROM mem_api_keys WHERE key_id = ? AND is_active = 1",
    )
    .bind(&key_id)
    .fetch_optional(pool)
    .await
    .map_err(api_err)?;

    let r = row.ok_or_else(|| (StatusCode::NOT_FOUND, "Key not found".to_string()))?;
    let owner: String = r.try_get("user_id").unwrap_or_default();
    if !is_master && owner != user_id {
        return Err((StatusCode::FORBIDDEN, "Not your key".to_string()));
    }
    Ok(Json(KeyResponse {
        key_id: r.try_get("key_id").unwrap_or_default(),
        user_id: owner,
        group_id: r.try_get("group_id").ok(),
        name: r.try_get("name").unwrap_or_default(),
        key_prefix: r.try_get("key_prefix").unwrap_or_default(),
        scopes: parse_scopes(
            &r.try_get::<String, _>("scopes")
                .unwrap_or_else(|_| DEFAULT_API_KEY_SCOPES.to_string()),
        ),
        created_at: r
            .try_get::<chrono::NaiveDateTime, _>("created_at")
            .map(|d| d.to_string())
            .unwrap_or_default(),
        expires_at: r
            .try_get::<Option<chrono::NaiveDateTime>, _>("expires_at")
            .ok()
            .flatten()
            .map(|d| d.to_string()),
        last_used_at: r
            .try_get::<Option<chrono::NaiveDateTime>, _>("last_used_at")
            .ok()
            .flatten()
            .map(|d| d.to_string()),
        raw_key: None,
    }))
}

/// PUT /auth/keys/:id/rotate — revoke old key, issue new one
pub async fn rotate_key(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(key_id): Path<String>,
) -> Result<(StatusCode, Json<KeyResponse>), (StatusCode, String)> {
    auth.require_scope(SCOPE_KEYS_MANAGE)?;
    let user_id = auth.user_id;
    let is_master = auth.is_master;
    let pool = auth_pool(&state)?;

    let old = sqlx::query(
        "SELECT user_id, group_id, name, scopes, expires_at, key_hash FROM mem_api_keys WHERE key_id = ? AND is_active = 1",
    )
    .bind(&key_id)
    .fetch_optional(pool)
    .await
    .map_err(api_err)?
    .ok_or_else(|| (StatusCode::NOT_FOUND, "Key not found".to_string()))?;

    let old_user: String = old.try_get("user_id").map_err(api_err)?;
    if old_user != user_id && !is_master {
        return Err((StatusCode::FORBIDDEN, "Not your key".to_string()));
    }

    let name: String = old.try_get("name").map_err(api_err)?;
    let group_id: Option<String> = old.try_get("group_id").ok();
    let expires_at: Option<chrono::NaiveDateTime> = old.try_get("expires_at").ok().flatten();
    let scopes_csv: String = old
        .try_get("scopes")
        .unwrap_or_else(|_| DEFAULT_API_KEY_SCOPES.to_string());
    let scopes = parse_scopes(&scopes_csv);

    // Invalidate cache before DB update
    if let Ok(key_hash) = old.try_get::<String, _>("key_hash") {
        state.api_key_cache.invalidate(&key_hash);
    }

    // Deactivate old
    sqlx::query("UPDATE mem_api_keys SET is_active = 0 WHERE key_id = ?")
        .bind(&key_id)
        .execute(pool)
        .await
        .map_err(api_err)?;

    // Create new
    let (raw_key, key_hash, key_prefix) = generate_key();
    let new_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().naive_utc();

    sqlx::query(
        "INSERT INTO mem_api_keys (key_id, user_id, group_id, name, key_hash, key_prefix, scopes, is_active, created_at, expires_at) \
         VALUES (?,?,?,?,?,?,?,1,?,?)"
    )
    .bind(&new_id).bind(&old_user).bind(group_id.as_deref()).bind(&name)
    .bind(&key_hash).bind(&key_prefix).bind(&scopes_csv).bind(now)
    .bind(expires_at)
    .execute(pool).await.map_err(api_err)?;

    Ok((
        StatusCode::CREATED,
        Json(KeyResponse {
            key_id: new_id,
            user_id: old_user,
            group_id,
            name,
            key_prefix,
            scopes,
            created_at: now.to_string(),
            expires_at: expires_at.map(|d| d.to_string()),
            last_used_at: None,
            raw_key: Some(raw_key),
        }),
    ))
}

/// DELETE /auth/keys/:id — revoke key
///
/// Access: key owner can revoke own key; master can revoke any key;
/// group owner can revoke any key scoped to their group.
pub async fn revoke_key(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(key_id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    auth.require_scope(SCOPE_KEYS_MANAGE)?;
    let user_id = auth.user_id;
    let is_master = auth.is_master;
    let pool = auth_pool(&state)?;

    let row = sqlx::query("SELECT user_id, group_id, key_hash FROM mem_api_keys WHERE key_id = ?")
        .bind(&key_id)
        .fetch_optional(pool)
        .await
        .map_err(api_err)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Key not found".to_string()))?;

    let key_owner: String = row.try_get("user_id").map_err(api_err)?;
    let key_group: Option<String> = row.try_get("group_id").ok().flatten();

    let mut authorized = is_master || key_owner == user_id;
    if !authorized {
        if let Some(gid) = &key_group {
            // Check if caller is the group owner
            let group_owner =
                sqlx::query("SELECT owner_user_id FROM mem_groups WHERE group_id = ? LIMIT 1")
                    .bind(gid)
                    .fetch_optional(pool)
                    .await
                    .map_err(api_err)?
                    .and_then(|r| r.try_get::<String, _>("owner_user_id").ok());
            if group_owner.as_deref() == Some(&user_id) {
                authorized = true;
            }
        }
    }
    if !authorized {
        return Err((StatusCode::FORBIDDEN, "Not your key".to_string()));
    }

    // Invalidate cache before DB update
    if let Ok(key_hash) = row.try_get::<String, _>("key_hash") {
        state.api_key_cache.invalidate(&key_hash);
    }

    sqlx::query("UPDATE mem_api_keys SET is_active = 0 WHERE key_id = ?")
        .bind(&key_id)
        .execute(pool)
        .await
        .map_err(api_err)?;

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_are_canonicalized_and_deduplicated() {
        let scopes = normalize_scopes(Some(vec![
            SCOPE_MEMORY_READ.to_string(),
            SCOPE_IDENTITY_READ.to_string(),
            SCOPE_MEMORY_READ.to_string(),
        ]))
        .unwrap();
        assert_eq!(
            scopes,
            vec![
                SCOPE_IDENTITY_READ.to_string(),
                SCOPE_MEMORY_READ.to_string()
            ]
        );
    }

    #[test]
    fn write_scope_requires_read_scope() {
        let err = normalize_scopes(Some(vec![
            SCOPE_IDENTITY_READ.to_string(),
            SCOPE_MEMORY_WRITE.to_string(),
        ]))
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn identity_scope_is_required() {
        let err = normalize_scopes(Some(vec![SCOPE_MEMORY_READ.to_string()])).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }
}
