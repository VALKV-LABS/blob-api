use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::Response,
    Json,
};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::auth_from_headers,
    db::{self, Object},
    error::{Result, StorageError},
    routes::AppState,
    signed,
};

// ── MIME pattern matching ─────────────────────────────────────────────────────
// Supports exact types ("image/png"), type wildcards ("image/*"), and the
// global wildcard ("*" or "*/*"). Content-type parameters (e.g. "; charset=utf-8")
// are stripped before comparison. Comparison is case-insensitive.
pub(crate) fn mime_matches(pattern: &str, content_type: &str) -> bool {
    let ct = content_type.split(';').next().unwrap_or(content_type).trim();
    match pattern {
        "*" | "*/*" => true,
        p if p.ends_with("/*") => {
            let prefix = &p[..p.len() - 2];
            ct.len() > prefix.len() + 1
                && ct[..prefix.len()].eq_ignore_ascii_case(prefix)
                && ct.as_bytes()[prefix.len()] == b'/'
        }
        p => ct.eq_ignore_ascii_case(p),
    }
}

// ── Header helpers ────────────────────────────────────────────────────────────

fn insert_s3_headers(headers: &mut axum::http::HeaderMap, etag: Option<&str>, last_modified: Option<&str>) {
    if let Some(v) = etag.and_then(|e| HeaderValue::from_str(e).ok()) {
        headers.insert(HeaderName::from_static("etag"), v);
    }
    if let Some(v) = last_modified.and_then(|lm| HeaderValue::from_str(lm).ok()) {
        headers.insert(HeaderName::from_static("last-modified"), v);
    }
}

// ── Sort / search helpers ─────────────────────────────────────────────────────

/// Build a LIKE pattern for prefix + substring search.
/// Escapes `%` and `_` in the user-supplied search term so they are treated as
/// literals rather than LIKE wildcards.
fn like_search_pattern(prefix: &str, search: &str) -> String {
    let escaped = search.replace('%', r"\%").replace('_', r"\_");
    format!("{}%{}%", prefix, escaped)
}

fn validated_sort<'a>(column: Option<&'a str>, order: Option<&'a str>) -> (&'static str, &'static str) {
    let col = match column.unwrap_or("name") {
        "created_at" => "created_at",
        "updated_at" => "updated_at",
        "last_accessed_at" => "last_accessed_at",
        _ => "name",
    };
    let ord = match order.unwrap_or("asc") {
        o if o.eq_ignore_ascii_case("desc") => "DESC",
        _ => "ASC",
    };
    (col, ord)
}

// ── Stream helpers ────────────────────────────────────────────────────────────

