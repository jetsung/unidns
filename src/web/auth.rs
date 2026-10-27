use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;
use tower_sessions::Session;

use crate::{crypto, error::ApiResult, web::SESSION_ADMIN_KEY};

use super::{ApiJson, SharedState};

#[derive(Deserialize)]
pub struct PasswordRequest {
    password: String,
}

#[derive(Deserialize)]
pub struct ChangePasswordRequest {
    old_password: String,
    new_password: String,
}

fn validate_password(pw: &str) -> Result<(), crate::error::AppError> {
    if pw.chars().count() < 8 {
        return Err(crate::error::AppError::BadRequest(
            "口令长度至少 8 个字符".into(),
        ));
    }
    Ok(())
}

/// GET /api/v1/auth/status —— 登录态自描述：是否已初始化口令、是否已登录。
pub async fn status(State(state): State<SharedState>, session: Session) -> Json<serde_json::Value> {
    let initialized = crypto::has_admin_password(&state.pool).await;
    let authenticated = session
        .get::<bool>(SESSION_ADMIN_KEY)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);
    Json(json!({"initialized": initialized, "authenticated": authenticated}))
}

/// GET /api/v1/auth/me —— 已登录身份探针（受保护）。
pub async fn me() -> Json<serde_json::Value> {
    Json(json!({"role": "admin"}))
}

