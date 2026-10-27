use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

/// 统一 API 错误：响应体为 `{"error": {"code": "...", "message": "..."}}`。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("未登录或会话已过期")]
    Unauthorized,
    /// 401 但带自定义消息（如登录口令错误）。
    #[error("{0}")]
    UnauthorizedMsg(String),
    #[error("资源不存在")]
    NotFound,
    #[error("{0}")]
    Provider(#[from] crate::providers::ProviderError),
    #[error("内部错误: {0}")]
    Internal(String),
    #[error("数据库错误")]
    Db(#[from] sqlx::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, "invalid_request", m),
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "未登录或会话已过期".to_string(),
            ),
            AppError::UnauthorizedMsg(m) => (StatusCode::UNAUTHORIZED, "unauthorized", m),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not_found", "资源不存在".to_string()),
            AppError::Provider(e) => provider_response(e),
            AppError::Internal(m) => {
                tracing::error!(%m, "内部错误");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal", m)
            }
            AppError::Db(e) => {
                tracing::error!(error = %e, "数据库错误");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "内部错误".to_string(),
                )
            }
        };
        (
            status,
            Json(json!({"error": {"code": code, "message": message}})),
        )
            .into_response()
    }
}

fn provider_response(e: crate::providers::ProviderError) -> (StatusCode, &'static str, String) {
    use crate::providers::ProviderError as P;
    match e {
        P::Auth(m) => (StatusCode::BAD_REQUEST, "provider_auth", m),
        P::RateLimited {
            retry_after,
            message,
        } => {
            let msg = match retry_after {
                Some(sec) => format!("{message}（建议 {sec} 秒后重试）"),
                None => message,
            };
            (StatusCode::TOO_MANY_REQUESTS, "provider_rate_limited", msg)
        }
        P::NotFound(m) => (StatusCode::NOT_FOUND, "provider_not_found", m),
        P::Unsupported(m) => (StatusCode::CONFLICT, "provider_unsupported", m),
        P::Network(m) => (StatusCode::BAD_GATEWAY, "provider_network", m),
        P::Api { code, message } => {
            tracing::warn!(code = %code, %message, "平台 API 错误");
            (
                StatusCode::BAD_GATEWAY,
                "provider_api",
                format!("[{code}] {message}"),
            )
        }
    }
}

pub type ApiResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderError;

    #[test]
    fn provider_errors_map_to_status_codes() {
        let (status, _, _) = provider_response(ProviderError::Auth("k".into()));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, msg) = provider_response(ProviderError::RateLimited {
            retry_after: Some(30),
            message: "限流".into(),
        });
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(msg.contains("30 秒"));
        let (status, _, _) = provider_response(ProviderError::NotFound("n".into()));
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = provider_response(ProviderError::Unsupported("u".into()));
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _, _) = provider_response(ProviderError::Network("x".into()));
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let (status, code, _) = provider_response(ProviderError::Api {
            code: "E1".into(),
            message: "boom".into(),
        });
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(code, "provider_api");
    }

    #[test]
    fn app_error_response_shape() {
        let resp = AppError::Provider(ProviderError::Unsupported("不支持".into())).into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let resp = AppError::Unauthorized.into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
