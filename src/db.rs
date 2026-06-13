use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{Result, StorageError};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Bucket {
    pub id: String,
    pub name: String,
    pub owner: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub public: bool,
    pub file_size_limit: Option<i64>,
    pub allowed_mime_types: Option<Vec<String>>,
    pub cache_control: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Object {
    pub id: Uuid,
    pub bucket_id: Option<String>,
    pub name: Option<String>,
    pub owner: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_accessed_at: DateTime<Utc>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MultipartUpload {
    pub id: String,
    pub bucket_id: String,
    pub object_name: String,
    pub s3_upload_id: String,
    pub upload_length: Option<i64>,
    pub upload_offset: i64,
    pub content_type: Option<String>,
    pub owner: Option<Uuid>,
    pub parts: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct LifecycleRule {
    pub id: Uuid,
    pub bucket_id: String,
    pub prefix: String,
    pub expires_days: i32,
    pub created_at: DateTime<Utc>,
}

// ── Schema bootstrap ─────────────────────────────────────────────────────────

pub async fn init_schema(pool: &PgPool) -> anyhow::Result<()> {
    let stmts = [
        "CREATE SCHEMA IF NOT EXISTS storage",
        "CREATE SCHEMA IF NOT EXISTS auth",
        r#"CREATE OR REPLACE FUNCTION auth.uid() RETURNS uuid
             LANGUAGE sql STABLE
             AS $$
               SELECT NULLIF(
                 COALESCE(current_setting('request.jwt.claims', true)::jsonb->>'sub', ''),
                 ''
               )::uuid
             $$"#,
        r#"CREATE OR REPLACE FUNCTION auth.role() RETURNS text
             LANGUAGE sql STABLE
             AS $$
               SELECT COALESCE(
                 current_setting('request.jwt.claims', true)::jsonb->>'role',
                 'anon'
               )::text
             $$"#,
        r#"CREATE OR REPLACE FUNCTION auth.jwt() RETURNS jsonb
             LANGUAGE sql STABLE
             AS $$
               SELECT current_setting('request.jwt.claims', true)::jsonb
             $$"#,
        r#"DO $$ BEGIN
             CREATE ROLE anon NOLOGIN NOINHERIT;
           EXCEPTION WHEN duplicate_object THEN NULL; END $$"#,
        r#"DO $$ BEGIN
             CREATE ROLE authenticated NOLOGIN NOINHERIT;
           EXCEPTION WHEN duplicate_object THEN NULL; END $$"#,
        r#"DO $$ BEGIN
             CREATE ROLE service_role BYPASSRLS NOLOGIN NOINHERIT;
           EXCEPTION WHEN duplicate_object THEN NULL; END $$"#,
        r#"CREATE TABLE IF NOT EXISTS storage.buckets (
             id                 text        PRIMARY KEY,
             name               text        NOT NULL UNIQUE,
             owner              uuid,
             created_at         timestamptz DEFAULT now(),
             updated_at         timestamptz DEFAULT now(),
             public             boolean     DEFAULT false,
             file_size_limit    bigint,
             allowed_mime_types text[],
             cache_control      text
           )"#,
        // Migration-safe: no-op if column already exists
        "ALTER TABLE storage.buckets ADD COLUMN IF NOT EXISTS cache_control text",
        r#"CREATE TABLE IF NOT EXISTS storage.objects (
             id               uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
             bucket_id        text        REFERENCES storage.buckets(id) ON DELETE CASCADE,
             name             text        NOT NULL,
             owner            uuid,
             created_at       timestamptz DEFAULT now(),
             updated_at       timestamptz DEFAULT now(),
             last_accessed_at timestamptz DEFAULT now(),
             metadata         jsonb,
             UNIQUE (bucket_id, name)
           )"#,
        r#"CREATE TABLE IF NOT EXISTS storage.multipart_uploads (
             id            text        PRIMARY KEY,
             bucket_id     text        NOT NULL,
             object_name   text        NOT NULL,
             s3_upload_id  text        NOT NULL,
             upload_length bigint,
             upload_offset bigint      NOT NULL DEFAULT 0,
             content_type  text,
             owner         uuid,
             parts         jsonb       NOT NULL DEFAULT '[]'::jsonb,
             created_at    timestamptz NOT NULL DEFAULT now()
           )"#,
        r#"CREATE TABLE IF NOT EXISTS storage.lifecycle_rules (
             id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
             bucket_id    text        NOT NULL REFERENCES storage.buckets(id) ON DELETE CASCADE,
             prefix       text        NOT NULL DEFAULT '',
             expires_days int         NOT NULL CHECK (expires_days > 0),
             created_at   timestamptz NOT NULL DEFAULT now()
           )"#,
        "GRANT USAGE ON SCHEMA storage TO anon, authenticated, service_role",
        "GRANT ALL ON ALL TABLES IN SCHEMA storage TO service_role",
        "GRANT SELECT ON storage.buckets TO anon, authenticated",
        "GRANT SELECT, INSERT, UPDATE, DELETE ON storage.objects TO authenticated",
        "GRANT SELECT ON storage.objects TO anon",
        r#"DO $$ BEGIN
             GRANT anon, authenticated TO CURRENT_USER;
           EXCEPTION WHEN OTHERS THEN NULL; END $$"#,
        "ALTER TABLE storage.objects ENABLE ROW LEVEL SECURITY",
        r#"DO $$ BEGIN
             CREATE POLICY objects_anon_select ON storage.objects
               FOR SELECT TO anon
               USING (EXISTS (
                 SELECT 1 FROM storage.buckets b
                 WHERE b.id = storage.objects.bucket_id AND b.public = true
               ));
           EXCEPTION WHEN duplicate_object THEN NULL; END $$"#,
        r#"DO $$ BEGIN
             CREATE POLICY objects_auth_select ON storage.objects
               FOR SELECT TO authenticated
               USING (owner = auth.uid()
                      OR EXISTS (
                        SELECT 1 FROM storage.buckets b
                        WHERE b.id = storage.objects.bucket_id AND b.public = true
                      ));
           EXCEPTION WHEN duplicate_object THEN NULL; END $$"#,
        r#"DO $$ BEGIN
             CREATE POLICY objects_auth_insert ON storage.objects
               FOR INSERT TO authenticated
               WITH CHECK (owner = auth.uid());
           EXCEPTION WHEN duplicate_object THEN NULL; END $$"#,
        r#"DO $$ BEGIN
             CREATE POLICY objects_auth_update ON storage.objects
               FOR UPDATE TO authenticated
               USING (owner = auth.uid())
               WITH CHECK (owner = auth.uid());
           EXCEPTION WHEN duplicate_object THEN NULL; END $$"#,
        r#"DO $$ BEGIN
             CREATE POLICY objects_auth_delete ON storage.objects
               FOR DELETE TO authenticated
               USING (owner = auth.uid());
           EXCEPTION WHEN duplicate_object THEN NULL; END $$"#,
    ];

    for stmt in &stmts {
        sqlx::query(stmt).execute(pool).await.map_err(|e| {
            anyhow::anyhow!("schema init failed at '{}...': {}", &stmt[..40.min(stmt.len())], e)
        })?;
    }
    Ok(())
}

