'use strict'
// Blob API integration test — runs against a live blob-api + S3 + Postgres.

const BASE = process.env.STORAGE_URL || 'http://localhost:5000'
const JWT_SECRET = process.env.JWT_SECRET || 'test-jwt-secret-32bytes-padded-ok'

// ── Minimal HS256 JWT mint (no deps) ─────────────────────────────────────────
const { createHmac } = require('crypto')

function b64url(buf) {
  return buf.toString('base64').replace(/\+/g, '-').replace(/\//g, '_').replace(/=/g, '')
}

function mintJwt(payload, secret) {
  const header = b64url(Buffer.from(JSON.stringify({ alg: 'HS256', typ: 'JWT' })))
  const body   = b64url(Buffer.from(JSON.stringify(payload)))
  const sig    = b64url(createHmac('sha256', secret).update(`${header}.${body}`).digest())
  return `${header}.${body}.${sig}`
}

const serviceToken = mintJwt({ role: 'service_role', sub: 'system', iat: Math.floor(Date.now() / 1000) }, JWT_SECRET)
const anonToken    = mintJwt({ role: 'anon',         sub: '',       iat: Math.floor(Date.now() / 1000) }, JWT_SECRET)
// Two distinct authenticated users used in RLS tests
const userAToken   = mintJwt({ role: 'authenticated', sub: '11111111-0000-0000-0000-000000000001', iat: Math.floor(Date.now() / 1000) }, JWT_SECRET)
const userBToken   = mintJwt({ role: 'authenticated', sub: '22222222-0000-0000-0000-000000000002', iat: Math.floor(Date.now() / 1000) }, JWT_SECRET)

function svcHdr(withBody = false) {
  const h = { Authorization: `Bearer ${serviceToken}` }
  if (withBody) h['Content-Type'] = 'application/json'
  return h
}

// ── Test harness ─────────────────────────────────────────────────────────────
let passed = 0, failed = 0
const steps = []

function step(name, fn) { steps.push({ name, fn }) }

async function run() {
  for (const { name, fn } of steps) {
    try {
      await fn()
      console.log(`  ✓ ${name}`)
      passed++
    } catch (e) {
      console.error(`  ✗ ${name}: ${e.message}`)
      failed++
    }
  }
  console.log(`\n${passed + failed} tests — ${passed} passed, ${failed} failed`)
  if (failed > 0) process.exit(1)
}

function assert(cond, msg) {
  if (!cond) throw new Error(msg || 'assertion failed')
}

async function api(method, path, opts = {}) {
  const { headers = {}, body, raw } = opts
  const init = { method, headers }
  if (body !== undefined) {
    if (typeof body === 'string' || Buffer.isBuffer(body)) {
      init.body = body
    } else {
      init.body = JSON.stringify(body)
    }
  }
  const res = await fetch(`${BASE}${path}`, init)
  if (raw) return res
  const text = await res.text()
  let data
  try { data = JSON.parse(text) } catch { data = text }
  return { status: res.status, data, headers: res.headers }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

step('health check returns ok', async () => {
  const { status, data } = await api('GET', '/storage/v1/health')
  assert(status === 200, `want 200 got ${status}`)
  assert(data.status === 'ok', `want ok got ${data.status}`)
})

// ── Bucket lifecycle ──────────────────────────────────────────────────────────

step('create private bucket', async () => {
  const { status, data } = await api('POST', '/storage/v1/bucket', {
    headers: svcHdr(true),
    body: { id: 'private-bucket', name: 'private-bucket', public: false },
  })
  assert(status === 200, `want 200 got ${status}: ${JSON.stringify(data)}`)
  assert(data.name === 'private-bucket', `name mismatch: ${JSON.stringify(data)}`)
})

step('create public bucket', async () => {
  const { status } = await api('POST', '/storage/v1/bucket', {
    headers: svcHdr(true),
    body: { id: 'public-bucket', name: 'public-bucket', public: true },
  })
  assert(status === 200, `want 200 got ${status}`)
})

step('duplicate bucket returns 409', async () => {
  const { status } = await api('POST', '/storage/v1/bucket', {
    headers: svcHdr(true),
    body: { id: 'private-bucket', name: 'private-bucket' },
  })
  assert(status === 409, `want 409 got ${status}`)
})

step('list buckets returns both', async () => {
  const { status, data } = await api('GET', '/storage/v1/bucket', { headers: svcHdr() })
  assert(status === 200, `want 200 got ${status}`)
  assert(Array.isArray(data), 'want array')
  const ids = data.map(b => b.id)
  assert(ids.includes('private-bucket'), 'missing private-bucket')
  assert(ids.includes('public-bucket'), 'missing public-bucket')
})

step('get bucket by id', async () => {
  const { status, data } = await api('GET', '/storage/v1/bucket/private-bucket', { headers: svcHdr() })
  assert(status === 200, `want 200 got ${status}`)
  assert(data.id === 'private-bucket')
})

step('get missing bucket returns 404', async () => {
  const { status } = await api('GET', '/storage/v1/bucket/no-such-bucket', { headers: svcHdr() })
  assert(status === 404, `want 404 got ${status}`)
})

step('update bucket public flag', async () => {
  const { status, data } = await api('PATCH', '/storage/v1/bucket/private-bucket', {
    headers: svcHdr(true),
    body: { public: true },
  })
  assert(status === 200, `want 200 got ${status}`)
  assert(data.public === true, `want public=true: ${JSON.stringify(data)}`)
  // revert
  await api('PATCH', '/storage/v1/bucket/private-bucket', {
    headers: svcHdr(true),
    body: { public: false },
  })
})

// ── Object lifecycle ──────────────────────────────────────────────────────────

const objContent = 'hello from storage-integ test'
const objContentType = 'text/plain'

step('upload object (POST — fail if exists)', async () => {
  const { status, data } = await api('POST', '/storage/v1/object/private-bucket/hello.txt', {
    headers: { ...svcHdr(), 'Content-Type': objContentType },
    body: objContent,
  })
  assert(status === 200, `want 200 got ${status}: ${JSON.stringify(data)}`)
  assert(data.Key === 'private-bucket/hello.txt', `Key mismatch: ${JSON.stringify(data)}`)
})

step('upload same object again returns 409', async () => {
  const { status } = await api('POST', '/storage/v1/object/private-bucket/hello.txt', {
    headers: { ...svcHdr(), 'Content-Type': objContentType },
    body: 'different content',
  })
  assert(status === 409, `want 409 got ${status}`)
})

step('upsert object (PUT) overwrites', async () => {
  const { status } = await api('PUT', '/storage/v1/object/private-bucket/hello.txt', {
    headers: { ...svcHdr(), 'Content-Type': objContentType },
    body: 'updated content',
  })
  assert(status === 200, `want 200 got ${status}`)
})

step('download object (authenticated)', async () => {
  const res = await api('GET', '/storage/v1/object/private-bucket/hello.txt', {
    headers: svcHdr(),
    raw: true,
  })
  assert(res.status === 200, `want 200 got ${res.status}`)
  const text = await res.text()
  assert(text === 'updated content', `body mismatch: ${text}`)
})

step('download via public route on private bucket returns 403', async () => {
  const { status } = await api('GET', '/storage/v1/object/public/private-bucket/hello.txt', { raw: true })
  assert(status === 403, `want 403 got ${status}`)
})

step('upload to public bucket and download without auth', async () => {
  await api('POST', '/storage/v1/object/public-bucket/img.png', {
    headers: { ...svcHdr(), 'Content-Type': 'image/png' },
    body: Buffer.from([137, 80, 78, 71]), // PNG magic bytes
  })
  const res = await api('GET', '/storage/v1/object/public/public-bucket/img.png', { raw: true })
  assert(res.status === 200, `want 200 got ${res.status}`)
})

step('list objects in bucket', async () => {
  const { status, data } = await api('POST', '/storage/v1/object/list/private-bucket', {
    headers: svcHdr(true),
    body: { prefix: '', limit: 100 },
  })
  assert(status === 200, `want 200 got ${status}`)
  assert(Array.isArray(data), 'want array')
  assert(data.length >= 1, 'want at least 1 object')
})

step('list objects with prefix filter', async () => {
  await api('POST', '/storage/v1/object/private-bucket/docs/readme.md', {
    headers: { ...svcHdr(), 'Content-Type': 'text/markdown' },
    body: '# readme',
  })
  const { status, data } = await api('POST', '/storage/v1/object/list/private-bucket', {
    headers: svcHdr(true),
    body: { prefix: 'docs/', limit: 10 },
  })
  assert(status === 200, `want 200 got ${status}`)
  assert(data.every(o => o.name.startsWith('docs/')), 'prefix filter failed')
})

// ── Signed URL flow ───────────────────────────────────────────────────────────

let signedUrl = ''

step('create signed URL', async () => {
  const { status, data } = await api('POST', '/storage/v1/object/sign/public-bucket/img.png', {
    headers: svcHdr(true),
    body: { expires_in: 300 },
  })
  assert(status === 200, `want 200 got ${status}: ${JSON.stringify(data)}`)
  assert(data.signedURL, 'no signedURL in response')
  signedUrl = data.signedURL
})

step('download via signed URL (no auth header)', async () => {
  const res = await api('GET', signedUrl, { raw: true })
  assert(res.status === 200, `want 200 got ${res.status}`)
})

step('signed URL with wrong path returns 401', async () => {
  const badToken = mintJwt({ url: 'public-bucket/other.txt', exp: Math.floor(Date.now() / 1000) + 300 }, JWT_SECRET)
  const { status } = await api('GET', `/storage/v1/object/sign/public-bucket/img.png?token=${badToken}`, { raw: true })
  assert(status === 401, `want 401 got ${status}`)
})

// ── Delete ────────────────────────────────────────────────────────────────────

step('delete object', async () => {
  const { status, data } = await api('DELETE', '/storage/v1/object/private-bucket/hello.txt', {
    headers: svcHdr(),
  })
  assert(status === 200, `want 200 got ${status}`)
  assert(Array.isArray(data) && data[0].name === 'hello.txt')
})

step('delete missing object returns 404', async () => {
  const { status } = await api('DELETE', '/storage/v1/object/private-bucket/hello.txt', {
    headers: svcHdr(),
  })
  assert(status === 404, `want 404 got ${status}`)
})

step('delete non-empty bucket returns 409', async () => {
  const { status } = await api('DELETE', '/storage/v1/bucket/public-bucket', {
    headers: svcHdr(),
  })
  assert(status === 409, `want 409 got ${status}`)
})

step('delete bucket after emptying', async () => {
  // empty public-bucket first
  await api('DELETE', '/storage/v1/object/public-bucket/img.png', { headers: svcHdr() })
  const { status } = await api('DELETE', '/storage/v1/bucket/public-bucket', { headers: svcHdr() })
  assert(status === 200, `want 200 got ${status}`)
})

// ── Auth enforcement ──────────────────────────────────────────────────────────

step('anon token cannot create bucket', async () => {
  const { status } = await api('POST', '/storage/v1/bucket', {
    headers: { Authorization: `Bearer ${anonToken}`, 'Content-Type': 'application/json' },
    body: { id: 'anon-bucket' },
  })
  assert(status === 403, `want 403 got ${status}`)
})

step('upload without any token returns 401 (anon blocked from writing)', async () => {
  const { status } = await api('POST', '/storage/v1/object/private-bucket/nope.txt', {
    headers: { 'Content-Type': 'text/plain' },
    body: 'data',
  })
  assert(status === 401, `want 401 got ${status}`)
})

// ── Per-bucket file size limit ────────────────────────────────────────────────

step('create bucket with 100-byte file size limit', async () => {
  const { status, data } = await api('POST', '/storage/v1/bucket', {
    headers: svcHdr(true),
    body: { id: 'size-limit-bucket', name: 'size-limit-bucket', public: false, file_size_limit: 100 },
  })
  assert(status === 200, `want 200 got ${status}: ${JSON.stringify(data)}`)
})

step('upload exceeding bucket file_size_limit returns 413', async () => {
  const { status } = await api('POST', '/storage/v1/object/size-limit-bucket/toobig.bin', {
    headers: { ...svcHdr(), 'Content-Type': 'application/octet-stream' },
    body: Buffer.alloc(200, 0xab),  // 200 bytes > 100-byte limit
  })
  assert(status === 413, `want 413 got ${status}`)
})

step('upload within bucket file_size_limit succeeds', async () => {
  const { status } = await api('POST', '/storage/v1/object/size-limit-bucket/ok.bin', {
    headers: { ...svcHdr(), 'Content-Type': 'application/octet-stream' },
    body: Buffer.alloc(50, 0xcd),  // 50 bytes < 100-byte limit
  })
  assert(status === 200, `want 200 got ${status}`)
})

// ── Per-bucket MIME type enforcement ─────────────────────────────────────────

step('create bucket restricted to image/png', async () => {
  const { status, data } = await api('POST', '/storage/v1/bucket', {
    headers: svcHdr(true),
    body: { id: 'mime-strict-bucket', name: 'mime-strict-bucket', public: false, allowed_mime_types: ['image/png'] },
  })
  assert(status === 200, `want 200 got ${status}: ${JSON.stringify(data)}`)
})

step('upload wrong MIME type to restricted bucket returns 400', async () => {
  const { status, data } = await api('POST', '/storage/v1/object/mime-strict-bucket/doc.txt', {
    headers: { ...svcHdr(), 'Content-Type': 'text/plain' },
    body: 'hello',
  })
  assert(status === 400, `want 400 got ${status}: ${JSON.stringify(data)}`)
})

step('upload correct MIME type to restricted bucket succeeds', async () => {
  const { status } = await api('POST', '/storage/v1/object/mime-strict-bucket/img.png', {
    headers: { ...svcHdr(), 'Content-Type': 'image/png' },
    body: Buffer.from([137, 80, 78, 71]),
  })
  assert(status === 200, `want 200 got ${status}`)
})

step('MIME parameter suffix is ignored in match (image/png; quality=85 matches image/png)', async () => {
  const { status } = await api('PUT', '/storage/v1/object/mime-strict-bucket/img2.png', {
    headers: { ...svcHdr(), 'Content-Type': 'image/png; quality=85' },
    body: Buffer.from([137, 80, 78, 71]),
  })
  assert(status === 200, `want 200 got ${status}`)
})

step('create bucket with image/* wildcard MIME type', async () => {
  const { status } = await api('POST', '/storage/v1/bucket', {
    headers: svcHdr(true),
    body: { id: 'mime-wildcard-bucket', name: 'mime-wildcard-bucket', public: false, allowed_mime_types: ['image/*'] },
  })
  assert(status === 200, `want 200 got ${status}`)
})

step('image/jpeg matches image/* wildcard and succeeds', async () => {
  const { status } = await api('POST', '/storage/v1/object/mime-wildcard-bucket/photo.jpg', {
    headers: { ...svcHdr(), 'Content-Type': 'image/jpeg' },
    body: Buffer.from([255, 216, 255]),
  })
  assert(status === 200, `want 200 got ${status}`)
})

step('text/plain does not match image/* wildcard, returns 400', async () => {
  const { status } = await api('POST', '/storage/v1/object/mime-wildcard-bucket/doc.txt', {
    headers: { ...svcHdr(), 'Content-Type': 'text/plain' },
    body: 'not an image',
  })
  assert(status === 400, `want 400 got ${status}`)
})

// ── RLS: authenticated users see only their own objects ───────────────────────

step('create private RLS test bucket', async () => {
  const { status } = await api('POST', '/storage/v1/bucket', {
    headers: svcHdr(true),
    body: { id: 'rls-bucket', name: 'rls-bucket', public: false },
  })
  assert(status === 200, `want 200 got ${status}`)
})

step('user-A uploads their own object (owner = user-A UUID)', async () => {
  const { status } = await api('POST', '/storage/v1/object/rls-bucket/user-a-file.txt', {
    headers: { Authorization: `Bearer ${userAToken}`, 'Content-Type': 'text/plain' },
    body: 'user a data',
  })
  assert(status === 200, `want 200 got ${status}`)
})

step('user-A can GET their own object', async () => {
  const res = await api('GET', '/storage/v1/object/rls-bucket/user-a-file.txt', {
    headers: { Authorization: `Bearer ${userAToken}` },
    raw: true,
  })
  assert(res.status === 200, `want 200 got ${res.status}`)
})

step('user-B cannot GET user-A object in private bucket (RLS blocks)', async () => {
  const { status } = await api('GET', '/storage/v1/object/rls-bucket/user-a-file.txt', {
    headers: { Authorization: `Bearer ${userBToken}` },
    raw: true,
  })
  assert(status === 403, `want 403 got ${status}`)
})

// ── sort_by and search ────────────────────────────────────────────────────────

step('list with sort_by created_at desc returns newest first', async () => {
  // Upload two objects into size-limit-bucket (already exists from earlier tests)
  await api('POST', '/storage/v1/object/size-limit-bucket/sort-a.txt', {
    headers: { ...svcHdr(), 'Content-Type': 'text/plain' },
    body: 'aaa',
  })
  await api('POST', '/storage/v1/object/size-limit-bucket/sort-b.txt', {
    headers: { ...svcHdr(), 'Content-Type': 'text/plain' },
    body: 'bbb',
  })
  const { status, data } = await api('POST', '/storage/v1/object/list/size-limit-bucket', {
    headers: svcHdr(true),
    body: { prefix: 'sort-', sort_by: { column: 'created_at', order: 'desc' } },
  })
  assert(status === 200, `want 200 got ${status}`)
  assert(Array.isArray(data) && data.length >= 2, 'want at least 2 objects')
  // Most recently created first
  const names = data.map(o => o.name)
  const idxA = names.indexOf('sort-a.txt')
  const idxB = names.indexOf('sort-b.txt')
  assert(idxB < idxA, `sort-b should be before sort-a (desc by created_at): ${names}`)
})

step('list with search param filters by substring', async () => {
  const { status, data } = await api('POST', '/storage/v1/object/list/size-limit-bucket', {
    headers: svcHdr(true),
    body: { prefix: '', search: 'sort-' },
  })
  assert(status === 200, `want 200 got ${status}`)
  assert(Array.isArray(data), 'want array')
  assert(data.every(o => o.name.includes('sort-')), `all results should match search: ${JSON.stringify(data.map(o => o.name))}`)
})

// ── HEAD /object ──────────────────────────────────────────────────────────────

step('HEAD /object returns content-type and content-length, no body', async () => {
  const res = await fetch(`${BASE}/storage/v1/object/size-limit-bucket/sort-a.txt`, {
    method: 'HEAD',
    headers: svcHdr(),
  })
  assert(res.status === 200, `want 200 got ${res.status}`)
  assert(res.headers.get('content-type') === 'text/plain', `want text/plain got ${res.headers.get('content-type')}`)
  assert(res.headers.get('content-length') === '3', `want 3 got ${res.headers.get('content-length')}`)
  const body = await res.text()
  assert(body === '', `want empty body on HEAD, got: ${body}`)
})

step('HEAD /object has all 5 standard headers with correct formats', async () => {
  const res = await fetch(`${BASE}/storage/v1/object/size-limit-bucket/sort-a.txt`, {
    method: 'HEAD',
    headers: svcHdr(),
  })
  assert(res.status === 200, `want 200 got ${res.status}`)

  // Content-Type
  assert(res.headers.get('content-type') === 'text/plain', `content-type: ${res.headers.get('content-type')}`)

  // Content-Length — sort-a.txt body is 'aaa' = 3 bytes
  assert(res.headers.get('content-length') === '3', `content-length: ${res.headers.get('content-length')}`)

  // Cache-Control — size-limit-bucket has no cache_control set, expect no-cache default
  assert(res.headers.get('cache-control') === 'no-cache', `cache-control: ${res.headers.get('cache-control')}`)

  // ETag — S3/MinIO returns a quoted MD5 hex string e.g. "47bce5c..."
  const etag = res.headers.get('etag')
  assert(etag, 'want etag header')
  assert(etag.startsWith('"') && etag.endsWith('"'), `etag should be a quoted string: ${etag}`)

  // Last-Modified — must be a valid HTTP date (parseable by Date)
  const lm = res.headers.get('last-modified')
  assert(lm, 'want last-modified header')
  assert(!isNaN(Date.parse(lm)), `last-modified not a valid date: ${lm}`)

  // Body must be empty on HEAD
  const body = await res.text()
  assert(body === '', `want empty body on HEAD, got: ${body}`)
})

step('GET /object ETag and Last-Modified match HEAD for same object', async () => {
  const [getRes, headRes] = await Promise.all([
    fetch(`${BASE}/storage/v1/object/size-limit-bucket/sort-a.txt`, { headers: svcHdr() }),
    fetch(`${BASE}/storage/v1/object/size-limit-bucket/sort-a.txt`, { method: 'HEAD', headers: svcHdr() }),
  ])
  assert(getRes.status === 200, `GET want 200 got ${getRes.status}`)
  assert(headRes.status === 200, `HEAD want 200 got ${headRes.status}`)

  const getEtag = getRes.headers.get('etag')
  const headEtag = headRes.headers.get('etag')
  assert(getEtag, 'want etag on GET')
  assert(getEtag === headEtag, `ETag mismatch GET vs HEAD: ${getEtag} vs ${headEtag}`)

  const getLm = getRes.headers.get('last-modified')
  const headLm = headRes.headers.get('last-modified')
  assert(getLm, 'want last-modified on GET')
  assert(getLm === headLm, `Last-Modified mismatch GET vs HEAD: ${getLm} vs ${headLm}`)
})

step('HEAD /object on missing object returns 404', async () => {
  const res = await fetch(`${BASE}/storage/v1/object/size-limit-bucket/nonexistent.txt`, {
    method: 'HEAD',
    headers: svcHdr(),
  })
  assert(res.status === 404, `want 404 got ${res.status}`)
})

// ── cache_control on downloads ────────────────────────────────────────────────

step('create bucket with cacheControl', async () => {
  const { status } = await api('POST', '/storage/v1/bucket', {
    headers: svcHdr(true),
    body: { id: 'cached-bucket', name: 'cached-bucket', public: true, cache_control: 'max-age=3600, public' },
  })
  assert(status === 200, `want 200 got ${status}`)
})

step('download from cacheControl bucket passes Cache-Control header', async () => {
  await api('POST', '/storage/v1/object/cached-bucket/logo.png', {
    headers: { ...svcHdr(), 'Content-Type': 'image/png' },
    body: Buffer.from([137, 80, 78, 71]),
  })
  const res = await fetch(`${BASE}/storage/v1/object/public/cached-bucket/logo.png`)
  assert(res.status === 200, `want 200 got ${res.status}`)
  assert(
    res.headers.get('cache-control') === 'max-age=3600, public',
    `want max-age=3600, public got ${res.headers.get('cache-control')}`
  )
})

step('update bucket cacheControl', async () => {
  const { status, data } = await api('PATCH', '/storage/v1/bucket/cached-bucket', {
    headers: svcHdr(true),
    body: { cache_control: 'no-store' },
  })
  assert(status === 200, `want 200 got ${status}`)
  assert(data.cache_control === 'no-store', `want no-store got ${data.cache_control}`)
})

// ── HEAD /object/public ───────────────────────────────────────────────────────

step('HEAD /object/public returns metadata without body', async () => {
  // cached-bucket is public; logo.png was uploaded in the cacheControl test above
  const res = await fetch(`${BASE}/storage/v1/object/public/cached-bucket/logo.png`, { method: 'HEAD' })
  assert(res.status === 200, `want 200 got ${res.status}`)
  assert(res.headers.get('content-type') === 'image/png', `want image/png got ${res.headers.get('content-type')}`)
  assert(Number(res.headers.get('content-length')) > 0, 'want content-length > 0')
  const body = await res.text()
  assert(body === '', `want empty body on HEAD, got: ${body}`)
})

step('HEAD /object/public on private bucket returns 403', async () => {
  const res = await fetch(`${BASE}/storage/v1/object/public/private-bucket/docs/readme.md`, { method: 'HEAD' })
  assert(res.status === 403, `want 403 got ${res.status}`)
})

// ── render/image routes (fallthrough — IMGPROXY_URL not set in test env) ─────

step('GET /render/image/authenticated falls through to raw download', async () => {
  const res = await api('GET', '/storage/v1/render/image/authenticated/cached-bucket/logo.png', {
    headers: svcHdr(),
    raw: true,
  })
  assert(res.status === 200, `want 200 got ${res.status}`)
  const bytes = await res.arrayBuffer()
  assert(bytes.byteLength === 4, `want 4 bytes got ${bytes.byteLength}`)
})

step('GET /render/image/public falls through to raw download on public bucket', async () => {
  const res = await fetch(`${BASE}/storage/v1/render/image/public/cached-bucket/logo.png`)
  assert(res.status === 200, `want 200 got ${res.status}`)
})

step('GET /render/image/public on private bucket returns 403', async () => {
  const { status } = await api('GET', '/storage/v1/render/image/public/private-bucket/docs/readme.md', { raw: true })
  assert(status === 403, `want 403 got ${status}`)
})

step('GET /render/image/authenticated with wrong user token returns 403 (RLS on private bucket)', async () => {
  const { status } = await api('GET', '/storage/v1/render/image/authenticated/rls-bucket/user-a-file.txt', {
    headers: { Authorization: `Bearer ${userBToken}` },
    raw: true,
  })
  assert(status === 403, `want 403 for wrong user on private bucket, got ${status}`)
})

// ── Copy / Move auth enforcement ──────────────────────────────────────────────

step('non-service-role cannot copy objects', async () => {
  const { status } = await api('POST', '/storage/v1/object/copy', {
    headers: { Authorization: `Bearer ${userAToken}`, 'Content-Type': 'application/json' },
    body: {
      bucketId: 'size-limit-bucket', sourceKey: 'sort-a.txt',
      destinationBucket: 'private-bucket', destinationKey: 'should-not-exist.txt',
    },
  })
  assert(status === 403, `want 403 got ${status}`)
})

step('non-service-role cannot move objects', async () => {
  const { status } = await api('POST', '/storage/v1/object/move', {
    headers: { Authorization: `Bearer ${userAToken}`, 'Content-Type': 'application/json' },
    body: {
      bucketId: 'private-bucket', sourceKey: 'moved.txt',
      destinationBucket: 'private-bucket', destinationKey: 'should-not-exist.txt',
    },
  })
  assert(status === 403, `want 403 got ${status}`)
})

// ── Bulk delete auth enforcement ──────────────────────────────────────────────

step('anon cannot bulk delete', async () => {
  const { status } = await api('DELETE', '/storage/v1/object/private-bucket', {
    headers: { 'Content-Type': 'application/json' },
    body: { prefixes: ['bulk-c.txt'] },
  })
  assert(status === 401, `want 401 got ${status}`)
})

// ── Signed upload respects bucket restrictions ────────────────────────────────

step('signed upload into MIME-restricted bucket rejects wrong content-type', async () => {
  // mime-strict-bucket allows only image/png
  const { status: signStatus, data: signData } = await api(
    'POST', '/storage/v1/object/upload/sign/mime-strict-bucket/wrong.txt',
    { headers: svcHdr(true), body: { expires_in: 300 } }
  )
  assert(signStatus === 200, `could not create upload token: ${signStatus}`)

  const res = await fetch(`${BASE}${signData.url}`, {
    method: 'PUT',
    headers: { 'Content-Type': 'text/plain' },
    body: 'should be rejected',
  })
  assert(res.status === 400, `want 400 for wrong MIME on signed upload, got ${res.status}`)
})

step('signed upload into size-limited bucket rejects oversized body', async () => {
  // size-limit-bucket has a 100-byte limit
  const { status: signStatus, data: signData } = await api(
    'POST', '/storage/v1/object/upload/sign/size-limit-bucket/toobig-signed.bin',
    { headers: svcHdr(true), body: { expires_in: 300 } }
  )
  assert(signStatus === 200, `could not create upload token: ${signStatus}`)

  const res = await fetch(`${BASE}${signData.url}`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/octet-stream' },
    body: Buffer.alloc(200, 0xff),
  })
  assert(res.status === 413, `want 413 for oversized signed upload, got ${res.status}`)
})

// ── Bulk delete ───────────────────────────────────────────────────────────────

step('bulk delete removes multiple objects', async () => {
  // Upload 3 objects
  for (const name of ['bulk-a.txt', 'bulk-b.txt', 'bulk-c.txt']) {
    await api('POST', `/storage/v1/object/private-bucket/${name}`, {
      headers: { ...svcHdr(), 'Content-Type': 'text/plain' },
      body: 'data',
    })
  }
  // Bulk delete 2 of them
  const { status, data } = await api('DELETE', '/storage/v1/object/private-bucket', {
    headers: svcHdr(true),
    body: { prefixes: ['bulk-a.txt', 'bulk-b.txt'] },
  })
  assert(status === 200, `want 200 got ${status}: ${JSON.stringify(data)}`)
  assert(Array.isArray(data), 'want array')
  const names = data.map(d => d.name).sort()
  assert(names[0] === 'bulk-a.txt' && names[1] === 'bulk-b.txt', `wrong deleted names: ${names}`)
  // bulk-c.txt should still exist
  const check = await api('GET', '/storage/v1/object/private-bucket/bulk-c.txt', { headers: svcHdr(), raw: true })
  assert(check.status === 200, `bulk-c.txt should still exist, got ${check.status}`)
})

step('bulk delete with empty prefixes returns empty array', async () => {
  const { status, data } = await api('DELETE', '/storage/v1/object/private-bucket', {
    headers: svcHdr(true),
    body: { prefixes: [] },
  })
  assert(status === 200, `want 200 got ${status}`)
  assert(Array.isArray(data) && data.length === 0, 'want empty array')
})

// ── Copy and Move ─────────────────────────────────────────────────────────────

step('copy object to another bucket', async () => {
  // src already exists: size-limit-bucket/sort-a.txt
  const { status, data } = await api('POST', '/storage/v1/object/copy', {
    headers: svcHdr(true),
    body: {
      bucketId:          'size-limit-bucket',
      sourceKey:         'sort-a.txt',
      destinationBucket: 'private-bucket',
      destinationKey:    'copied-sort-a.txt',
    },
  })
  assert(status === 200, `want 200 got ${status}: ${JSON.stringify(data)}`)
  assert(data.Key === 'private-bucket/copied-sort-a.txt', `Key mismatch: ${data.Key}`)
  // Source still exists
  const src = await api('GET', '/storage/v1/object/size-limit-bucket/sort-a.txt', { headers: svcHdr(), raw: true })
  assert(src.status === 200, `source should still exist after copy: ${src.status}`)
  // Destination exists
  const dst = await api('GET', '/storage/v1/object/private-bucket/copied-sort-a.txt', { headers: svcHdr(), raw: true })
  assert(dst.status === 200, `destination should exist after copy: ${dst.status}`)
})

step('move object removes source', async () => {
  // Upload a fresh file to move
  await api('POST', '/storage/v1/object/private-bucket/to-move.txt', {
    headers: { ...svcHdr(), 'Content-Type': 'text/plain' },
    body: 'move me',
  })
  const { status, data } = await api('POST', '/storage/v1/object/move', {
    headers: svcHdr(true),
    body: {
      bucketId:          'private-bucket',
      sourceKey:         'to-move.txt',
      destinationBucket: 'private-bucket',
      destinationKey:    'moved.txt',
    },
  })
  assert(status === 200, `want 200 got ${status}: ${JSON.stringify(data)}`)
  // Source gone
  const src = await api('GET', '/storage/v1/object/private-bucket/to-move.txt', { headers: svcHdr(), raw: true })
  assert(src.status === 404, `source should be gone after move, got ${src.status}`)
  // Destination exists with same content
  const dst = await fetch(`${BASE}/storage/v1/object/private-bucket/moved.txt`, { headers: svcHdr() })
  assert(dst.status === 200, `destination should exist after move: ${dst.status}`)
  assert(await dst.text() === 'move me', 'content mismatch after move')
})

step('copy missing object returns 404', async () => {
  const { status } = await api('POST', '/storage/v1/object/copy', {
    headers: svcHdr(true),
    body: {
      bucketId:          'private-bucket',
      sourceKey:         'does-not-exist.txt',
      destinationBucket: 'private-bucket',
      destinationKey:    'nowhere.txt',
    },
  })
  assert(status === 404, `want 404 got ${status}`)
})

// ── Multi-path signed URLs ────────────────────────────────────────────────────

step('create multiple signed URLs in one request', async () => {
  // Ensure objects exist in cached-bucket (public)
  await api('PUT', '/storage/v1/object/cached-bucket/a.txt', {
    headers: { ...svcHdr(), 'Content-Type': 'text/plain' },
    body: 'aaa',
  })
  await api('PUT', '/storage/v1/object/cached-bucket/b.txt', {
    headers: { ...svcHdr(), 'Content-Type': 'text/plain' },
    body: 'bbb',
  })

  const { status, data } = await api('POST', '/storage/v1/object/sign/cached-bucket', {
    headers: svcHdr(true),
    body: { paths: ['a.txt', 'b.txt'], expires_in: 300 },
  })
  assert(status === 200, `want 200 got ${status}: ${JSON.stringify(data)}`)
  assert(Array.isArray(data) && data.length === 2, 'want 2 signed URLs')
  assert(data[0].signedURL && data[1].signedURL, 'want signedURL in each entry')
  assert(data[0].path === 'a.txt' && data[1].path === 'b.txt', `path mismatch: ${JSON.stringify(data)}`)

  // Both tokens should work
  for (const entry of data) {
    const res = await fetch(`${BASE}${entry.signedURL}`)
    assert(res.status === 200, `signed URL for ${entry.path} failed: ${res.status}`)
  }
})

// ── Signed upload URLs ────────────────────────────────────────────────────────

let uploadSignedUrl = ''
let uploadToken = ''

step('create signed upload URL', async () => {
  const { status, data } = await api('POST', '/storage/v1/object/upload/sign/private-bucket/signed-upload.txt', {
    headers: svcHdr(true),
    body: { expires_in: 300 },
  })
  assert(status === 200, `want 200 got ${status}: ${JSON.stringify(data)}`)
  assert(data.url, 'want url in response')
  assert(data.token, 'want token in response')
  uploadSignedUrl = data.url
  uploadToken = data.token
})

step('upload via signed upload URL succeeds', async () => {
  const res = await fetch(`${BASE}${uploadSignedUrl}`, {
    method: 'PUT',
    headers: { 'Content-Type': 'text/plain' },
    body: 'signed upload content',
  })
  assert(res.status === 200, `want 200 got ${res.status}`)
})

step('uploaded object via signed URL is retrievable', async () => {
  const res = await api('GET', '/storage/v1/object/private-bucket/signed-upload.txt', {
    headers: svcHdr(),
    raw: true,
  })
  assert(res.status === 200, `want 200 got ${res.status}`)
  assert(await res.text() === 'signed upload content', 'content mismatch')
})

step('signed upload token cannot be used for download', async () => {
  const { status } = await api('GET', `/storage/v1/object/sign/private-bucket/signed-upload.txt?token=${uploadToken}`, {
    raw: true,
  })
  assert(status === 401, `upload token should be rejected as download token, got ${status}`)
})

step('anon cannot create signed upload URL', async () => {
  const { status } = await api('POST', '/storage/v1/object/upload/sign/private-bucket/nope.txt', {
    headers: { 'Content-Type': 'application/json' },
    body: { expires_in: 60 },
  })
  assert(status === 401, `want 401 got ${status}`)
})

// ── Lifecycle rules ───────────────────────────────────────────────────────────

let lifecycleRuleId = ''

step('create lifecycle rule on bucket', async () => {
  const { status, data } = await api('POST', '/storage/v1/bucket/private-bucket/lifecycle', {
    headers: svcHdr(true),
    body: { prefix: 'tmp/', expires_days: 7 },
  })
  assert(status === 200, `want 200 got ${status}: ${JSON.stringify(data)}`)
  assert(data.id, 'want id')
  assert(data.expires_days === 7, `want 7 got ${data.expires_days}`)
  assert(data.prefix === 'tmp/', `want tmp/ got ${data.prefix}`)
  lifecycleRuleId = data.id
})

step('list lifecycle rules returns created rule', async () => {
  const { status, data } = await api('GET', '/storage/v1/bucket/private-bucket/lifecycle', {
    headers: svcHdr(),
  })
  assert(status === 200, `want 200 got ${status}`)
  assert(Array.isArray(data), 'want array')
  assert(data.some(r => r.id === lifecycleRuleId), 'created rule not in list')
})

step('create lifecycle rule with expires_days=0 returns 400', async () => {
  const { status } = await api('POST', '/storage/v1/bucket/private-bucket/lifecycle', {
    headers: svcHdr(true),
    body: { expires_days: 0 },
  })
  assert(status === 400, `want 400 got ${status}`)
})

step('delete lifecycle rule', async () => {
  const { status } = await api('DELETE', `/storage/v1/bucket/private-bucket/lifecycle/${lifecycleRuleId}`, {
    headers: svcHdr(),
  })
  assert(status === 200, `want 200 got ${status}`)
})

step('lifecycle rule gone after delete', async () => {
  const { status, data } = await api('GET', '/storage/v1/bucket/private-bucket/lifecycle', {
    headers: svcHdr(),
  })
  assert(status === 200)
  assert(!data.some(r => r.id === lifecycleRuleId), 'rule should be gone')
})

step('delete non-existent lifecycle rule returns 404', async () => {
  const { status } = await api('DELETE', '/storage/v1/bucket/private-bucket/lifecycle/00000000-0000-0000-0000-000000000000', {
    headers: svcHdr(),
  })
  assert(status === 404, `want 404 got ${status}`)
})

step('lifecycle rule on missing bucket returns 404', async () => {
  const { status } = await api('POST', '/storage/v1/bucket/no-such-bucket/lifecycle', {
    headers: svcHdr(true),
    body: { expires_days: 1 },
  })
  assert(status === 404, `want 404 got ${status}`)
})

// ── Isolation between users ───────────────────────────────────────────────────
// One end user must never change, remove, list or hand out another's objects.
// User A owns the objects below; user B (and anon) try every way in.

const hdr = (token, ct) => ({ Authorization: `Bearer ${token}`, ...(ct ? { 'Content-Type': ct } : {}) })
const readAs = async (token, path) => {
  const res = await api('GET', `/storage/v1/object/${path}`, { headers: hdr(token), raw: true })
  return { status: res.status, text: res.status === 200 ? await res.text() : '' }
}
const aStillHas = async (path, text) => {
  const r = await readAs(userAToken, path)
  assert(r.status === 200 && r.text === text, `A's ${path} changed: ${r.status} ${JSON.stringify(r.text)}`)
}

step('isolation: setup — private and public buckets, A uploads to each', async () => {
  for (const [id, pub] of [['iso-private', false], ['iso-public', true]]) {
    const { status } = await api('POST', '/storage/v1/bucket', { headers: svcHdr(true), body: { id, name: id, public: pub } })
    assert(status === 200, `create ${id}: ${status}`)
  }
  for (const path of ['iso-private/a.txt', 'iso-public/p.txt']) {
    const { status, data } = await api('POST', `/storage/v1/object/${path}`, { headers: hdr(userAToken, 'text/plain'), body: `A's ${path}` })
    assert(status === 200, `A upload ${path}: ${status} ${JSON.stringify(data)}`)
  }
  await aStillHas('iso-private/a.txt', "A's iso-private/a.txt")
})

step('isolation: B cannot read A\'s private object', async () => {
  const r = await readAs(userBToken, 'iso-private/a.txt')
  assert(r.status === 403, `want 403 got ${r.status}`)
})

step('isolation: B cannot overwrite A\'s object (POST 409, PUT 403), bytes untouched', async () => {
  const post = await api('POST', '/storage/v1/object/iso-private/a.txt', { headers: hdr(userBToken, 'text/plain'), body: 'B was here' })
  assert(post.status === 409, `POST want 409 got ${post.status}`)
  const put = await api('PUT', '/storage/v1/object/iso-private/a.txt', { headers: hdr(userBToken, 'text/plain'), body: 'B was here' })
  assert(put.status === 403, `PUT want 403 got ${put.status}`)
  const pubPut = await api('PUT', '/storage/v1/object/iso-public/p.txt', { headers: hdr(userBToken, 'text/plain'), body: 'B was here' })
  assert(pubPut.status === 403, `PUT on a public bucket want 403 got ${pubPut.status}`)
  await aStillHas('iso-private/a.txt', "A's iso-private/a.txt")
  await aStillHas('iso-public/p.txt', "A's iso-public/p.txt")
})

step('isolation: B cannot delete A\'s object, singly or in bulk', async () => {
  const one = await api('DELETE', '/storage/v1/object/iso-private/a.txt', { headers: hdr(userBToken) })
  assert(one.status === 404, `DELETE want 404 got ${one.status}`)
  const pub = await api('DELETE', '/storage/v1/object/iso-public/p.txt', { headers: hdr(userBToken) })
  assert(pub.status === 404, `DELETE on a public bucket want 404 got ${pub.status}`)
  const bulk = await api('DELETE', '/storage/v1/object/iso-private', { headers: hdr(userBToken, 'application/json'), body: { prefixes: ['a.txt'] } })
  assert(bulk.status === 200 && Array.isArray(bulk.data) && bulk.data.length === 0, `bulk want [] got ${bulk.status} ${JSON.stringify(bulk.data)}`)
  await aStillHas('iso-private/a.txt', "A's iso-private/a.txt")
  await aStillHas('iso-public/p.txt', "A's iso-public/p.txt")
})

step('isolation: listing a private bucket shows B nothing of A\'s, and A their own', async () => {
  const b = await api('POST', '/storage/v1/object/list/iso-private', { headers: hdr(userBToken, 'application/json'), body: { prefix: '' } })
  assert(b.status === 200 && b.data.length === 0, `B sees ${JSON.stringify(b.data)}`)
  const a = await api('POST', '/storage/v1/object/list/iso-private', { headers: hdr(userAToken, 'application/json'), body: { prefix: '' } })
  assert(a.status === 200 && a.data.some(o => o.name === 'a.txt'), `A sees ${JSON.stringify(a.data)}`)
  const anon = await api('POST', '/storage/v1/object/list/iso-private', { headers: hdr(anonToken, 'application/json'), body: { prefix: '' } })
  assert(anon.status === 200 && anon.data.length === 0, `anon sees ${JSON.stringify(anon.data)}`)
})

step('isolation: no signed download URL for an object the caller cannot read', async () => {
  const b = await api('POST', '/storage/v1/object/sign/iso-private/a.txt', { headers: hdr(userBToken, 'application/json'), body: { expires_in: 60 } })
  assert(b.status === 403, `B want 403 got ${b.status}`)
  const anon = await api('POST', '/storage/v1/object/sign/iso-private/a.txt', { headers: { 'Content-Type': 'application/json' }, body: { expires_in: 60 } })
  assert(anon.status === 403, `no token want 403 got ${anon.status}`)
  const multi = await api('POST', '/storage/v1/object/sign/iso-private', { headers: hdr(userBToken, 'application/json'), body: { paths: ['a.txt'], expires_in: 60 } })
  assert(multi.status === 200 && multi.data[0].signedURL === null && multi.data[0].error, `multi gave B ${JSON.stringify(multi.data)}`)
  const a = await api('POST', '/storage/v1/object/sign/iso-private/a.txt', { headers: hdr(userAToken, 'application/json'), body: { expires_in: 60 } })
  assert(a.status === 200 && a.data.signedURL, `A want a URL got ${a.status}`)
  const res = await fetch(`${BASE}${a.data.signedURL}`)
  assert(res.status === 200 && await res.text() === "A's iso-private/a.txt", `A's signed URL: ${res.status}`)
})

step('isolation: signed upload URLs — none for A\'s path to B; A\'s writes as A', async () => {
  const b = await api('POST', '/storage/v1/object/upload/sign/iso-private/a.txt', { headers: hdr(userBToken, 'application/json'), body: { expires_in: 60 } })
  assert(b.status === 403, `B want 403 got ${b.status}`)
  const a = await api('POST', '/storage/v1/object/upload/sign/iso-private/a-signed.txt', { headers: hdr(userAToken, 'application/json'), body: { expires_in: 60 } })
  assert(a.status === 200, `A want 200 got ${a.status}`)
  const up = await fetch(`${BASE}${a.data.url}`, { method: 'PUT', headers: { 'Content-Type': 'text/plain' }, body: 'via signed URL' })
  assert(up.status === 200, `upload via A's URL: ${up.status}`)
  await aStillHas('iso-private/a-signed.txt', 'via signed URL')
  const r = await readAs(userBToken, 'iso-private/a-signed.txt')
  assert(r.status === 403, `the object is A's, B want 403 got ${r.status}`)
})

const tusMeta = (bucket, name) =>
  [['bucketName', bucket], ['objectName', name], ['contentType', 'text/plain']]
    .map(([k, v]) => `${k} ${Buffer.from(v).toString('base64')}`).join(',')
const tusCreate = (token, bucket, name, length) =>
  fetch(`${BASE}/storage/v1/upload/resumable`, {
    method: 'POST',
    headers: { ...hdr(token), 'Tus-Resumable': '1.0.0', 'Upload-Length': String(length), 'Upload-Metadata': tusMeta(bucket, name) },
  })
const tusPatch = (token, location, offset, body) =>
  fetch(`${BASE}${location}`, {
    method: 'PATCH',
    headers: { ...hdr(token), 'Tus-Resumable': '1.0.0', 'Content-Type': 'application/offset+octet-stream', 'Upload-Offset': String(offset) },
    body,
  })

step('isolation: B cannot start a resumable upload onto A\'s object', async () => {
  const res = await tusCreate(userBToken, 'iso-private', 'a.txt', 10)
  assert(res.status === 403, `want 403 got ${res.status}`)
  await aStillHas('iso-private/a.txt', "A's iso-private/a.txt")
})

step('isolation: A\'s resumable upload is A\'s alone, and makes A\'s object', async () => {
  const body = "A's resumable upload"
  const created = await tusCreate(userAToken, 'iso-private', 'a-tus.txt', body.length)
  assert(created.status === 201, `create want 201 got ${created.status}`)
  const location = created.headers.get('location')
  const head = await fetch(`${BASE}${location}`, { method: 'HEAD', headers: { ...hdr(userBToken), 'Tus-Resumable': '1.0.0' } })
  assert(head.status === 404, `B HEAD want 404 got ${head.status}`)
  const bPatch = await tusPatch(userBToken, location, 0, 'B was here')
  assert(bPatch.status === 404, `B PATCH want 404 got ${bPatch.status}`)
  const bAbort = await fetch(`${BASE}${location}`, { method: 'DELETE', headers: { ...hdr(userBToken), 'Tus-Resumable': '1.0.0' } })
  assert(bAbort.status === 404, `B DELETE want 404 got ${bAbort.status}`)
  const aPatch = await tusPatch(userAToken, location, 0, body)
  assert(aPatch.status === 204, `A PATCH want 204 got ${aPatch.status}`)
  await aStillHas('iso-private/a-tus.txt', body)
  const r = await readAs(userBToken, 'iso-private/a-tus.txt')
  assert(r.status === 403, `B want 403 got ${r.status}`)
})

step('isolation: A can replace and delete their own objects', async () => {
  const put = await api('PUT', '/storage/v1/object/iso-private/a.txt', { headers: hdr(userAToken, 'text/plain'), body: 'A replaced it' })
  assert(put.status === 200, `A PUT want 200 got ${put.status}`)
  await aStillHas('iso-private/a.txt', 'A replaced it')
  for (const path of ['iso-private/a.txt', 'iso-private/a-signed.txt', 'iso-private/a-tus.txt', 'iso-public/p.txt']) {
    const del = await api('DELETE', `/storage/v1/object/${path}`, { headers: hdr(userAToken) })
    assert(del.status === 200, `A DELETE ${path} want 200 got ${del.status}`)
  }
})

run().catch(e => { console.error(e); process.exit(1) })
