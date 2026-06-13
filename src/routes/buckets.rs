use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::auth_from_headers,
    db::{self, Bucket, BUCKET_COLS},
    error::{Result, StorageError},
    routes::AppState,
};

#[derive(Deserialize)]
pub struct CreateBucketRequest {
    pub id: String,
    pub name: Option<String>,
    pub public: Option<bool>,
    pub file_size_limit: Option<i64>,
    pub allowed_mime_types: Option<Vec<String>>,
    /// Value passed as Cache-Control header on object downloads, e.g. "max-age=3600, public".
    pub cache_control: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateBucketRequest {
    pub public: Option<bool>,
    pub file_size_limit: Option<i64>,
    pub allowed_mime_types: Option<Vec<String>>,
    pub cache_control: Option<String>,
}

fn require_service_role(headers: &HeaderMap, secret: &str) -> Result<()> {
    let claims = auth_from_headers(headers, secret)?;
    if !claims.is_service_role() {
        return Err(StorageError::Forbidden);
    }
    Ok(())
}

// POST /storage/v1/bucket
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateBucketRequest>,
) -> Result<Json<Value>> {
    require_service_role(&headers, &state.config.jwt_secret)?;

    if body.id.is_empty() {
        return Err(StorageError::BadRequest("id is required".into()));
    }
    let name = body.name.as_deref().unwrap_or(&body.id);

    let bucket: Bucket = sqlx::query_as(&format!(
        "INSERT INTO storage.buckets \
           (id, name, public, file_size_limit, allowed_mime_types, cache_control) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         RETURNING {BUCKET_COLS}"
    ))
    .bind(&body.id)
    .bind(name)
    .bind(body.public.unwrap_or(false))
    .bind(body.file_size_limit)
    .bind(body.allowed_mime_types.as_deref())
    .bind(body.cache_control.as_deref())
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        if e.to_string().contains("unique") || e.to_string().contains("duplicate") {
            StorageError::Conflict("Bucket already exists".into())
        } else {
            StorageError::Db(e)
        }
    })?;

    Ok(Json(json!({ "name": bucket.id })))
}

// GET /storage/v1/bucket
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    auth_from_headers(&headers, &state.config.jwt_secret)?;

    let buckets: Vec<Bucket> = sqlx::query_as(&format!(
        "SELECT {BUCKET_COLS} FROM storage.buckets ORDER BY created_at"
    ))
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!(buckets)))
}

// GET /storage/v1/bucket/:id
pub async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    auth_from_headers(&headers, &state.config.jwt_secret)?;
    let bucket = db::require_bucket(&state.db, &id).await?;
    Ok(Json(json!(bucket)))
}

// PATCH /storage/v1/bucket/:id
pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<UpdateBucketRequest>,
) -> Result<Json<Value>> {
    require_service_role(&headers, &state.config.jwt_secret)?;

    let bucket: Option<Bucket> = sqlx::query_as(&format!(
        "UPDATE storage.buckets \
         SET public             = COALESCE($2, public), \
             file_size_limit    = COALESCE($3, file_size_limit), \
             allowed_mime_types = COALESCE($4, allowed_mime_types), \
             cache_control      = COALESCE($5, cache_control), \
             updated_at         = now() \
         WHERE id = $1 \
         RETURNING {BUCKET_COLS}"
    ))
    .bind(&id)
    .bind(body.public)
    .bind(body.file_size_limit)
    .bind(body.allowed_mime_types.as_deref())
    .bind(body.cache_control.as_deref())
    .fetch_optional(&state.db)
    .await?;

    bucket.map(|b| Json(json!(b))).ok_or(StorageError::NotFound)
}

// DELETE /storage/v1/bucket/:id
pub async fn remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    require_service_role(&headers, &state.config.jwt_secret)?;

    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM storage.objects WHERE bucket_id = $1")
            .bind(&id)
            .fetch_one(&state.db)
            .await?;

    if count.0 > 0 {
        return Err(StorageError::Conflict(
            "Bucket must be empty before deletion".into(),
        ));
    }

    let deleted = sqlx::query("DELETE FROM storage.buckets WHERE id = $1 RETURNING id")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?;

    if deleted.is_none() {
        return Err(StorageError::NotFound);
    }

    Ok(Json(json!({ "message": "Successfully deleted" })))
}