/// POST /api/v1/auth/setup —— 首次启动设置管理员口令（仅未初始化时可用）。
pub async fn setup(
    State(state): State<SharedState>,
    session: Session,
    ApiJson(req): ApiJson<PasswordRequest>,
) -> ApiResult<StatusCode> {
    if crypto::has_admin_password(&state.pool).await {
        return Err(crate::error::AppError::BadRequest(
            "管理员口令已初始化，请直接登录或登录后修改口令".into(),
        ));
    }
    validate_password(&req.password)?;
    crypto::set_admin_password(&state.pool, None, &req.password)
        .await
        .map_err(crate::error::AppError::BadRequest)?;
    session
        .insert(SESSION_ADMIN_KEY, true)
        .await
        .map_err(|e| crate::error::AppError::Internal(format!("写入会话失败: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/v1/auth/login —— 管理员口令登录。
pub async fn login(
    State(state): State<SharedState>,
    session: Session,
    ApiJson(req): ApiJson<PasswordRequest>,
) -> ApiResult<StatusCode> {
    if !crypto::has_admin_password(&state.pool).await {
        return Err(crate::error::AppError::BadRequest(
            "管理员口令尚未初始化，请先完成首次设置".into(),
        ));
    }
    if !crypto::verify_admin_password(&state.pool, &req.password).await {
        return Err(crate::error::AppError::UnauthorizedMsg("口令错误".into()));
    }
    // 换发会话 ID，防会话固定
    session
        .cycle_id()
        .await
        .map_err(|e| crate::error::AppError::Internal(format!("重置会话失败: {e}")))?;
    session
        .insert(SESSION_ADMIN_KEY, true)
        .await
        .map_err(|e| crate::error::AppError::Internal(format!("写入会话失败: {e}")))?;
    tracing::info!("管理员登录成功");
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/v1/auth/logout —— 登出（清空会话数据并删除 cookie）。
pub async fn logout(session: Session) -> ApiResult<StatusCode> {
    session
        .flush()
        .await
        .map_err(|e| crate::error::AppError::Internal(format!("销毁会话失败: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/v1/auth/password —— 修改管理员口令（需旧口令）。
pub async fn change_password(
    State(state): State<SharedState>,
    ApiJson(req): ApiJson<ChangePasswordRequest>,
) -> ApiResult<StatusCode> {
    validate_password(&req.new_password)?;
    crypto::set_admin_password(&state.pool, Some(&req.old_password), &req.new_password)
        .await
        .map_err(crate::error::AppError::BadRequest)?;
    tracing::info!("管理员口令已修改");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use crate::web::{router, test_state};
    use axum::{body::Body, extract::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn json_req(
        method: &str,
        uri: &str,
        body: Option<&str>,
        cookie: Option<&str>,
    ) -> Request {
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        if let Some(c) = cookie {
            builder = builder.header("cookie", c);
        }
        match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        }
    }

    fn cookie_of(res: &axum::response::Response) -> Option<String> {
        res.headers()
            .get_all("set-cookie")
            .iter()
            .find_map(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next().map(|s| s.to_string()))
    }

    async fn body_string(res: axum::response::Response) -> String {
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn auth_full_flow() {
        let state = test_state().await;
        let app = router(state);

        // 未初始化：status 报告未初始化
        let res = app
            .clone()
            .oneshot(json_req("GET", "/api/v1/auth/status", None, None).await)
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let body = body_string(res).await;
        assert!(body.contains("\"initialized\":false"), "{body}");

        // 未登录访问受保护 API → 401，且错误体结构统一
        let res = app
            .clone()
            .oneshot(json_req("GET", "/api/v1/auth/me", None, None).await)
            .await
            .unwrap();
        assert_eq!(res.status(), 401);
        let body = body_string(res).await;
        assert!(
            body.contains("\"error\"") && body.contains("\"code\":\"unauthorized\""),
            "{body}"
        );

        // 登录未初始化 → 400 引导先 setup
        let res = app
            .clone()
            .oneshot(
                json_req(
                    "POST",
                    "/api/v1/auth/login",
                    Some(r#"{"password":"password123"}"#),
                    None,
                )
                .await,
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 400);

        // setup 设置口令（太短的口令被拒绝）
        let res = app
            .clone()
            .oneshot(
                json_req(
                    "POST",
                    "/api/v1/auth/setup",
                    Some(r#"{"password":"short"}"#),
                    None,
                )
                .await,
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 400);

        let res = app
            .clone()
            .oneshot(
                json_req(
                    "POST",
                    "/api/v1/auth/setup",
                    Some(r#"{"password":"password123"}"#),
                    None,
                )
                .await,
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
        let cookie = cookie_of(&res).expect("setup 应返回会话 cookie");

        // setup 后的会话可直接访问受保护 API
        let res = app
            .clone()
            .oneshot(json_req("GET", "/api/v1/auth/me", None, Some(&cookie)).await)
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // 重复 setup → 400
        let res = app
            .clone()
            .oneshot(
                json_req(
                    "POST",
                    "/api/v1/auth/setup",
                    Some(r#"{"password":"password456"}"#),
                    None,
                )
                .await,
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 400);

        // 登出 → 再访问 → 401
        let res = app
            .clone()
            .oneshot(json_req("POST", "/api/v1/auth/logout", None, Some(&cookie)).await)
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
        let res = app
            .clone()
            .oneshot(json_req("GET", "/api/v1/auth/me", None, Some(&cookie)).await)
            .await
            .unwrap();
        assert_eq!(res.status(), 401);

        // 错误口令登录 → 401
        let res = app
            .clone()
            .oneshot(
                json_req(
                    "POST",
                    "/api/v1/auth/login",
                    Some(r#"{"password":"wrong-pass"}"#),
                    None,
                )
                .await,
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 401);

        // 正确口令登录 → 成功，可访问受保护 API
        let res = app
            .clone()
            .oneshot(
                json_req(
                    "POST",
                    "/api/v1/auth/login",
                    Some(r#"{"password":"password123"}"#),
                    None,
                )
                .await,
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
        let cookie2 = cookie_of(&res).expect("login 应返回会话 cookie");

        // 修改口令：旧口令错误 → 400
        let res = app
            .clone()
            .oneshot(
                json_req(
                    "POST",
                    "/api/v1/auth/password",
                    Some(r#"{"old_password":"bad","new_password":"newpass456"}"#),
                    Some(&cookie2),
                )
                .await,
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 400);

        // 修改口令成功 → 旧口令失效
        let res = app
            .clone()
            .oneshot(
                json_req(
                    "POST",
                    "/api/v1/auth/password",
                    Some(r#"{"old_password":"password123","new_password":"newpass456"}"#),
                    Some(&cookie2),
                )
                .await,
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
        let res = app
            .clone()
            .oneshot(
                json_req(
                    "POST",
                    "/api/v1/auth/login",
                    Some(r#"{"password":"password123"}"#),
                    None,
                )
                .await,
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 401);
        let res = app
            .clone()
            .oneshot(
                json_req(
                    "POST",
                    "/api/v1/auth/login",
                    Some(r#"{"password":"newpass456"}"#),
                    None,
                )
                .await,
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
    }
}