// ── Bucket helpers ────────────────────────────────────────────────────────────

pub const BUCKET_COLS: &str =
    "id, name, owner, created_at, updated_at, public, file_size_limit, allowed_mime_types, cache_control";

pub async fn get_bucket(pool: &PgPool, bucket_id: &str) -> Result<Option<Bucket>> {
    sqlx::query_as(&format!(
        "SELECT {BUCKET_COLS} FROM storage.buckets WHERE id = $1"
    ))
    .bind(bucket_id)
    .fetch_optional(pool)
    .await
    .map_err(StorageError::Db)
}

pub async fn require_bucket(pool: &PgPool, bucket_id: &str) -> Result<Bucket> {
    get_bucket(pool, bucket_id).await?.ok_or(StorageError::NotFound)
}

// ── Object helpers ────────────────────────────────────────────────────────────

pub async fn get_object_metadata(pool: &PgPool, bucket_id: &str, name: &str) -> Result<Option<Object>> {
    sqlx::query_as(
        "SELECT id, bucket_id, name, owner, created_at, updated_at, last_accessed_at, metadata \
         FROM storage.objects WHERE bucket_id = $1 AND name = $2",
    )
    .bind(bucket_id)
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(StorageError::Db)
}

// Delete objects by exact name list; returns names that were actually deleted.
pub async fn bulk_delete_objects(pool: &PgPool, bucket_id: &str, names: &[String]) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "DELETE FROM storage.objects WHERE bucket_id = $1 AND name = ANY($2) RETURNING name",
    )
    .bind(bucket_id)
    .bind(names)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

