# valkv Blob API

[![CI](https://github.com/VALKV-LABS/blob-api/actions/workflows/ci.yml/badge.svg)](https://github.com/VALKV-LABS/blob-api/actions/workflows/ci.yml)
[![Release](https://github.com/VALKV-LABS/blob-api/actions/workflows/release.yml/badge.svg)](https://github.com/VALKV-LABS/blob-api/actions/workflows/release.yml)
[![Docker Hub](https://img.shields.io/docker/v/valkv/blob-api?sort=semver&label=docker&logo=docker)](https://hub.docker.com/r/valkv/blob-api)
[![License](https://img.shields.io/github/license/VALKV-LABS/blob-api)](LICENSE)

A self-hosted, Supabase Storage-compatible blob storage service written in Rust.
Stores object metadata in PostgreSQL and binary data in any S3-compatible backend
(AWS S3, MinIO, Cloudflare R2, Tigris, etc.).

## Features

- Supabase Storage-compatible REST API — works with existing Supabase client libraries
- Resumable uploads via [TUS protocol](https://tus.io/) (S3 multipart under the hood)
- Row-level security (RLS) — access policies enforced at the database level using JWT identity
- Per-bucket file size limits and allowed MIME type enforcement (exact, wildcard `image/*`, global `*`)
- Per-bucket `Cache-Control` header passed through on downloads
- Project-level cumulative storage quota (`STORAGE_QUOTA_BYTES`)
- Multi-tenant via path-based S3 key namespacing (`{tenant_id}/{bucket}/{object}`)
- Signed download URLs (single path and multi-path batch)
- Signed upload URLs — let clients upload directly without exposing the service key
- Server-side copy and move (no re-upload)
- Bulk delete by exact path list
- Object listing with prefix filter, substring search, and sort by any metadata column
- `HEAD` on any object — returns metadata (Content-Type, Content-Length) without streaming the body
- Image transforms proxied through [imgproxy](https://imgproxy.net/) when `IMGPROXY_URL` is set
- Bucket lifecycle rules — automatic object expiry after N days (daily background sweep)
- Public / private bucket access control

## Architecture

```
Client
  │  JWT (HS256)
  ▼
blob-api  (Axum, :5000)
  ├── PostgreSQL  — bucket & object metadata, RLS policies, lifecycle rules
  ├── S3          — object binary data
  │               key format: {TENANT_ID}/{bucket_id}/{object_name}
  └── imgproxy    — optional image transform sidecar (IMGPROXY_URL)
```

Object metadata (bucket, owner, mime type, size) lives in Postgres.
Binary data lives in one shared S3 bucket, namespaced by tenant.
There is no 1:1 mapping between storage "buckets" and S3 buckets — all
tenants share a single S3 bucket, isolated by key prefix.

---

## Running locally

### Option A — Full Docker Compose (MinIO + Postgres, zero config)

```bash
cd blob-api
make integ          # spins up postgres + minio + the API + runs the test suite
make down           # tear everything down
```

### Option B — Local binary against MinIO

Start Postgres and MinIO manually (or reuse the test compose):

```bash
docker compose -f docker-compose.test.yml --profile integ up -d postgres-test minio-test
```

Then run the binary with env vars:

```bash
export DATABASE_URL="postgresql://storage_test:test@localhost:5432/storage_test"
export PGRST_JWT_SECRET="your-jwt-secret-at-least-32-bytes"
export GLOBAL_S3_BUCKET="my-bucket"
export GLOBAL_S3_ENDPOINT="http://localhost:9000"
export GLOBAL_S3_FORCE_PATH_STYLE="true"
export AWS_ACCESS_KEY_ID="minioadmin"
export AWS_SECRET_ACCESS_KEY="minioadmin"
export AWS_DEFAULT_REGION="us-east-1"
export TENANT_ID="local-dev"

cargo run --bin blob-api
```

### Step-by-step: interactive local testing

The blob-api validates JWTs locally using `PGRST_JWT_SECRET` — it never calls
an external auth service at request time. You can mint tokens yourself and hit the API directly.

**1. Start backing services**

```bash
cd blob-api
docker compose -f docker-compose.test.yml --profile integ up -d postgres-test minio-test
```

Postgres is on `localhost:5432`, MinIO API on `localhost:9000`, MinIO console
on `http://localhost:9001` (login: `testkey` / `testsecret`).

**2. Run the API**

```bash
export DATABASE_URL="postgresql://storage_test:test@localhost:5432/storage_test"
export PGRST_JWT_SECRET="test-jwt-secret-32bytes-padded-ok"
export GLOBAL_S3_BUCKET="test-bucket"
export GLOBAL_S3_ENDPOINT="http://localhost:9000"
export GLOBAL_S3_FORCE_PATH_STYLE="true"
export AWS_ACCESS_KEY_ID="testkey"
export AWS_SECRET_ACCESS_KEY="testsecret"
export AWS_DEFAULT_REGION="us-east-1"
export TENANT_ID="local-dev"
export RUST_LOG="valkv_blob=debug"

cargo run --bin blob-api
```

The schema is created automatically on startup. You should see
`blob-api listening on 0.0.0.0:5000`.

**3. Mint a JWT**

There is no UI for this — generate a token from the command line. Save this as
`mint.js` and run it once:

```js
// mint.js
const { createHmac } = require('crypto')
const secret = process.env.PGRST_JWT_SECRET || 'test-jwt-secret-32bytes-padded-ok'
const role   = process.argv[2] || 'service_role'
const sub    = process.argv[3] || 'system'
const b64    = b => b.toString('base64').replace(/\+/g,'-').replace(/\//g,'_').replace(/=/g,'')
const h = b64(Buffer.from(JSON.stringify({ alg: 'HS256', typ: 'JWT' })))
const p = b64(Buffer.from(JSON.stringify({ role, sub, iat: Math.floor(Date.now()/1000) })))
const s = b64(createHmac('sha256', secret).update(`${h}.${p}`).digest())
console.log(`${h}.${p}.${s}`)
```

```bash
# service_role token (full access)
SERVICE_TOKEN=$(node mint.js service_role system)

# authenticated user token (RLS applies — owner = the sub UUID)
USER_TOKEN=$(node mint.js authenticated 11111111-0000-0000-0000-000000000001)
```

**4. Call the API with curl**

```bash
BASE="http://localhost:5000/storage/v1"

# Create a private bucket
curl -sf -X POST "$BASE/bucket" \
  -H "Authorization: Bearer $SERVICE_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"id":"my-bucket","name":"my-bucket","public":false}' | jq

# Upload a file
curl -sf -X POST "$BASE/object/my-bucket/hello.txt" \
  -H "Authorization: Bearer $SERVICE_TOKEN" \
  -H "Content-Type: text/plain" \
  --data-binary "hello world" | jq

# Download it
curl -sf "$BASE/object/my-bucket/hello.txt" \
  -H "Authorization: Bearer $SERVICE_TOKEN"

# List objects
curl -sf -X POST "$BASE/object/list/my-bucket" \
  -H "Authorization: Bearer $SERVICE_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"prefix":"","limit":20}' | jq

# Create a signed download URL (expires in 5 min)
curl -sf -X POST "$BASE/object/sign/my-bucket/hello.txt" \
  -H "Authorization: Bearer $SERVICE_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"expires_in":300}' | jq

# HEAD — metadata only, no body
curl -sI "$BASE/object/my-bucket/hello.txt" \
  -H "Authorization: Bearer $SERVICE_TOKEN"
```

**5. Run the automated test suite**

```bash
make integ          # clean slate + full integ test run
make unit           # Rust unit tests only (no external services needed)
```

---

## Running against AWS S3

### With AWS SSO (recommended for local development against real AWS)

**1. Configure SSO once:**

```bash
aws configure sso
# Follow the prompts — set SSO start URL, region, account, role.
# Give the profile a name, e.g. "valkv-dev".
```

**2. Login:**

```bash
aws sso login --profile valkv-dev
```

**3. Export credentials into the shell:**

```bash
# Option A — set AWS_PROFILE and let the SDK resolve via SSO token cache
export AWS_PROFILE=valkv-dev

# Option B — export short-lived keys directly (useful when AWS_PROFILE isn't picked up)
eval "$(aws configure export-credentials --profile valkv-dev --format env)"
# This sets AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_SESSION_TOKEN
```

**4. Set blob-api env vars and run:**

```bash
export DATABASE_URL="postgresql://..."
export PGRST_JWT_SECRET="your-secret"
export GLOBAL_S3_BUCKET="my-valkv-storage-bucket"
export AWS_DEFAULT_REGION="us-east-1"
export TENANT_ID="your-tenant-id"
# No endpoint override — omit GLOBAL_S3_ENDPOINT for real AWS

cargo run --bin blob-api
```

The SDK credential chain is: `AWS_PROFILE` SSO token → env vars → `~/.aws/credentials` →
ECS task role → EC2 instance metadata. The same binary works in all environments without
code changes — just set the right env vars (or attach an IAM role in production).

### Required IAM permissions

The IAM role or SSO permission set needs the following S3 actions on your bucket:

```json
{
  "Effect": "Allow",
  "Action": [
    "s3:CreateBucket",
    "s3:PutObject",
    "s3:GetObject",
    "s3:DeleteObject",
    "s3:DeleteObjects",
    "s3:ListBucket",
    "s3:AbortMultipartUpload",
    "s3:ListMultipartUploadParts",
    "s3:CreateMultipartUpload",
    "s3:CompleteMultipartUpload",
    "s3:CopyObject"
  ],
  "Resource": [
    "arn:aws:s3:::my-valkv-storage-bucket",
    "arn:aws:s3:::my-valkv-storage-bucket/*"
  ]
}
```

### On EC2 with an instance profile (production)

Attach an IAM instance profile with the permissions above to your EC2 instance.
Set no AWS credential env vars at all — the SDK discovers rotating credentials
from the instance metadata service (IMDS v2) automatically.

---

## Environment variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `DATABASE_URL` | ✅ | — | PostgreSQL connection string. Schema is auto-created on startup. |
| `PGRST_JWT_SECRET` | ✅ | — | HS256 secret used to verify client JWTs. Also accepted as `JWT_SECRET`. Must be ≥ 32 bytes. |
| `GLOBAL_S3_BUCKET` | ✅ | — | Name of the S3 bucket that holds all object data. |
| `AWS_DEFAULT_REGION` | — | `us-east-1` | AWS region for the S3 bucket. |
| `GLOBAL_S3_ENDPOINT` | — | _(AWS)_ | Custom S3 endpoint URL. Set for MinIO, R2, Tigris, or any S3-compatible service. |
| `GLOBAL_S3_FORCE_PATH_STYLE` | — | `false` | Set `true` for MinIO and other non-AWS S3 services that require path-style addressing. |
| `AWS_ACCESS_KEY_ID` | — | _(chain)_ | Static access key. Dev / docker-compose only. In production, use an IAM role instead. |
| `AWS_SECRET_ACCESS_KEY` | — | _(chain)_ | Static secret key. Dev / docker-compose only. |
| `AWS_SESSION_TOKEN` | — | _(chain)_ | STS session token. Set automatically when using `aws configure export-credentials`. |
| `AWS_PROFILE` | — | — | AWS named profile. Enables SSO token-based auth without exporting static keys. |
| `TENANT_ID` | — | `""` | Prefix added to every S3 key: `{TENANT_ID}/{bucket}/{object}`. Use the account UUID in multi-tenant deployments. Empty string means no prefix. |
| `PORT` | — | `5000` | Port the HTTP server listens on. |
| `FILE_SIZE_LIMIT` | — | `52428800` | Global maximum upload size in bytes (default 50 MB). Per-bucket limits can be set lower via the bucket API. |
| `STORAGE_QUOTA_BYTES` | — | _(unset)_ | Maximum cumulative bytes stored across all objects in this project. Uploads that would push total usage over this limit are rejected with 413. Unset means no quota is enforced. |
| `UPLOAD_SIGNED_URL_EXPIRATION_TIME` | — | `120` | Default signed URL TTL in seconds. Callers can override per-request. |
| `IMGPROXY_URL` | — | _(unset)_ | Base URL of an [imgproxy](https://imgproxy.net/) instance, e.g. `http://imgproxy:8080`. When set, requests to `/render/image/…` are proxied through imgproxy for on-the-fly resize, crop, and format conversion. When unset, those routes return the raw object. |
| `SERVICE_KEY` | — | `""` | Pre-issued service role JWT. Informational only — not enforced by the API itself. |
| `ANON_KEY` | — | `""` | Pre-issued anon JWT. Informational only. |
| `RUST_LOG` | — | — | Tracing filter, e.g. `valkv_blob=debug,info`. |

---

## API reference

All routes are prefixed with `/storage/v1`.

### Health

| Method | Path | Auth | Description |
|---|---|---|---|
| `GET` | `/health` | None | Returns `{"status":"ok"}` |

### Buckets

Create, update, and delete require a `service_role` JWT. List and get require any valid JWT.

| Method | Path | Description |
|---|---|---|
| `POST` | `/bucket` | Create a bucket |
| `GET` | `/bucket` | List all buckets |
| `GET` | `/bucket/:id` | Get bucket metadata |
| `PATCH` | `/bucket/:id` | Update bucket |
| `DELETE` | `/bucket/:id` | Delete bucket (must be empty) |

**Create / update bucket fields:**

| Field | Type | Description |
|---|---|---|
| `id` | string | Bucket identifier (create only, required) |
| `name` | string | Display name (defaults to `id`) |
| `public` | bool | Allow unauthenticated downloads via `/object/public/…` |
| `file_size_limit` | int | Per-upload byte cap for this bucket (overrides global `FILE_SIZE_LIMIT`) |
| `allowed_mime_types` | string[] | Allowlist of MIME patterns. Supports exact (`image/png`), wildcard (`image/*`), and global (`*`). Null/empty = allow all. |
| `cache_control` | string | Value sent as `Cache-Control` header on all object downloads from this bucket, e.g. `"max-age=3600, public"`. |

```json
{
  "id": "avatars",
  "public": false,
  "file_size_limit": 5242880,
  "allowed_mime_types": ["image/png", "image/jpeg", "image/*"],
  "cache_control": "max-age=86400, public"
}
```

### Bucket lifecycle rules

Require a `service_role` JWT (reads require any valid JWT).

Objects that match a rule are automatically deleted by a background sweep that runs once per day.

| Method | Path | Description |
|---|---|---|
| `POST` | `/bucket/:id/lifecycle` | Create a lifecycle rule |
| `GET` | `/bucket/:id/lifecycle` | List lifecycle rules for a bucket |
| `DELETE` | `/bucket/:id/lifecycle/:rule_id` | Delete a lifecycle rule |

**Create rule request body:**

```json
{ "prefix": "tmp/", "expires_days": 7 }
```

`prefix` is optional (defaults to `""` = all objects). `expires_days` must be ≥ 1.
Objects whose `name` starts with `prefix` and whose `created_at` is older than `expires_days` days are deleted on the next daily sweep.

### Objects

| Method | Path | Auth | Description |
|---|---|---|---|
| `POST` | `/object/:bucket/*path` | JWT | Upload — fails if object already exists |
| `PUT` | `/object/:bucket/*path` | JWT | Upload or overwrite (upsert) |
| `GET` | `/object/:bucket/*path` | JWT | Download (RLS enforced for private buckets) |
| `HEAD` | `/object/:bucket/*path` | JWT | Metadata only — returns `Content-Type` and `Content-Length`, no body |
| `DELETE` | `/object/:bucket/*path` | JWT | Delete a single object |
| `DELETE` | `/object/:bucket` | JWT | **Bulk delete** — body `{"prefixes": ["path1", "path2"]}` (exact paths, not prefix-matched) |
| `POST` | `/object/list/:bucket` | JWT | List objects |
| `GET` | `/object/public/:bucket/*path` | None | Download from a public bucket (no auth) |
| `HEAD` | `/object/public/:bucket/*path` | None | Metadata from a public bucket, no body |
| `POST` | `/object/copy` | `service_role` | Server-side copy (no re-upload) |
| `POST` | `/object/move` | `service_role` | Server-side move / rename |

**List request body:**

```json
{
  "prefix": "avatars/",
  "search": "profile",
  "limit": 100,
  "offset": 0,
  "sort_by": { "column": "created_at", "order": "desc" }
}
```

`prefix` filters by key prefix. `search` is an additional substring match on the name within that prefix. `sort_by.column` accepts `name` (default), `created_at`, `updated_at`, `last_accessed_at`. `sort_by.order` accepts `asc` (default) or `desc`.

**Copy / move request body:**

```json
{
  "bucketId": "source-bucket",
  "sourceKey": "path/to/object.png",
  "destinationBucket": "dest-bucket",
  "destinationKey": "new/path/object.png"
}
```

### Signed URLs

| Method | Path | Auth | Description |
|---|---|---|---|
| `POST` | `/object/sign/:bucket/*path` | JWT | Create a signed **download** URL for a single object |
| `POST` | `/object/sign/:bucket` | JWT | Create signed **download** URLs for multiple paths in one call |
| `GET` | `/object/sign/:bucket/*path?token=…` | None | Download via signed URL |
| `POST` | `/object/upload/sign/:bucket/*path` | JWT | Create a signed **upload** URL (lets a client upload without the service key) |
| `PUT` | `/object/upload/sign/:bucket/*path?token=…` | None | Upload via signed URL |

**Single-path sign request:**
```json
{ "expires_in": 300 }
```

**Multi-path sign request (`POST /object/sign/:bucket`):**
```json
{ "paths": ["avatars/user1.png", "avatars/user2.png"], "expires_in": 300 }
```

**Signed upload URL flow:**
1. Backend calls `POST /object/upload/sign/:bucket/path/to/file.jpg` with a service/user JWT.
2. Response: `{ "url": "/storage/v1/object/upload/sign/…?token=…", "token": "…", "path": "…" }`.
3. Client browser `PUT`s the file bytes directly to that URL — no service key needed.
4. Upload tokens are scoped to the exact bucket + path and cannot be used as download tokens.

### Image transforms

Requests to these routes are proxied through [imgproxy](https://imgproxy.net/) when
`IMGPROXY_URL` is set. When `IMGPROXY_URL` is not configured they fall back to raw object
download — so they work in all environments without code changes.

| Method | Path | Auth | Description |
|---|---|---|---|
| `GET` | `/render/image/authenticated/:bucket/*path` | JWT | Transform and serve (respects RLS) |
| `GET` | `/render/image/public/:bucket/*path` | None | Transform and serve (bucket must be public) |

**Query parameters:**

| Param | Example | Description |
|---|---|---|
| `width` | `300` | Resize width in pixels |
| `height` | `200` | Resize height in pixels |
| `format` | `webp` | Output format (`webp`, `avif`, `png`, `jpeg`) |
| `quality` | `85` | Output quality (1–100) |

Width and height together use `resize:fit` mode (preserves aspect ratio). Either dimension alone constrains that axis only.

### Resumable uploads (TUS)

| Method | Path | Description |
|---|---|---|
| `POST` | `/upload/resumable` | Initiate a resumable upload — include `bucketName` and `objectName` in `Upload-Metadata` header |
| `HEAD` | `/upload/resumable/:id` | Query upload progress |
| `PATCH` | `/upload/resumable/:id` | Upload a chunk |
| `DELETE` | `/upload/resumable/:id` | Abort upload |

TUS extensions supported: **creation**, **termination**.

---

## Auth model

The API uses HS256 JWTs signed with `PGRST_JWT_SECRET`. Three roles are recognised:

| `role` claim | Access |
|---|---|
| `service_role` | Full access — bypasses RLS, can manage buckets, copy/move objects |
| `authenticated` | Can upload/download/delete own objects; subject to RLS |
| `anon` | Read-only access to public bucket objects only |

Row-level security is enforced at the PostgreSQL level: for non-`service_role` requests
the API opens a transaction with the caller's role and JWT claims set, so Postgres
evaluates the RLS policies directly.

Signed upload URLs act as bearer tokens scoped to a single (bucket, path) pair. They
carry a `kind: "upload"` claim and are cryptographically rejected if presented to the
download endpoint, and vice versa.

---

## Development

### Prerequisites

- Docker (for tests)
- Rust 1.92+ (for local builds)
- `make`

### Commands

```bash
make unit            # run Rust unit tests inside Docker (no external services)
make integ           # run integration tests (Postgres + MinIO + the API)
make unit NO_CACHE=1 # rebuild Docker image from scratch (clears cargo cache)
make down            # stop and remove all test containers and volumes
```

### Project layout

```
src/
  main.rs          — startup, schema init, lifecycle task spawn, server bind
  config.rs        — environment variable loading
  auth.rs          — JWT decode, Claims struct
  db.rs            — schema bootstrap, bucket/object/lifecycle helpers, RLS check
  s3.rs            — S3Client wrapper (put/get/delete/copy/batch-delete/multipart)
  signed.rs        — signed URL creation and verification (download + upload tokens)
  lifecycle.rs     — daily background sweep that expires objects by lifecycle rules
  error.rs         — StorageError enum → HTTP response
  routes/
    mod.rs         — axum Router wiring
    buckets.rs     — bucket CRUD + cache_control
    objects.rs     — object upload/download/head/list/delete/copy/move/sign/render
    lifecycle.rs   — lifecycle rule CRUD endpoints
    tus.rs         — TUS resumable upload
    health.rs      — health check
test/
  storage-integ.js — Node.js integration test suite (no dependencies)
Makefile
docker-compose.test.yml
Dockerfile
```

---

## Contributing

1. Fork the repo and create a feature branch.
2. Add or update tests — unit tests live alongside source in `#[cfg(test)]` modules;
   integration tests go in `test/blob-integ.js` as additional `step()` calls.
3. Run `make unit && make integ` — both must pass.
4. Open a pull request describing what changed and why.

Please do not reference internal infrastructure names (MinIO, RocksDB, etc.) in
user-visible error messages or API responses.

---

## Releasing

Every merge to `main` is automatically tagged with the next minor version and
published to [Docker Hub](https://hub.docker.com/r/valkv/blob-api). No manual
tagging required.

**Auto-release flow**

```
PR merged to main
  → workflow computes next minor version (v1.0.0 → v1.1.0)
  → creates the git tag
  → builds linux/amd64 + linux/arm64 natively (no emulation)
  → pushes multi-platform image to Docker Hub
```

| Docker tag | Example |
|---|---|
| Full version | `valkv/blob-api:1.1.0` |
| Minor | `valkv/blob-api:1.1` |
| Major | `valkv/blob-api:1` |
| Latest stable | `valkv/blob-api:latest` |

**Manual release** (specific version): Go to Actions → Release → Run workflow →
enter the tag (e.g. `v1.5.0`). The tag must not already exist.

**To undo a release** (before the workflow completes):

```bash
git push origin :refs/tags/v1.1.0   # delete remote tag
```

Note: once pushed to Docker Hub the image cannot be unpublished from the registry,
only the git tag can be removed.
