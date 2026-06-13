use axum::{
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use bytes::Bytes;
use serde_json::json;
use uuid::Uuid;

use crate::{
    auth::auth_from_headers,
    db::MultipartUpload,
    error::{Result, StorageError},
    routes::AppState,
};

const SELECT_ALL: &str =
    "SELECT id, bucket_id, object_name, s3_upload_id, upload_offset, upload_length, content_type, owner, parts \
     FROM storage.multipart_uploads WHERE id = $1";

// POST /storage/v1/upload/resumable
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response> {
    let claims = auth_from_headers(&headers, &state.config.jwt_secret)?;
    if !claims.is_service_role() && claims.role() == "anon" {
        return Err(StorageError::Unauthorized);
    }

    let upload_length: i64 = headers
        .get("upload-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| StorageError::BadRequest("Missing Upload-Length header".into()))?;

    if upload_length as u64 > state.config.file_size_limit {
        return Err(StorageError::PayloadTooLarge);
    }

    let meta = parse_tus_metadata(&headers);
    let bucket_id = meta
        .get("bucketName")
        .or_else(|| meta.get("bucket"))
        .ok_or_else(|| StorageError::BadRequest("Missing bucketName in Upload-Metadata".into()))?
        .clone();
    let object_name = meta
        .get("objectName")
        .or_else(|| meta.get("name"))
        .ok_or_else(|| StorageError::BadRequest("Missing objectName in Upload-Metadata".into()))?
        .clone();
    let content_type = meta
        .get("contentType")
        .cloned()
        .unwrap_or_else(|| "application/octet-stream".into());

    let b = crate::db::require_bucket(&state.db, &bucket_id).await?;

    // Per-bucket file size cap
    if let Some(limit) = b.file_size_limit {
        if upload_length as u64 > limit as u64 {
            return Err(StorageError::PayloadTooLarge);
        }
    }

    // MIME type enforcement — null/empty means "allow all"
    if let Some(ref allowed) = b.allowed_mime_types {
        if !allowed.is_empty() && !allowed.iter().any(|p| crate::routes::objects::mime_matches(p, &content_type)) {
            return Err(StorageError::BadRequest(format!(
                "content type '{}' is not allowed for this bucket",
                content_type
            )));
        }
    }

    // Project-level storage quota — checked at create time against declared upload length
    if let Some(quota) = state.config.storage_quota_bytes {
        let used = crate::db::used_storage_bytes(&state.db).await?;
        if used as u64 + upload_length as u64 > quota {
            return Err(StorageError::PayloadTooLarge);
        }
    }

    let s3_upload_id = state
        .s3
        .create_multipart(&bucket_id, &object_name, &content_type)
        .await?;

    let upload_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO storage.multipart_uploads \
         (id, bucket_id, object_name, s3_upload_id, upload_offset, upload_length, content_type) \
         VALUES ($1, $2, $3, $4, 0, $5, $6)",
    )
    .bind(&upload_id)
    .bind(&bucket_id)
    .bind(&object_name)
    .bind(&s3_upload_id)
    .bind(upload_length)
    .bind(&content_type)
    .execute(&state.db)
    .await?;

    let location = format!("/storage/v1/upload/resumable/{}", upload_id);
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::CREATED;
    resp.headers_mut()
        .insert("location", HeaderValue::from_str(&location).unwrap());
    tus_version_headers(resp.headers_mut());
    Ok(resp)
}

// HEAD /storage/v1/upload/resumable/:id
pub async fn head(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    auth_from_headers(&headers, &state.config.jwt_secret)?;

    let upload: Option<MultipartUpload> =
        sqlx::query_as(SELECT_ALL).bind(&id).fetch_optional(&state.db).await?;
    let upload = upload.ok_or(StorageError::NotFound)?;

    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        "upload-offset",
        HeaderValue::from_str(&upload.upload_offset.to_string()).unwrap(),
    );
    resp.headers_mut().insert(
        "upload-length",
        HeaderValue::from_str(
            &upload.upload_length.unwrap_or(0).to_string(),
        )
        .unwrap(),
    );
    resp.headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    tus_version_headers(resp.headers_mut());
    Ok(resp)
}

