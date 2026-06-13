use std::sync::Arc;
use axum::{
    routing::{delete, get, head, post, put},
    Router,
};

use crate::{config::Config, s3::S3Client};

pub mod buckets;
pub mod health;
pub mod lifecycle;
pub mod objects;
pub mod tus;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: sqlx::PgPool,
    pub s3: Arc<S3Client>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        // Health
        .route("/storage/v1/health", get(health::handle))

        // Buckets
        .route("/storage/v1/bucket",     post(buckets::create).get(buckets::list))
        .route("/storage/v1/bucket/:id", get(buckets::get).patch(buckets::update).delete(buckets::remove))

        // Bucket lifecycle rules
        .route("/storage/v1/bucket/:id/lifecycle",           post(lifecycle::create_rule).get(lifecycle::list_rules))
        .route("/storage/v1/bucket/:id/lifecycle/:rule_id",  delete(lifecycle::delete_rule))

        // ── Objects ─────────────────────────────────────────────────────────────
        // Routes are listed most-specific first (literal segments before params).

        // Copy / Move (literal "copy"/"move" beat :bucket param)
        .route("/storage/v1/object/copy",  post(objects::copy))
        .route("/storage/v1/object/move",  post(objects::move_object))

        // Signed download URLs — single path and multi-path
        .route("/storage/v1/object/sign/:bucket/*path",
            post(objects::create_signed_url).get(objects::get_signed))
        .route("/storage/v1/object/sign/:bucket",
            post(objects::create_multi_signed_urls))

        // Signed upload URLs
        .route("/storage/v1/object/upload/sign/:bucket/*path",
            post(objects::create_upload_signed_url).put(objects::upload_via_signed_url))

        // Public access (no auth)
        .route("/storage/v1/object/public/:bucket/*path",
            get(objects::get_public).head(objects::head_public))

        // Object list
        .route("/storage/v1/object/list/:bucket", post(objects::list))

        // Bulk delete (no /*path — different shape from single-object delete)
        .route("/storage/v1/object/:bucket",
            delete(objects::bulk_delete))

        // Single-object CRUD + HEAD
        .route("/storage/v1/object/:bucket/*path",
            post(objects::upload)
                .put(objects::upsert)
                .get(objects::get_object)
                .delete(objects::remove)
                .head(objects::head_object))

        // Image transform (proxied through imgproxy when IMGPROXY_URL is set)
        .route("/storage/v1/render/image/authenticated/:bucket/*path",
            get(objects::render_image_authenticated))
        .route("/storage/v1/render/image/public/:bucket/*path",
            get(objects::render_image_public))

        // TUS resumable upload
        .route("/storage/v1/upload/resumable",
            post(tus::create))
        .route("/storage/v1/upload/resumable/:id",
            head(tus::head).patch(tus::patch_chunk).delete(tus::abort))

        .with_state(state)
}
