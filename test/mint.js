const { createHmac } = require('crypto')
const secret = process.env.PGRST_JWT_SECRET || 'test-jwt-secret-32bytes-padded-ok'
const role   = process.argv[2] || 'service_role'
const sub    = process.argv[3] || 'system'
const b64    = b => b.toString('base64').replace(/\+/g,'-').replace(/\//g,'_').replace(/=/g,'')
const h = b64(Buffer.from(JSON.stringify({ alg: 'HS256', typ: 'JWT' })))
const p = b64(Buffer.from(JSON.stringify({ role, sub, iat: Math.floor(Date.now()/1000) })))
const s = b64(createHmac('sha256', secret).update(`${h}.${p}`).digest())
console.log(`${h}.${p}.${s}`)