// PATCH /storage/v1/upload/resumable/:id
pub async fn patch_chunk(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    auth_from_headers(&headers, &state.config.jwt_secret)?;

    let ct = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ct.starts_with("application/offset+octet-stream") {
        return Err(StorageError::BadRequest(
            "Content-Type must be application/offset+octet-stream".into(),
        ));
    }

    let client_offset: i64 = headers
        .get("upload-offset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| StorageError::BadRequest("Missing Upload-Offset header".into()))?;

    let upload: Option<MultipartUpload> =
        sqlx::query_as(SELECT_ALL).bind(&id).fetch_optional(&state.db).await?;
    let upload = upload.ok_or(StorageError::NotFound)?;

    if client_offset != upload.upload_offset {
        return Err(StorageError::BadRequest(format!(
            "Upload-Offset mismatch: expected {}, got {}",
            upload.upload_offset, client_offset
        )));
    }

    // Part number = number of already-uploaded parts + 1
    let existing_parts = upload.parts.as_array().map(|a| a.len()).unwrap_or(0);
    let part_number = (existing_parts + 1) as i32;
    let chunk_len = body.len() as i64;

    let etag = state
        .s3
        .upload_part(
            &upload.bucket_id,
            &upload.object_name,
            &upload.s3_upload_id,
            part_number,
            body,
        )
        .await?;

    let new_offset = upload.upload_offset + chunk_len;

    let mut parts = upload.parts.clone();
    parts
        .as_array_mut()
        .unwrap()
        .push(json!({ "n": part_number, "e": etag }));

    sqlx::query(
        "UPDATE storage.multipart_uploads SET upload_offset = $2, parts = $3 WHERE id = $1",
    )
    .bind(&id)
    .bind(new_offset)
    .bind(&parts)
    .execute(&state.db)
    .await?;

    let upload_length = upload.upload_length.unwrap_or(i64::MAX);
    if new_offset >= upload_length {
        let completed_parts: Vec<(i32, String)> = parts
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|p| {
                let n = p["n"].as_i64()? as i32;
                let e = p["e"].as_str()?.to_string();
                Some((n, e))
            })
            .collect();

        state
            .s3
            .complete_multipart(
                &upload.bucket_id,
                &upload.object_name,
                &upload.s3_upload_id,
                completed_parts,
            )
            .await?;

        sqlx::query(
            "INSERT INTO storage.objects (bucket_id, name, metadata) VALUES ($1, $2, $3) \
             ON CONFLICT (bucket_id, name) DO UPDATE SET updated_at = now(), metadata = EXCLUDED.metadata",
        )
        .bind(&upload.bucket_id)
        .bind(&upload.object_name)
        .bind(json!({ "mimetype": upload.content_type, "size": new_offset }))
        .execute(&state.db)
        .await?;

        sqlx::query("DELETE FROM storage.multipart_uploads WHERE id = $1")
            .bind(&id)
            .execute(&state.db)
            .await?;
    }

    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::NO_CONTENT;
    resp.headers_mut().insert(
        "upload-offset",
        HeaderValue::from_str(&new_offset.to_string()).unwrap(),
    );
    tus_version_headers(resp.headers_mut());
    Ok(resp)
}

// DELETE /storage/v1/upload/resumable/:id
pub async fn abort(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    auth_from_headers(&headers, &state.config.jwt_secret)?;

    let upload: Option<MultipartUpload> =
        sqlx::query_as(SELECT_ALL).bind(&id).fetch_optional(&state.db).await?;
    let upload = upload.ok_or(StorageError::NotFound)?;

    let _ = state
        .s3
        .abort_multipart(&upload.bucket_id, &upload.object_name, &upload.s3_upload_id)
        .await;

    sqlx::query("DELETE FROM storage.multipart_uploads WHERE id = $1")
        .bind(&id)
        .execute(&state.db)
        .await?;

    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::NO_CONTENT;
    Ok(resp)
}

fn tus_version_headers(h: &mut HeaderMap) {
    h.insert("tus-resumable", HeaderValue::from_static("1.0.0"));
    h.insert("tus-version", HeaderValue::from_static("1.0.0"));
}

fn parse_tus_metadata(headers: &HeaderMap) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some(v) = headers.get("upload-metadata").and_then(|v| v.to_str().ok()) else {
        return map;
    };
    for pair in v.split(',') {
        let mut parts = pair.trim().splitn(2, ' ');
        if let (Some(key), Some(val_b64)) = (parts.next(), parts.next()) {
            if let Ok(bytes) = base64_decode(val_b64.trim()) {
                if let Ok(s) = String::from_utf8(bytes) {
                    map.insert(key.trim().to_string(), s);
                }
            }
        }
    }
    map
}

fn base64_decode(input: &str) -> anyhow::Result<Vec<u8>> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut table = [0u8; 256];
    for (i, &b) in alphabet.iter().enumerate() {
        table[b as usize] = i as u8;
    }
    let input = input.trim_end_matches('=');
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let a = table[bytes[i] as usize] as u32;
        let b = table[bytes[i + 1] as usize] as u32;
        let c = table[bytes[i + 2] as usize] as u32;
        let d = table[bytes[i + 3] as usize] as u32;
        let n = (a << 18) | (b << 12) | (c << 6) | d;
        out.push((n >> 16) as u8);
        out.push((n >> 8) as u8);
        out.push(n as u8);
        i += 4;
    }
    match bytes.len() - i {
        2 => {
            let a = table[bytes[i] as usize] as u32;
            let b = table[bytes[i + 1] as usize] as u32;
            out.push(((a << 18) | (b << 12) >> 16) as u8);
        }
        3 => {
            let a = table[bytes[i] as usize] as u32;
            let b = table[bytes[i + 1] as usize] as u32;
            let c = table[bytes[i + 2] as usize] as u32;
            let n = (a << 18) | (b << 12) | (c << 6);
            out.push((n >> 16) as u8);
            out.push((n >> 8) as u8);
        }
        _ => {}
    }
    Ok(out)
}