async fn stream_object(
    state: &AppState,
    bucket_id: &str,
    path: &str,
    cache_control: Option<&str>,
) -> Result<Response> {
    let out = state.s3.get(bucket_id, path).await?;

    let content_type = out
        .content_type
        .unwrap_or_else(|| "application/octet-stream".into());

    let etag = out.e_tag.clone();
    let last_modified = out.last_modified.and_then(|dt| {
        chrono::DateTime::<chrono::Utc>::from_timestamp(dt.secs(), dt.subsec_nanos())
            .map(|d| d.format("%a, %d %b %Y %H:%M:%S GMT").to_string())
    });

    let bytes = out
        .body
        .collect()
        .await
        .map_err(|e| StorageError::S3(e.to_string()))?
        .into_bytes();

    let _ = sqlx::query(
        "UPDATE storage.objects SET last_accessed_at = now() WHERE bucket_id = $1 AND name = $2",
    )
    .bind(bucket_id)
    .bind(path)
    .execute(&state.db)
    .await;

    let len = bytes.len();
    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut().insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_str(&content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    resp.headers_mut().insert(
        HeaderName::from_static("content-length"),
        HeaderValue::from_str(&len.to_string()).unwrap(),
    );
    resp.headers_mut().insert(
        HeaderName::from_static("cache-control"),
        cache_control
            .and_then(|cc| HeaderValue::from_str(cc).ok())
            .unwrap_or_else(|| HeaderValue::from_static("no-cache")),
    );
    insert_s3_headers(resp.headers_mut(), etag.as_deref(), last_modified.as_deref());
    Ok(resp)
}

// ── Upload (POST = fail-if-exists, PUT = upsert) ──────────────────────────────

async fn upload_inner(
    state: &AppState,
    owner: Option<uuid::Uuid>,
    content_type: &str,
    bucket_id: &str,
    path: &str,
    upsert: bool,
    body: Bytes,
) -> Result<Json<Value>> {
    if body.len() as u64 > state.config.file_size_limit {
        return Err(StorageError::PayloadTooLarge);
    }

    let b = db::require_bucket(&state.db, bucket_id).await?;

    if let Some(limit) = b.file_size_limit {
        if body.len() as u64 > limit as u64 {
            return Err(StorageError::PayloadTooLarge);
        }
    }

    if let Some(ref allowed) = b.allowed_mime_types {
        if !allowed.is_empty() && !allowed.iter().any(|p| mime_matches(p, content_type)) {
            return Err(StorageError::BadRequest(format!(
                "content type '{}' is not allowed for this bucket",
                content_type
            )));
        }
    }

    if let Some(quota) = state.config.storage_quota_bytes {
        let used = db::used_storage_bytes(&state.db).await?;
        if used as u64 + body.len() as u64 > quota {
            return Err(StorageError::PayloadTooLarge);
        }
    }

    if !upsert {
        let existing: Option<(uuid::Uuid,)> =
            sqlx::query_as("SELECT id FROM storage.objects WHERE bucket_id = $1 AND name = $2")
                .bind(bucket_id)
                .bind(path)
                .fetch_optional(&state.db)
                .await?;
        if existing.is_some() {
            return Err(StorageError::Conflict("The resource already exists".into()));
        }
    }

    let body_len = body.len();
    state.s3.put(bucket_id, path, content_type, body).await?;

    let (id,): (uuid::Uuid,) = sqlx::query_as(
        r#"INSERT INTO storage.objects (bucket_id, name, owner, metadata)
           VALUES ($1, $2, $3, $4)
           ON CONFLICT (bucket_id, name) DO UPDATE
             SET updated_at = now(),
                 owner = EXCLUDED.owner,
                 metadata = EXCLUDED.metadata
           RETURNING id"#,
    )
    .bind(bucket_id)
    .bind(path)
    .bind(owner)
    .bind(json!({ "mimetype": content_type, "size": body_len }))
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "Id": id.to_string(), "Key": format!("{}/{}", bucket_id, path) })))
}