// Insert destination metadata after an S3 copy/move; returns the new object id.
pub async fn copy_object_metadata(
    pool: &PgPool,
    src_bucket: &str,
    src_name: &str,
    dst_bucket: &str,
    dst_name: &str,
    new_owner: Option<Uuid>,
) -> Result<Uuid> {
    let src = get_object_metadata(pool, src_bucket, src_name)
        .await?
        .ok_or(StorageError::NotFound)?;

    let (id,): (Uuid,) = sqlx::query_as(
        r#"INSERT INTO storage.objects (bucket_id, name, owner, metadata)
           VALUES ($1, $2, $3, $4)
           ON CONFLICT (bucket_id, name) DO UPDATE
             SET updated_at = now(),
                 owner      = EXCLUDED.owner,
                 metadata   = EXCLUDED.metadata
           RETURNING id"#,
    )
    .bind(dst_bucket)
    .bind(dst_name)
    .bind(new_owner.or(src.owner))
    .bind(src.metadata)
    .fetch_one(pool)
    .await?;

    Ok(id)
}

// ── Quota ─────────────────────────────────────────────────────────────────────

pub async fn used_storage_bytes(pool: &PgPool) -> Result<i64> {
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM((metadata->>'size')::bigint), 0) \
         FROM storage.objects WHERE metadata ? 'size'",
    )
    .fetch_one(pool)
    .await?;
    Ok(total)
}

// ── Object RLS check ─────────────────────────────────────────────────────────

fn pg_role_or_anon(role: &str) -> &'static str {
    match role {
        "service_role" => "service_role",
        "authenticated" => "authenticated",
        _ => "anon",
    }
}

pub async fn rls_object_exists(
    pool: &PgPool,
    pg_role: &str,
    claims_json: &str,
    bucket_id: &str,
    object_name: &str,
) -> Result<bool> {
    let safe_role = pg_role_or_anon(pg_role);
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('role', $1, true)")
        .bind(safe_role)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT set_config('request.jwt.claims', $1, true)")
        .bind(claims_json)
        .execute(&mut *tx)
        .await?;
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM storage.objects WHERE bucket_id = $1 AND name = $2",
    )
    .bind(bucket_id)
    .bind(object_name)
    .fetch_optional(&mut *tx)
    .await?;
    tx.rollback().await?;
    Ok(row.is_some())
}

// ── Lifecycle rules ───────────────────────────────────────────────────────────

pub async fn create_lifecycle_rule(
    pool: &PgPool,
    bucket_id: &str,
    prefix: &str,
    expires_days: i32,
) -> Result<LifecycleRule> {
    sqlx::query_as(
        "INSERT INTO storage.lifecycle_rules (bucket_id, prefix, expires_days) \
         VALUES ($1, $2, $3) \
         RETURNING id, bucket_id, prefix, expires_days, created_at",
    )
    .bind(bucket_id)
    .bind(prefix)
    .bind(expires_days)
    .fetch_one(pool)
    .await
    .map_err(StorageError::Db)
}

pub async fn list_lifecycle_rules(pool: &PgPool, bucket_id: &str) -> Result<Vec<LifecycleRule>> {
    sqlx::query_as(
        "SELECT id, bucket_id, prefix, expires_days, created_at \
         FROM storage.lifecycle_rules WHERE bucket_id = $1 ORDER BY created_at",
    )
    .bind(bucket_id)
    .fetch_all(pool)
    .await
    .map_err(StorageError::Db)
}

pub async fn delete_lifecycle_rule(pool: &PgPool, bucket_id: &str, rule_id: Uuid) -> Result<bool> {
    let r = sqlx::query(
        "DELETE FROM storage.lifecycle_rules WHERE id = $1 AND bucket_id = $2",
    )
    .bind(rule_id)
    .bind(bucket_id)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

// Returns (bucket_id, object_name) pairs that have passed their lifecycle expiry.
pub async fn find_expired_objects(pool: &PgPool) -> anyhow::Result<Vec<(String, String)>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT DISTINCT o.bucket_id, o.name
           FROM storage.objects o
           WHERE EXISTS (
               SELECT 1 FROM storage.lifecycle_rules r
               WHERE r.bucket_id = o.bucket_id
                 AND o.created_at < now() - make_interval(days => r.expires_days)
                 AND (r.prefix = '' OR o.name LIKE r.prefix || '%')
           )"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn delete_objects_by_pairs(
    pool: &PgPool,
    pairs: &[(String, String)],
) -> anyhow::Result<u64> {
    if pairs.is_empty() {
        return Ok(0);
    }
    let mut total = 0u64;
    for (bucket_id, name) in pairs {
        let r = sqlx::query(
            "DELETE FROM storage.objects WHERE bucket_id = $1 AND name = $2",
        )
        .bind(bucket_id)
        .bind(name)
        .execute(pool)
        .await?;
        total += r.rows_affected();
    }
    Ok(total)
}
