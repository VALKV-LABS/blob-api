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
//
// Brings the database to what this service needs without assuming it starts
// empty. Every step first checks whether its object already exists and only
// creates what is missing, so it works:
//   - standalone, on an empty database, as its owner;
//   - beside an auth service (e.g. GoTrue on supabase/postgres) that already
//     owns the `auth` schema and the API roles, and an older `storage` schema,
//     connected as a non-superuser that owns the storage tables
//     (supabase_storage_admin), which may not even use the `auth` schema.
// Nothing here replaces an object another service made, and the policies read
// the caller from the request's claims rather than through auth.uid(), so
// this service never depends on `auth`. Postgres checks most
// privileges before it checks existence (CREATE SCHEMA IF NOT EXISTS,
// CREATE ROLE … BYPASSRLS, ALTER TABLE … IF NOT EXISTS), which is why each step
// is an explicit existence check rather than IF NOT EXISTS or a caught
// duplicate_object.
//
// STORAGE_SCHEMA_INIT=false skips all of it, for a schema managed elsewhere.

/// The columns this service reads from storage.buckets, beyond its key.
const BUCKET_COLUMNS: &[(&str, &str)] = &[
    ("name",               "text"),
    ("owner",              "uuid"),
    ("created_at",         "timestamptz DEFAULT now()"),
    ("updated_at",         "timestamptz DEFAULT now()"),
    ("public",             "boolean DEFAULT false"),
    ("file_size_limit",    "bigint"),
    ("allowed_mime_types", "text[]"),
    ("cache_control",      "text"),
];

/// The columns this service reads from storage.objects, beyond its key.
const OBJECT_COLUMNS: &[(&str, &str)] = &[
    ("bucket_id",        "text"),
    ("name",             "text"),
    ("owner",            "uuid"),
    ("created_at",       "timestamptz DEFAULT now()"),
    ("updated_at",       "timestamptz DEFAULT now()"),
    ("last_accessed_at", "timestamptz DEFAULT now()"),
    ("metadata",         "jsonb"),
];

/// SQL that adds `column` to `table` only if it isn't there. ALTER TABLE …
/// ADD COLUMN IF NOT EXISTS would need the table's owner even when it is.
fn add_column_if_missing(table: &str, column: &str, ty: &str) -> String {
    format!(
        "DO $init$ BEGIN
           IF NOT EXISTS (SELECT 1 FROM pg_attribute
                          WHERE attrelid = '{table}'::regclass AND attname = '{column}' AND NOT attisdropped) THEN
             ALTER TABLE {table} ADD COLUMN {column} {ty};
           END IF;
         END $init$"
    )
}

/// SQL that creates `role` only if no role by that name exists.
fn create_role_if_missing(role: &str, options: &str) -> String {
    format!(
        "DO $init$ BEGIN
           IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role}') THEN
             CREATE ROLE {role} {options};
           END IF;
         END $init$"
    )
}

/// The caller's user id, as auth.uid() returns it, read straight from the
/// request's claims (set per request with set_config) so the policies don't
/// need the `auth` schema.
const CALLER_UID: &str = "NULLIF(current_setting('request.jwt.claims', true)::jsonb->>'sub', '')::uuid";

/// SQL that creates a policy on storage.objects only if none by that name exists.
fn create_policy_if_missing(name: &str, body: &str) -> String {
    format!(
        "DO $init$ BEGIN
           IF NOT EXISTS (SELECT 1 FROM pg_policies
                          WHERE schemaname = 'storage' AND tablename = 'objects' AND policyname = '{name}') THEN
             CREATE POLICY {name} ON storage.objects {body};
           END IF;
         END $init$"
    )
}

