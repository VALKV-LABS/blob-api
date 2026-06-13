use axum::http::HeaderMap;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, StorageError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: Option<String>,
    pub role: Option<String>,
    pub exp: Option<u64>,
    pub iat: Option<u64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl Claims {
    pub fn role(&self) -> &str {
        self.role.as_deref().unwrap_or("anon")
    }

    pub fn is_service_role(&self) -> bool {
        self.role() == "service_role"
    }

    pub fn user_id(&self) -> Option<&str> {
        self.sub.as_deref()
    }

    pub fn to_json_string(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

pub fn extract_token(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("authorization") {
        if let Ok(s) = v.to_str() {
            if let Some(t) = s.strip_prefix("Bearer ") {
                return Some(t.to_string());
            }
        }
    }
    if let Some(v) = headers.get("apikey") {
        if let Ok(s) = v.to_str() {
            return Some(s.to_string());
        }
    }
    None
}

pub fn verify_jwt(token: &str, secret: &str) -> Result<Claims> {
    let mut validation = Validation::new(Algorithm::HS256);
    // anon_key and service_role_key have no exp — don't reject them
    validation.validate_exp = false;
    validation.required_spec_claims.clear();

    decode::<Claims>(token, &DecodingKey::from_secret(secret.as_bytes()), &validation)
        .map(|d| d.claims)
        .map_err(|_| StorageError::Unauthorized)
}

/// Verify and return claims, or None if no token present (anon request).
pub fn auth_from_headers(headers: &HeaderMap, secret: &str) -> Result<Claims> {
    match extract_token(headers) {
        Some(token) => verify_jwt(&token, secret),
        None => Ok(Claims {
            sub: None,
            role: Some("anon".into()),
            exp: None,
            iat: None,
            extra: Default::default(),
        }),
    }
}

/// Used for testing: mint a HS256 JWT with the given role.
#[cfg(test)]
pub fn mint_token(role: &str, sub: Option<&str>, secret: &str) -> String {
    use jsonwebtoken::{encode, EncodingKey, Header};
    let claims = Claims {
        sub: sub.map(Into::into),
        role: Some(role.into()),
        exp: None,
        iat: None,
        extra: Default::default(),
    };
    encode(&Header::new(Algorithm::HS256), &claims, &EncodingKey::from_secret(secret.as_bytes())).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-secret";

    #[test]
    fn anon_token_roundtrip() {
        let token = mint_token("anon", None, SECRET);
        let c = verify_jwt(&token, SECRET).unwrap();
        assert_eq!(c.role(), "anon");
        assert!(!c.is_service_role());
    }

    #[test]
    fn service_role_token_roundtrip() {
        let token = mint_token("service_role", None, SECRET);
        let c = verify_jwt(&token, SECRET).unwrap();
        assert!(c.is_service_role());
    }

    #[test]
    fn authenticated_token_has_sub() {
        let token = mint_token("authenticated", Some("user-123"), SECRET);
        let c = verify_jwt(&token, SECRET).unwrap();
        assert_eq!(c.user_id(), Some("user-123"));
        assert_eq!(c.role(), "authenticated");
    }

    #[test]
    fn wrong_secret_rejected() {
        let token = mint_token("anon", None, "secret-a");
        assert!(verify_jwt(&token, "secret-b").is_err());
    }

    #[test]
    fn extract_from_bearer_header() {
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer tok123".parse().unwrap());
        assert_eq!(extract_token(&h).as_deref(), Some("tok123"));
    }

    #[test]
    fn extract_from_apikey_header() {
        let mut h = HeaderMap::new();
        h.insert("apikey", "key456".parse().unwrap());
        assert_eq!(extract_token(&h).as_deref(), Some("key456"));
    }

    #[test]
    fn no_auth_headers_returns_none() {
        assert_eq!(extract_token(&HeaderMap::new()), None);
    }

    #[test]
    fn no_token_auth_from_headers_gives_anon() {
        let c = auth_from_headers(&HeaderMap::new(), SECRET).unwrap();
        assert_eq!(c.role(), "anon");
    }
}
