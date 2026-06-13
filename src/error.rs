use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("not found")]
    NotFound,
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    #[error("payload too large")]
    PayloadTooLarge,
    #[error("{0}")]
    S3(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for StorageError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            StorageError::NotFound      => (StatusCode::NOT_FOUND,            "not_found"),
            StorageError::Unauthorized  => (StatusCode::UNAUTHORIZED,         "unauthorized"),
            StorageError::Forbidden     => (StatusCode::FORBIDDEN,            "forbidden"),
            StorageError::BadRequest(_) => (StatusCode::BAD_REQUEST,          "invalid_request"),
            StorageError::Conflict(_)   => (StatusCode::CONFLICT,             "already_exists"),
            StorageError::PayloadTooLarge => (StatusCode::PAYLOAD_TOO_LARGE,  "payload_too_large"),
            StorageError::S3(_)         => (StatusCode::INTERNAL_SERVER_ERROR, "s3_error"),
            StorageError::Db(e) => {
                tracing::error!(err = %e, "db error");
                (StatusCode::INTERNAL_SERVER_ERROR, "db_error")
            }
            StorageError::Internal(e) => {
                tracing::error!(err = %e, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
        };

        let message = match &self {
            StorageError::Db(_) | StorageError::Internal(_) => "Internal server error".into(),
            other => other.to_string(),
        };

        (status, Json(json!({ "statusCode": status.as_u16(), "error": code, "message": message }))).into_response()
    }
}

pub type Result<T, E = StorageError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn not_found_is_404() {
        let r = StorageError::NotFound.into_response();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unauthorized_is_401() {
        let r = StorageError::Unauthorized.into_response();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bad_request_is_400() {
        let r = StorageError::BadRequest("bad".into()).into_response();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn conflict_is_409() {
        let r = StorageError::Conflict("dup".into()).into_response();
        assert_eq!(r.status(), StatusCode::CONFLICT);
    }
}