pub async fn upload(
    State(state): State<AppState>,
    Path((bucket, path)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let claims = auth_from_headers(&headers, &state.config.jwt_secret)?;
    if !claims.is_service_role() && claims.role() == "anon" {
        return Err(StorageError::Unauthorized);
    }
    let ct = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    upload_inner(&state, claims.user_id().and_then(|s| s.parse().ok()), &ct, &bucket, &path, false, body).await
}

pub async fn upsert(
    State(state): State<AppState>,
    Path((bucket, path)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    let claims = auth_from_headers(&headers, &state.config.jwt_secret)?;
    if !claims.is_service_role() && claims.role() == "anon" {
        return Err(StorageError::Unauthorized);
    }
    let ct = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    upload_inner(&state, claims.user_id().and_then(|s| s.parse().ok()), &ct, &bucket, &path, true, body).await
}

// ── Signed upload URLs ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SignUploadRequest {
    pub expires_in: Option<u64>,
}

// POST /storage/v1/object/upload/sign/:bucket/*path
pub async fn create_upload_signed_url(
    State(state): State<AppState>,
    Path((bucket, path)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SignUploadRequest>,
) -> Result<Json<Value>> {
    let claims = auth_from_headers(&headers, &state.config.jwt_secret)?;
    if claims.role() == "anon" {
        return Err(StorageError::Unauthorized);
    }
    db::require_bucket(&state.db, &bucket).await?;

    let expiry = body.expires_in.unwrap_or(state.config.signed_url_expiry_secs);
    let token = signed::create_upload_token(&bucket, &path, expiry, &state.config.jwt_secret)?;
    let url = format!(
        "/storage/v1/object/upload/sign/{}/{}?token={}",
        bucket, path, token
    );

    Ok(Json(json!({ "url": url, "token": token, "path": path })))
}

#[derive(Deserialize)]
pub struct TokenQuery {
    pub token: String,
}

// PUT /storage/v1/object/upload/sign/:bucket/*path?token=<jwt>
pub async fn upload_via_signed_url(
    State(state): State<AppState>,
    Path((bucket, path)): Path<(String, String)>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>> {
    signed::verify_upload_token(&q.token, &bucket, &path, &state.config.jwt_secret)?;

    let ct = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    // No owner — the token is the auth mechanism; the object is unowned.
    upload_inner(&state, None, &ct, &bucket, &path, true, body).await
}

// ── Download ──────────────────────────────────────────────────────────────────

// GET /storage/v1/object/public/:bucket/*path
pub async fn get_public(
    State(state): State<AppState>,
    Path((bucket, path)): Path<(String, String)>,
) -> Result<Response> {
    let b = db::require_bucket(&state.db, &bucket).await?;
    if !b.public {
        return Err(StorageError::Forbidden);
    }
    stream_object(&state, &bucket, &path, b.cache_control.as_deref()).await
}

// GET /storage/v1/object/:bucket/*path
pub async fn get_object(
    State(state): State<AppState>,
    Path((bucket, path)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let claims = auth_from_headers(&headers, &state.config.jwt_secret)?;
    let b = db::require_bucket(&state.db, &bucket).await?;

    if !b.public && !claims.is_service_role() {
        let ok = db::rls_object_exists(
            &state.db,
            claims.role(),
            &claims.to_json_string(),
            &bucket,
            &path,
        )
        .await?;
        if !ok {
            return Err(StorageError::Forbidden);
        }
    }

    stream_object(&state, &bucket, &path, b.cache_control.as_deref()).await
}

// GET /storage/v1/object/sign/:bucket/*path?token=…
pub async fn get_signed(
    State(state): State<AppState>,
    Path((bucket, path)): Path<(String, String)>,
    Query(q): Query<TokenQuery>,
) -> Result<Response> {
    signed::verify_token(&q.token, &bucket, &path, &state.config.jwt_secret)?;
    let b = db::require_bucket(&state.db, &bucket).await?;
    stream_object(&state, &bucket, &path, b.cache_control.as_deref()).await
}

// ── HEAD (metadata only, no body) ─────────────────────────────────────────────

fn head_from_object(
    obj: &Object,
    cache_control: Option<&str>,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Response {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::OK;

    let content_type = obj
        .metadata
        .as_ref()
        .and_then(|m| m.get("mimetype"))
        .and_then(|v| v.as_str())
        .unwrap_or("application/octet-stream");
    resp.headers_mut().insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_str(content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );

    if let Some(size) = obj
        .metadata
        .as_ref()
        .and_then(|m| m.get("size"))
        .and_then(|v| v.as_i64())
    {
        resp.headers_mut().insert(
            HeaderName::from_static("content-length"),
            HeaderValue::from_str(&size.to_string()).unwrap(),
        );
    }

    resp.headers_mut().insert(
        HeaderName::from_static("cache-control"),
        cache_control
            .and_then(|cc| HeaderValue::from_str(cc).ok())
            .unwrap_or_else(|| HeaderValue::from_static("no-cache")),
    );
    insert_s3_headers(resp.headers_mut(), etag, last_modified);
    resp
}

// HEAD /storage/v1/object/public/:bucket/*path
pub async fn head_public(
    State(state): State<AppState>,
    Path((bucket, path)): Path<(String, String)>,
) -> Result<Response> {
    let b = db::require_bucket(&state.db, &bucket).await?;
    if !b.public {
        return Err(StorageError::Forbidden);
    }
    let obj = db::get_object_metadata(&state.db, &bucket, &path)
        .await?
        .ok_or(StorageError::NotFound)?;
    let (etag, last_modified) = state.s3.head_meta(&bucket, &path).await.unwrap_or_default();
    Ok(head_from_object(&obj, b.cache_control.as_deref(), etag.as_deref(), last_modified.as_deref()))
}

// HEAD /storage/v1/object/:bucket/*path
pub async fn head_object(
    State(state): State<AppState>,
    Path((bucket, path)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let claims = auth_from_headers(&headers, &state.config.jwt_secret)?;
    let b = db::require_bucket(&state.db, &bucket).await?;

    if !b.public && !claims.is_service_role() {
        let ok = db::rls_object_exists(
            &state.db,
            claims.role(),
            &claims.to_json_string(),
            &bucket,
            &path,
        )
        .await?;
        if !ok {
            return Err(StorageError::Forbidden);
        }
    }

    let obj = db::get_object_metadata(&state.db, &bucket, &path)
        .await?
        .ok_or(StorageError::NotFound)?;
    let (etag, last_modified) = state.s3.head_meta(&bucket, &path).await.unwrap_or_default();
    Ok(head_from_object(&obj, b.cache_control.as_deref(), etag.as_deref(), last_modified.as_deref()))
}

// ── Create signed URL(s) ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SignRequest {
    pub expires_in: Option<u64>,
}

// POST /storage/v1/object/sign/:bucket/*path  — single signed download URL
pub async fn create_signed_url(
    State(state): State<AppState>,
    Path((bucket, path)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SignRequest>,
) -> Result<Json<Value>> {
    auth_from_headers(&headers, &state.config.jwt_secret)?;
    db::require_bucket(&state.db, &bucket).await?;

    let expiry = body.expires_in.unwrap_or(state.config.signed_url_expiry_secs);
    let token = signed::create_token(&bucket, &path, expiry, &state.config.jwt_secret)?;
    let signed_url = format!("/storage/v1/object/sign/{}/{}?token={}", bucket, path, token);

    Ok(Json(json!({ "signedURL": signed_url, "token": token, "path": path })))
}

#[derive(Deserialize)]
pub struct MultiSignRequest {
    pub paths: Vec<String>,
    pub expires_in: Option<u64>,
}

// POST /storage/v1/object/sign/:bucket  — multiple signed download URLs in one call
pub async fn create_multi_signed_urls(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    headers: HeaderMap,
    Json(body): Json<MultiSignRequest>,
) -> Result<Json<Value>> {
    auth_from_headers(&headers, &state.config.jwt_secret)?;
    db::require_bucket(&state.db, &bucket).await?;

    let expiry = body.expires_in.unwrap_or(state.config.signed_url_expiry_secs);
    let results: Vec<Value> = body
        .paths
        .iter()
        .map(|path| {
            let token = signed::create_token(&bucket, path, expiry, &state.config.jwt_secret)?;
            let signed_url =
                format!("/storage/v1/object/sign/{}/{}?token={}", bucket, path, token);
            Ok(json!({ "path": path, "signedURL": signed_url, "token": token }))
        })
        .collect::<Result<_>>()?;

    Ok(Json(json!(results)))
}

// ── Delete ────────────────────────────────────────────────────────────────────

// DELETE /storage/v1/object/:bucket/*path  — single object
pub async fn remove(
    State(state): State<AppState>,
    Path((bucket, path)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let claims = auth_from_headers(&headers, &state.config.jwt_secret)?;
    if !claims.is_service_role() && claims.role() == "anon" {
        return Err(StorageError::Unauthorized);
    }

    db::require_bucket(&state.db, &bucket).await?;

    let deleted = sqlx::query(
        "DELETE FROM storage.objects WHERE bucket_id = $1 AND name = $2 RETURNING id",
    )
    .bind(&bucket)
    .bind(&path)
    .fetch_optional(&state.db)
    .await?;

    if deleted.is_none() {
        return Err(StorageError::NotFound);
    }

    state.s3.delete(&bucket, &path).await?;

    Ok(Json(json!([{ "name": path }])))
}

#[derive(Deserialize)]
pub struct BulkDeleteRequest {
    /// Exact object paths to delete (not prefix-matched).
    pub prefixes: Vec<String>,
}

// DELETE /storage/v1/object/:bucket  — bulk delete by exact path list
pub async fn bulk_delete(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    headers: HeaderMap,
    Json(body): Json<BulkDeleteRequest>,
) -> Result<Json<Value>> {
    let claims = auth_from_headers(&headers, &state.config.jwt_secret)?;
    if !claims.is_service_role() && claims.role() == "anon" {
        return Err(StorageError::Unauthorized);
    }

    db::require_bucket(&state.db, &bucket).await?;

    if body.prefixes.is_empty() {
        return Ok(Json(json!([])));
    }

    let deleted_names = db::bulk_delete_objects(&state.db, &bucket, &body.prefixes).await?;

    // Delete from S3 (best-effort — DB is already updated).
    let pairs: Vec<(String, String)> = deleted_names
        .iter()
        .map(|n| (bucket.clone(), n.clone()))
        .collect();
    let _ = state.s3.delete_many(&pairs).await;

    let result: Vec<Value> = deleted_names
        .iter()
        .map(|n| json!({ "name": n }))
        .collect();

    Ok(Json(json!(result)))
}

// ── Copy / Move ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CopyRequest {
    #[serde(rename = "bucketId")]
    pub bucket_id: String,
    #[serde(rename = "sourceKey")]
    pub source_key: String,
    #[serde(rename = "destinationBucket")]
    pub destination_bucket: String,
    #[serde(rename = "destinationKey")]
    pub destination_key: String,
}

// POST /storage/v1/object/copy
pub async fn copy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CopyRequest>,
) -> Result<Json<Value>> {
    let claims = auth_from_headers(&headers, &state.config.jwt_secret)?;
    if !claims.is_service_role() {
        return Err(StorageError::Forbidden);
    }

    db::require_bucket(&state.db, &body.bucket_id).await?;
    db::require_bucket(&state.db, &body.destination_bucket).await?;

    // Verify source exists before touching S3 so a missing object returns 404.
    db::get_object_metadata(&state.db, &body.bucket_id, &body.source_key)
        .await?
        .ok_or(StorageError::NotFound)?;

    state
        .s3
        .copy(
            &body.bucket_id,
            &body.source_key,
            &body.destination_bucket,
            &body.destination_key,
        )
        .await?;

    let id = db::copy_object_metadata(
        &state.db,
        &body.bucket_id,
        &body.source_key,
        &body.destination_bucket,
        &body.destination_key,
        claims.user_id().and_then(|s| s.parse().ok()),
    )
    .await?;

    Ok(Json(json!({
        "Id": id.to_string(),
        "Key": format!("{}/{}", body.destination_bucket, body.destination_key),
    })))
}

// POST /storage/v1/object/move
pub async fn move_object(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CopyRequest>,
) -> Result<Json<Value>> {
    let claims = auth_from_headers(&headers, &state.config.jwt_secret)?;
    if !claims.is_service_role() {
        return Err(StorageError::Forbidden);
    }

    db::require_bucket(&state.db, &body.bucket_id).await?;
    db::require_bucket(&state.db, &body.destination_bucket).await?;

    // Verify source exists before touching S3 so a missing object returns 404.
    db::get_object_metadata(&state.db, &body.bucket_id, &body.source_key)
        .await?
        .ok_or(StorageError::NotFound)?;

    // S3 copy then delete source.
    state
        .s3
        .copy(
            &body.bucket_id,
            &body.source_key,
            &body.destination_bucket,
            &body.destination_key,
        )
        .await?;

    let id = db::copy_object_metadata(
        &state.db,
        &body.bucket_id,
        &body.source_key,
        &body.destination_bucket,
        &body.destination_key,
        claims.user_id().and_then(|s| s.parse().ok()),
    )
    .await?;

    // Remove source from S3 and DB.
    let _ = state.s3.delete(&body.bucket_id, &body.source_key).await;
    let _ = sqlx::query(
        "DELETE FROM storage.objects WHERE bucket_id = $1 AND name = $2",
    )
    .bind(&body.bucket_id)
    .bind(&body.source_key)
    .execute(&state.db)
    .await;

    Ok(Json(json!({
        "Id": id.to_string(),
        "Key": format!("{}/{}", body.destination_bucket, body.destination_key),
    })))
}

// ── List ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ListRequest {
    pub prefix: Option<String>,
    pub search: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub sort_by: Option<SortBy>,
}

#[derive(Deserialize)]
pub struct SortBy {
    pub column: Option<String>,
    pub order: Option<String>,
}

// POST /storage/v1/object/list/:bucket
pub async fn list(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ListRequest>,
) -> Result<Json<Value>> {
    auth_from_headers(&headers, &state.config.jwt_secret)?;
    db::require_bucket(&state.db, &bucket).await?;

    let prefix = body.prefix.unwrap_or_default();
    let limit = body.limit.unwrap_or(100).min(1000);
    let offset = body.offset.unwrap_or(0);

    let sort = body.sort_by.as_ref();
    let (col, ord) = validated_sort(
        sort.and_then(|s| s.column.as_deref()),
        sort.and_then(|s| s.order.as_deref()),
    );

    let objects: Vec<Object> = if let Some(ref search) = body.search {
        // search is a substring match on name within the prefix
        let pattern = like_search_pattern(&prefix, search);
        sqlx::query_as(&format!(
            "SELECT id, bucket_id, name, owner, created_at, updated_at, last_accessed_at, metadata \
             FROM storage.objects \
             WHERE bucket_id = $1 AND name LIKE $2 ESCAPE '\\' \
             ORDER BY {col} {ord} \
             LIMIT $3 OFFSET $4"
        ))
        .bind(&bucket)
        .bind(&pattern)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as(&format!(
            "SELECT id, bucket_id, name, owner, created_at, updated_at, last_accessed_at, metadata \
             FROM storage.objects \
             WHERE bucket_id = $1 AND name LIKE $2 \
             ORDER BY {col} {ord} \
             LIMIT $3 OFFSET $4"
        ))
        .bind(&bucket)
        .bind(format!("{}%", prefix))
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await?
    };

    Ok(Json(json!(objects)))
}

// ── Image transform (imgproxy proxy) ─────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct TransformParams {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub format: Option<String>,
    pub quality: Option<u32>,
}

async fn transform_and_serve(
    state: &AppState,
    bucket_id: &str,
    path: &str,
    params: &TransformParams,
    cache_control: Option<&str>,
) -> Result<Response> {
    let Some(ref imgproxy_base) = state.config.imgproxy_url else {
        // No imgproxy configured — fall through to raw download.
        return stream_object(state, bucket_id, path, cache_control).await;
    };

    let s3_key = state.s3.object_key(bucket_id, path);
    let source = format!("s3://{}/{}", state.s3.s3_bucket(), s3_key);

    // Build processing options for imgproxy plain URL format:
    // /insecure/{opts}/plain/{source_url}
    let mut opts: Vec<String> = vec![];
    match (params.width, params.height) {
        (Some(w), Some(h)) => opts.push(format!("rs:fit:{}:{}", w, h)),
        (Some(w), None) => opts.push(format!("w:{}", w)),
        (None, Some(h)) => opts.push(format!("h:{}", h)),
        _ => {}
    }
    if let Some(q) = params.quality {
        opts.push(format!("q:{}", q));
    }
    if let Some(ref fmt) = params.format {
        opts.push(format!("f:{}", fmt));
    }

    let opts_path = if opts.is_empty() {
        String::new()
    } else {
        format!("{}/", opts.join("/"))
    };

    let imgproxy_url = format!("{}/insecure/{}plain/{}", imgproxy_base, opts_path, source);

    let client = reqwest::Client::new();
    let resp = client
        .get(&imgproxy_url)
        .send()
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("imgproxy request failed: {}", e)))?;

    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("imgproxy read failed: {}", e)))?;

    if !status.is_success() {
        return Err(StorageError::Internal(anyhow::anyhow!(
            "imgproxy returned {}",
            status
        )));
    }

    let mut axum_resp = Response::new(Body::from(bytes));
    axum_resp.headers_mut().insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_str(&content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    axum_resp.headers_mut().insert(
        HeaderName::from_static("cache-control"),
        cache_control
            .and_then(|cc| HeaderValue::from_str(cc).ok())
            .unwrap_or_else(|| HeaderValue::from_static("no-cache")),
    );
    Ok(axum_resp)
}