/// Every statement init_schema runs, in order, each with a label for errors.
pub fn schema_statements() -> Vec<(String, String)> {
    let mut stmts: Vec<(String, String)> = Vec::new();
    let mut add = |label: &str, sql: String| stmts.push((label.to_string(), sql));

    add("storage schema", r#"DO $init$ BEGIN
          IF to_regnamespace('storage') IS NULL THEN CREATE SCHEMA storage; END IF;
        END $init$"#.into());

    // auth: an auth service, when there is one, owns this schema; leave it be.
    // Standalone (no auth schema), provide the usual helpers for customers'
    // own policies, as earlier versions did.
    add("auth schema and helpers (standalone only)", r#"DO $init$ BEGIN
          IF to_regnamespace('auth') IS NULL THEN
            CREATE SCHEMA auth;
            CREATE FUNCTION auth.uid() RETURNS uuid LANGUAGE sql STABLE AS $fn$
              SELECT NULLIF(
                COALESCE(current_setting('request.jwt.claims', true)::jsonb->>'sub', ''),
                ''
              )::uuid
            $fn$;
            CREATE FUNCTION auth.role() RETURNS text LANGUAGE sql STABLE AS $fn$
              SELECT COALESCE(current_setting('request.jwt.claims', true)::jsonb->>'role', 'anon')::text
            $fn$;
            CREATE FUNCTION auth.jwt() RETURNS jsonb LANGUAGE sql STABLE AS $fn$
              SELECT current_setting('request.jwt.claims', true)::jsonb
            $fn$;
          END IF;
        END $init$"#.into());

    add("role anon",          create_role_if_missing("anon", "NOLOGIN NOINHERIT"));
    add("role authenticated", create_role_if_missing("authenticated", "NOLOGIN NOINHERIT"));
    add("role service_role",  create_role_if_missing("service_role", "BYPASSRLS NOLOGIN NOINHERIT"));

    add("table storage.buckets", r#"DO $init$ BEGIN
          IF to_regclass('storage.buckets') IS NULL THEN
            CREATE TABLE storage.buckets (
              id                 text        PRIMARY KEY,
              name               text        NOT NULL UNIQUE,
              owner              uuid,
              created_at         timestamptz DEFAULT now(),
              updated_at         timestamptz DEFAULT now(),
              public             boolean     DEFAULT false,
              file_size_limit    bigint,
              allowed_mime_types text[],
              cache_control      text
            );
          END IF;
        END $init$"#.into());
    for (column, ty) in BUCKET_COLUMNS {
        add(&format!("column storage.buckets.{column}"), add_column_if_missing("storage.buckets", column, ty));
    }

    add("table storage.objects", r#"DO $init$ BEGIN
          IF to_regclass('storage.objects') IS NULL THEN
            CREATE TABLE storage.objects (
              id               uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
              bucket_id        text        REFERENCES storage.buckets(id) ON DELETE CASCADE,
              name             text        NOT NULL,
              owner            uuid,
              created_at       timestamptz DEFAULT now(),
              updated_at       timestamptz DEFAULT now(),
              last_accessed_at timestamptz DEFAULT now(),
              metadata         jsonb,
              UNIQUE (bucket_id, name)
            );
          END IF;
        END $init$"#.into());
    for (column, ty) in OBJECT_COLUMNS {
        add(&format!("column storage.objects.{column}"), add_column_if_missing("storage.objects", column, ty));
    }
    // Uploads use ON CONFLICT (bucket_id, name), which needs a unique index on
    // exactly those columns. An existing schema may have one under another name.
    add("unique index storage.objects (bucket_id, name)", r#"DO $init$ BEGIN
          IF NOT EXISTS (
            SELECT 1 FROM pg_index i
            WHERE i.indrelid = 'storage.objects'::regclass AND i.indisunique AND i.indpred IS NULL
              AND (SELECT array_agg(k ORDER BY k) FROM unnest(i.indkey::int2[]) AS k)
                = (SELECT array_agg(attnum ORDER BY attnum) FROM pg_attribute
                   WHERE attrelid = 'storage.objects'::regclass AND attname IN ('bucket_id', 'name'))
          ) THEN
            CREATE UNIQUE INDEX objects_bucket_id_name_key ON storage.objects (bucket_id, name);
          END IF;
        END $init$"#.into());

    add("table storage.multipart_uploads", r#"CREATE TABLE IF NOT EXISTS storage.multipart_uploads (
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
        )"#.into());
    add("table storage.lifecycle_rules", r#"CREATE TABLE IF NOT EXISTS storage.lifecycle_rules (
          id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
          bucket_id    text        NOT NULL REFERENCES storage.buckets(id) ON DELETE CASCADE,
          prefix       text        NOT NULL DEFAULT '',
          expires_days int         NOT NULL CHECK (expires_days > 0),
          created_at   timestamptz NOT NULL DEFAULT now()
        )"#.into());

    add("grants", "GRANT USAGE ON SCHEMA storage TO anon, authenticated, service_role".into());
    add("grants", "GRANT ALL ON ALL TABLES IN SCHEMA storage TO service_role".into());
    add("grants", "GRANT SELECT ON storage.buckets TO anon, authenticated".into());
    add("grants", "GRANT SELECT, INSERT, UPDATE, DELETE ON storage.objects TO authenticated".into());
    add("grants", "GRANT SELECT ON storage.objects TO anon".into());
    // Requests run as anon/authenticated (set_config('role', …)), so this
    // service's own role must be able to switch to them. Already true where an
    // auth service set up the roles (through authenticator); best effort otherwise.
    add("membership in anon, authenticated", r#"DO $init$ BEGIN
          IF NOT (pg_has_role(current_user, 'anon', 'MEMBER') AND pg_has_role(current_user, 'authenticated', 'MEMBER')) THEN
            GRANT anon, authenticated TO CURRENT_USER;
          END IF;
        EXCEPTION WHEN OTHERS THEN
          RAISE WARNING 'blob-api: could not grant anon, authenticated to %: %', current_user, SQLERRM;
        END $init$"#.into());

    add("row level security", "ALTER TABLE storage.objects ENABLE ROW LEVEL SECURITY".into());
    add("policy objects_anon_select", create_policy_if_missing("objects_anon_select",
        "FOR SELECT TO anon USING (EXISTS (
           SELECT 1 FROM storage.buckets b WHERE b.id = storage.objects.bucket_id AND b.public = true))"));
    add("policy objects_auth_select", create_policy_if_missing("objects_auth_select", &format!(
        "FOR SELECT TO authenticated USING (owner = {CALLER_UID} OR EXISTS (
           SELECT 1 FROM storage.buckets b WHERE b.id = storage.objects.bucket_id AND b.public = true))")));
    add("policy objects_auth_insert", create_policy_if_missing("objects_auth_insert", &format!(
        "FOR INSERT TO authenticated WITH CHECK (owner = {CALLER_UID})")));
    add("policy objects_auth_update", create_policy_if_missing("objects_auth_update", &format!(
        "FOR UPDATE TO authenticated USING (owner = {CALLER_UID}) WITH CHECK (owner = {CALLER_UID})")));
    add("policy objects_auth_delete", create_policy_if_missing("objects_auth_delete", &format!(
        "FOR DELETE TO authenticated USING (owner = {CALLER_UID})")));

    stmts
}

pub async fn init_schema(pool: &PgPool) -> anyhow::Result<()> {
    for (label, stmt) in schema_statements() {
        sqlx::query(&stmt).execute(pool).await.map_err(|e| {
            anyhow::anyhow!("schema init failed at {label}: {e}")
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    #[test]
    fn every_column_the_queries_read_is_ensured() {
        // BUCKET_COLS is what bucket queries select; each must be ensured.
        for col in BUCKET_COLS.split(',').map(str::trim).filter(|c| *c != "id") {
            assert!(BUCKET_COLUMNS.iter().any(|(c, _)| *c == col), "storage.buckets.{col} is never ensured");
        }
    }

    #[test]
    fn nothing_is_replaced_or_created_unconditionally() {
        for (label, sql) in schema_statements() {
            let s = sql.to_uppercase();
            assert!(!s.contains("CREATE OR REPLACE"), "{label} replaces an object another service may own");
            for create in ["CREATE SCHEMA", "CREATE ROLE", "CREATE FUNCTION", "CREATE POLICY", "ADD COLUMN"] {
                if s.contains(create) {
                    assert!(s.contains("IF NOT EXISTS (") || s.contains(" IS NULL"),
                        "{label}: {create} without checking for an existing one first");
                }
            }
        }
    }

    #[test]
    fn auth_is_touched_only_when_it_does_not_exist() {
        for (label, sql) in schema_statements() {
            if label.starts_with("auth schema") {
                assert!(sql.contains("IF to_regnamespace('auth') IS NULL THEN"), "{label}: auth must be created only when absent");
            } else {
                assert!(!sql.contains("auth."), "{label} depends on the auth schema, which may belong to another service");
            }
        }
    }
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
