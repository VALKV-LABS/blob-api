use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::error::{Result, StorageError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedUrlClaims {
    /// "{bucket_id}/{object_name}"
    pub url: String,
    pub exp: u64,
    pub iat: u64,
    /// "download" | "upload" — defaults to "download" for tokens minted before
    /// this field was added.
    #[serde(default = "default_download")]
    pub kind: String,
}

fn default_download() -> String {
    "download".into()
}

// ── Download signed URLs ──────────────────────────────────────────────────────

pub fn create_token(
    bucket_id: &str,
    object_name: &str,
    expires_in_secs: u64,
    secret: &str,
) -> Result<String> {
    let now = unix_now();
    let claims = SignedUrlClaims {
        url: format!("{}/{}", bucket_id, object_name),
        exp: now + expires_in_secs,
        iat: now,
        kind: "download".into(),
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| StorageError::Internal(anyhow::anyhow!("sign error: {}", e)))
}

pub fn verify_token(
    token: &str,
    bucket_id: &str,
    object_name: &str,
    secret: &str,
) -> Result<SignedUrlClaims> {
    let claims = decode_claims(token, secret)?;
    if claims.kind != "download" {
        return Err(StorageError::Unauthorized);
    }
    if claims.url != format!("{}/{}", bucket_id, object_name) {
        return Err(StorageError::Unauthorized);
    }
    Ok(claims)
}

// ── Upload signed URLs ────────────────────────────────────────────────────────

pub fn create_upload_token(
    bucket_id: &str,
    object_name: &str,
    expires_in_secs: u64,
    secret: &str,
) -> Result<String> {
    let now = unix_now();
    let claims = SignedUrlClaims {
        url: format!("{}/{}", bucket_id, object_name),
        exp: now + expires_in_secs,
        iat: now,
        kind: "upload".into(),
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| StorageError::Internal(anyhow::anyhow!("sign error: {}", e)))
}

pub fn verify_upload_token(
    token: &str,
    bucket_id: &str,
    object_name: &str,
    secret: &str,
) -> Result<SignedUrlClaims> {
    let claims = decode_claims(token, secret)?;
    if claims.kind != "upload" {
        return Err(StorageError::Unauthorized);
    }
    if claims.url != format!("{}/{}", bucket_id, object_name) {
        return Err(StorageError::Unauthorized);
    }
    Ok(claims)
}

// ── Shared ────────────────────────────────────────────────────────────────────

fn decode_claims(token: &str, secret: &str) -> Result<SignedUrlClaims> {
    let mut v = Validation::new(Algorithm::HS256);
    v.validate_exp = true;
    v.required_spec_claims.clear();

    decode::<SignedUrlClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &v,
    )
    .map(|d| d.claims)
    .map_err(|_| StorageError::Unauthorized)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-jwt-secret";

    #[test]
    fn download_roundtrip() {
        let tok = create_token("mybucket", "path/to/file.png", 120, SECRET).unwrap();
        let c = verify_token(&tok, "mybucket", "path/to/file.png", SECRET).unwrap();
        assert_eq!(c.url, "mybucket/path/to/file.png");
        assert_eq!(c.kind, "download");
    }

    #[test]
    fn upload_roundtrip() {
        let tok = create_upload_token("b", "obj.bin", 120, SECRET).unwrap();
        let c = verify_upload_token(&tok, "b", "obj.bin", SECRET).unwrap();
        assert_eq!(c.kind, "upload");
    }

    #[test]
    fn upload_token_rejected_as_download() {
        let tok = create_upload_token("b", "f.txt", 120, SECRET).unwrap();
        assert!(verify_token(&tok, "b", "f.txt", SECRET).is_err());
    }

    #[test]
    fn download_token_rejected_as_upload() {
        let tok = create_token("b", "f.txt", 120, SECRET).unwrap();
        assert!(verify_upload_token(&tok, "b", "f.txt", SECRET).is_err());
    }

    #[test]
    fn wrong_bucket_rejected() {
        let tok = create_token("bucket-a", "file.txt", 120, SECRET).unwrap();
        assert!(verify_token(&tok, "bucket-b", "file.txt", SECRET).is_err());
    }

    #[test]
    fn wrong_object_rejected() {
        let tok = create_token("bucket", "a.txt", 120, SECRET).unwrap();
        assert!(verify_token(&tok, "bucket", "b.txt", SECRET).is_err());
    }

    #[test]
    fn expired_rejected() {
        let claims = SignedUrlClaims {
            url: "bucket/f.txt".into(),
            exp: 1,
            iat: 0,
            kind: "download".into(),
        };
        let tok = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .unwrap();
        assert!(verify_token(&tok, "bucket", "f.txt", SECRET).is_err());
    }

    #[test]
    fn wrong_secret_rejected() {
        let tok = create_token("b", "f.txt", 120, SECRET).unwrap();
        assert!(verify_token(&tok, "b", "f.txt", "wrong").is_err());
    }
}