// GET /storage/v1/render/image/authenticated/:bucket/*path
pub async fn render_image_authenticated(
    State(state): State<AppState>,
    Path((bucket, path)): Path<(String, String)>,
    headers: HeaderMap,
    Query(params): Query<TransformParams>,
) -> Result<Response> {
    let claims = auth_from_headers(&headers, &state.config.jwt_secret)?;
    let b = db::require_bucket(&state.db, &bucket).await?;

    if !b.public && !claims.is_service_role() {
        let ok = db::rls_object_exists(
            &state.db,
            claims.role(),
            &claims.to_json_string(),
            &bucket,
            &path,
        )
        .await?;
        if !ok {
            return Err(StorageError::Forbidden);
        }
    }

    transform_and_serve(&state, &bucket, &path, &params, b.cache_control.as_deref()).await
}

// GET /storage/v1/render/image/public/:bucket/*path
pub async fn render_image_public(
    State(state): State<AppState>,
    Path((bucket, path)): Path<(String, String)>,
    Query(params): Query<TransformParams>,
) -> Result<Response> {
    let b = db::require_bucket(&state.db, &bucket).await?;
    if !b.public {
        return Err(StorageError::Forbidden);
    }
    transform_and_serve(&state, &bucket, &path, &params, b.cache_control.as_deref()).await
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── mime_matches ──────────────────────────────────────────────────────────

    #[test]
    fn exact_match() {
        assert!(mime_matches("image/png", "image/png"));
        assert!(mime_matches("text/plain", "text/plain"));
        assert!(!mime_matches("image/png", "image/jpeg"));
        assert!(!mime_matches("text/plain", "text/html"));
    }

    #[test]
    fn case_insensitive() {
        assert!(mime_matches("image/png", "IMAGE/PNG"));
        assert!(mime_matches("TEXT/PLAIN", "text/plain"));
    }

    #[test]
    fn strips_parameters() {
        assert!(mime_matches("text/plain", "text/plain; charset=utf-8"));
        assert!(mime_matches("image/*", "image/png; quality=85"));
        assert!(!mime_matches("image/png", "image/jpeg; q=0.9"));
    }

    #[test]
    fn type_wildcard() {
        assert!(mime_matches("image/*", "image/png"));
        assert!(mime_matches("image/*", "image/jpeg"));
        assert!(mime_matches("image/*", "image/webp"));
        assert!(!mime_matches("image/*", "text/plain"));
        assert!(!mime_matches("image/*", "video/mp4"));
        assert!(!mime_matches("image/*", "imagex/png"));
    }

    #[test]
    fn global_wildcard() {
        assert!(mime_matches("*", "anything/goes"));
        assert!(mime_matches("*/*", "text/plain"));
        assert!(mime_matches("*/*", "application/octet-stream"));
    }

    // ── validated_sort ────────────────────────────────────────────────────────

    #[test]
    fn sort_defaults_to_name_asc() {
        assert_eq!(validated_sort(None, None), ("name", "ASC"));
    }

    #[test]
    fn sort_valid_column_and_order() {
        assert_eq!(validated_sort(Some("created_at"), Some("desc")), ("created_at", "DESC"));
        assert_eq!(validated_sort(Some("updated_at"), Some("asc")), ("updated_at", "ASC"));
        assert_eq!(validated_sort(Some("last_accessed_at"), Some("DESC")), ("last_accessed_at", "DESC"));
    }

    #[test]
    fn sort_unknown_column_falls_back_to_name() {
        // SQL injection attempt should fall through to safe default
        assert_eq!(validated_sort(Some("'; DROP TABLE storage.objects; --"), None), ("name", "ASC"));
        assert_eq!(validated_sort(Some("id"), None), ("name", "ASC"));
    }

    #[test]
    fn sort_unknown_order_falls_back_to_asc() {
        assert_eq!(validated_sort(None, Some("sideways")), ("name", "ASC"));
    }

    // ── like_search_pattern ───────────────────────────────────────────────────

    #[test]
    fn search_pattern_basic() {
        assert_eq!(like_search_pattern("", "logo"), "%logo%");
        assert_eq!(like_search_pattern("docs/", "read"), "docs/%read%");
    }

    #[test]
    fn search_pattern_escapes_percent() {
        // A bare % would match anything; it must be treated as a literal.
        assert_eq!(like_search_pattern("", "%"), r"%\%%");
        assert_eq!(like_search_pattern("tmp/", "a%b"), r"tmp/%a\%b%");
    }

    #[test]
    fn search_pattern_escapes_underscore() {
        assert_eq!(like_search_pattern("", "_"), r"%\_%");
        assert_eq!(like_search_pattern("x/", "a_b"), r"x/%a\_b%");
    }

    #[test]
    fn search_pattern_empty_search_matches_all_in_prefix() {
        // empty search → same as prefix-only query
        assert_eq!(like_search_pattern("img/", ""), "img/%%");
    }

    // ── insert_s3_headers ─────────────────────────────────────────────────────

    #[test]
    fn s3_headers_both_set() {
        let mut map = axum::http::HeaderMap::new();
        insert_s3_headers(&mut map, Some(r#""abc123""#), Some("Sat, 13 Jun 2026 04:51:34 GMT"));
        assert_eq!(map.get("etag").unwrap(), r#""abc123""#);
        assert_eq!(map.get("last-modified").unwrap(), "Sat, 13 Jun 2026 04:51:34 GMT");
    }

    #[test]
    fn s3_headers_none_skipped() {
        let mut map = axum::http::HeaderMap::new();
        insert_s3_headers(&mut map, None, None);
        assert!(map.get("etag").is_none());
        assert!(map.get("last-modified").is_none());
    }

    #[test]
    fn s3_headers_partial() {
        let mut map = axum::http::HeaderMap::new();
        insert_s3_headers(&mut map, Some(r#""etag-only""#), None);
        assert_eq!(map.get("etag").unwrap(), r#""etag-only""#);
        assert!(map.get("last-modified").is_none());
    }

    // ── head_from_object ──────────────────────────────────────────────────────

    fn make_object(mimetype: Option<&str>, size: Option<i64>) -> Object {
        use serde_json::{json, Map, Value};
        let metadata = match (mimetype, size) {
            (None, None) => None,
            _ => {
                let mut m = Map::new();
                if let Some(ct) = mimetype {
                    m.insert("mimetype".into(), json!(ct));
                }
                if let Some(s) = size {
                    m.insert("size".into(), json!(s));
                }
                Some(Value::Object(m))
            }
        };
        Object {
            id: uuid::Uuid::new_v4(),
            bucket_id: Some("test-bucket".into()),
            name: Some("test.txt".into()),
            owner: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_accessed_at: chrono::Utc::now(),
            metadata,
        }
    }

    #[test]
    fn head_response_has_all_five_headers() {
        let obj = make_object(Some("text/plain"), Some(42));
        let resp = head_from_object(&obj, Some("max-age=60"), Some(r#""etag1""#), Some("Mon, 01 Jan 2024 00:00:00 GMT"));
        let h = resp.headers();
        assert_eq!(h.get("content-type").unwrap(), "text/plain");
        assert_eq!(h.get("content-length").unwrap(), "42");
        assert_eq!(h.get("cache-control").unwrap(), "max-age=60");
        assert_eq!(h.get("etag").unwrap(), r#""etag1""#);
        assert_eq!(h.get("last-modified").unwrap(), "Mon, 01 Jan 2024 00:00:00 GMT");
    }

    #[test]
    fn head_response_defaults_content_type_when_metadata_absent() {
        // Object with no metadata at all must still emit content-type
        let obj = make_object(None, None);
        let resp = head_from_object(&obj, None, None, None);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/octet-stream"
        );
        assert_eq!(resp.headers().get("cache-control").unwrap(), "no-cache");
        assert!(resp.headers().get("etag").is_none());
        assert!(resp.headers().get("last-modified").is_none());
    }

    #[test]
    fn head_response_no_content_length_when_size_missing() {
        let obj = make_object(Some("image/png"), None);
        let resp = head_from_object(&obj, None, None, None);
        // content-type set, content-length absent
        assert_eq!(resp.headers().get("content-type").unwrap(), "image/png");
        assert!(resp.headers().get("content-length").is_none());
    }

    #[test]
    fn head_response_status_is_200() {
        let obj = make_object(Some("text/plain"), Some(10));
        let resp = head_from_object(&obj, None, None, None);
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
