use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    auth::auth_from_headers,
    db,
    error::{Result, StorageError},
    routes::AppState,
};

fn require_service_role(headers: &HeaderMap, secret: &str) -> Result<()> {
    let claims = auth_from_headers(headers, secret)?;
    if !claims.is_service_role() {
        return Err(StorageError::Forbidden);
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct CreateRuleRequest {
    /// Optional key prefix — only objects whose name starts with this string
    /// are subject to the rule. Empty string (default) matches all objects.
    pub prefix: Option<String>,
    /// Number of days after which matching objects expire. Must be ≥ 1.
    pub expires_days: i32,
}

// POST /storage/v1/bucket/:id/lifecycle
pub async fn create_rule(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(bucket_id): Path<String>,
    Json(body): Json<CreateRuleRequest>,
) -> Result<Json<Value>> {
    require_service_role(&headers, &state.config.jwt_secret)?;

    if body.expires_days < 1 {
        return Err(StorageError::BadRequest(
            "expires_days must be at least 1".into(),
        ));
    }

    db::require_bucket(&state.db, &bucket_id).await?;

    let rule = db::create_lifecycle_rule(
        &state.db,
        &bucket_id,
        body.prefix.as_deref().unwrap_or(""),
        body.expires_days,
    )
    .await?;

    Ok(Json(json!(rule)))
}

// GET /storage/v1/bucket/:id/lifecycle
pub async fn list_rules(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(bucket_id): Path<String>,
) -> Result<Json<Value>> {
    auth_from_headers(&headers, &state.config.jwt_secret)?;
    db::require_bucket(&state.db, &bucket_id).await?;
    let rules = db::list_lifecycle_rules(&state.db, &bucket_id).await?;
    Ok(Json(json!(rules)))
}

// DELETE /storage/v1/bucket/:id/lifecycle/:rule_id
pub async fn delete_rule(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((bucket_id, rule_id)): Path<(String, String)>,
) -> Result<Json<Value>> {
    require_service_role(&headers, &state.config.jwt_secret)?;

    let rule_uuid: Uuid = rule_id
        .parse()
        .map_err(|_| StorageError::BadRequest("invalid rule id".into()))?;

    let deleted = db::delete_lifecycle_rule(&state.db, &bucket_id, rule_uuid).await?;
    if !deleted {
        return Err(StorageError::NotFound);
    }
    Ok(Json(json!({ "message": "rule deleted" })))
}
