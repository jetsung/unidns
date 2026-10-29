use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    crypto,
    error::{ApiResult, AppError},
    models::{AccountRow, now_utc},
};

use super::{ApiJson, SharedState};

// ---------- DTO ----------

#[derive(Deserialize)]
pub struct CreateAccountRequest {
    pub name: String,
    pub provider_type: crate::providers::ProviderType,
    pub credentials: Value,
}

#[derive(Deserialize)]
pub struct UpdateAccountRequest {
    pub name: Option<String>,
    pub status: Option<String>,
    pub credentials: Option<Value>,
}

#[derive(sqlx::FromRow)]
struct AccountWithSync {
    #[sqlx(flatten)]
    account: AccountRow,
    last_sync_at: Option<String>,
    last_sync_status: Option<String>,
    last_sync_error: Option<String>,
}

fn account_json(row: &AccountWithSync) -> Value {
    let a = &row.account;
    json!({
        "id": a.id,
        "name": a.name,
        "provider_type": a.provider_type,
        "provider_name": a.provider_type().map(|p| p.display_name()).unwrap_or("未知"),
        "status": a.status,
        "credentials_mask": a.credentials_mask,
        "created_at": a.created_at,
        "updated_at": a.updated_at,
        "last_verified_at": a.last_verified_at,
        "last_verify_ok": a.last_verify_ok.map(|v| v == 1),
        "last_verify_msg": a.last_verify_msg,
        "last_sync_at": row.last_sync_at,
        "last_sync_status": row.last_sync_status,
        "last_sync_error": row.last_sync_error,
    })
}

// ---------- 校验与凭据辅助 ----------

fn validate_credentials(
    ptype: crate::providers::ProviderType,
    credentials: &Value,
) -> Result<(), AppError> {
    let obj = credentials
        .as_object()
        .ok_or_else(|| AppError::BadRequest("credentials 必须是 JSON 对象".into()))?;
    for field in ptype.required_credential_fields() {
        let value = obj
            .get(*field)
            .and_then(|v| v.as_str())
            .filter(|v| !v.trim().is_empty());
        if value.is_none() {
            return Err(AppError::BadRequest(format!(
                "平台 {} 缺少必填凭据字段: {field}",
                ptype.display_name()
            )));
        }
    }
    Ok(())
}

fn credentials_mask(ptype: crate::providers::ProviderType, credentials: &Value) -> String {
    let first_field = ptype.required_credential_fields()[0];
    let value = credentials
        .get(first_field)
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let tail: String = value
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("****{tail}")
}

fn encrypt_credentials(state: &SharedState, credentials: &Value) -> Result<Vec<u8>, AppError> {
    let raw = serde_json::to_string(credentials)
        .map_err(|e| AppError::BadRequest(format!("凭据序列化失败: {e}")))?;
    crypto::encrypt(&state.master_key, raw.as_bytes())
        .map_err(|e| AppError::Internal(format!("凭据加密失败: {e}")))
}

pub(crate) fn decrypt_credentials(state: &SharedState, account: &AccountRow) -> ApiResult<String> {
    let bytes = crypto::decrypt(&state.master_key, &account.credentials_enc)
        .map_err(|e| AppError::Internal(format!("凭据解密失败: {e}")))?;
    String::from_utf8(bytes).map_err(|e| AppError::Internal(format!("凭据数据损坏: {e}")))
}

async fn load_account(state: &SharedState, id: i64) -> ApiResult<AccountRow> {
    sqlx::query_as::<_, AccountRow>("SELECT * FROM accounts WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(AppError::NotFound)
}

// ---------- Handlers ----------

/// POST /api/v1/accounts —— 添加平台账户（凭据加密入库）。
pub async fn create(
    State(state): State<SharedState>,
    ApiJson(req): ApiJson<CreateAccountRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if req.name.trim().is_empty() {
        return Err(AppError::BadRequest("账户名称不能为空".into()));
    }
    validate_credentials(req.provider_type, &req.credentials)?;
    let enc = encrypt_credentials(&state, &req.credentials)?;
    let mask = credentials_mask(req.provider_type, &req.credentials);
    let now = now_utc();
    let result = sqlx::query(
        "INSERT INTO accounts (name, provider_type, credentials_enc, credentials_mask, status, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'enabled', ?, ?)",
    )
    .bind(req.name.trim())
    .bind(req.provider_type.as_str())
    .bind(&enc)
    .bind(&mask)
    .bind(&now)
    .bind(&now)
    .execute(&state.pool)
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"id": result.last_insert_rowid()})),
    ))
}

/// GET /api/v1/accounts —— 账户列表（凭据脱敏，附最近一次同步结果，分页）。
#[derive(Deserialize)]
pub struct AccountQuery {
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

pub async fn list(
    State(state): State<SharedState>,
    Query(q): Query<AccountQuery>,
) -> ApiResult<Json<Value>> {
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(10).clamp(1, 200);
    let offset = (page - 1) * page_size;

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts a")
        .fetch_one(&state.pool)
        .await?;

    let rows = sqlx::query_as::<_, AccountWithSync>(
        "SELECT a.*, \
                s.started_at AS last_sync_at, s.status AS last_sync_status, s.error AS last_sync_error \
         FROM accounts a \
         LEFT JOIN sync_runs s ON s.id = (\
             SELECT id FROM sync_runs WHERE account_id = a.id AND status != 'running' \
             ORDER BY id DESC LIMIT 1) \
         ORDER BY a.id LIMIT ? OFFSET ?",
    )
    .bind(page_size)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "total": total,
        "page": page,
        "page_size": page_size,
        "items": rows.iter().map(account_json).collect::<Vec<_>>(),
    })))
}

/// GET /api/v1/accounts/{id}
pub async fn get_one(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let account = load_account(&state, id).await?;
    let row = AccountWithSync {
        last_sync_at: None,
        last_sync_status: None,
        last_sync_error: None,
        account,
    };
    Ok(Json(account_json(&row)))
}

/// PATCH /api/v1/accounts/{id} —— 修改名称 / 启停 / 更新凭据。
pub async fn update(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    ApiJson(req): ApiJson<UpdateAccountRequest>,
) -> ApiResult<StatusCode> {
    let mut account = load_account(&state, id).await?;
    if let Some(name) = &req.name {
        if name.trim().is_empty() {
            return Err(AppError::BadRequest("账户名称不能为空".into()));
        }
        account.name = name.trim().to_string();
    }
    if let Some(status) = &req.status {
        if status != "enabled" && status != "disabled" {
            return Err(AppError::BadRequest(
                "status 只能是 enabled 或 disabled".into(),
            ));
        }
        account.status = status.clone();
    }
    if let Some(credentials) = &req.credentials {
        let ptype = account.provider_type().ok_or_else(|| {
            AppError::Internal(format!("未知的 provider_type: {}", account.provider_type))
        })?;
        validate_credentials(ptype, credentials)?;
        account.credentials_enc = encrypt_credentials(&state, credentials)?;
        account.credentials_mask = credentials_mask(ptype, credentials);
        // 凭据更换后，原验证结果不再可信
        account.last_verified_at = None;
        account.last_verify_ok = None;
        account.last_verify_msg = None;
    }
    sqlx::query(
        "UPDATE accounts SET name = ?, status = ?, credentials_enc = ?, credentials_mask = ?, \
         last_verified_at = ?, last_verify_ok = ?, last_verify_msg = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&account.name)
    .bind(&account.status)
    .bind(&account.credentials_enc)
    .bind(&account.credentials_mask)
    .bind(account.last_verified_at.clone())
    .bind(account.last_verify_ok)
    .bind(account.last_verify_msg.clone())
    .bind(now_utc())
    .bind(id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/v1/accounts/{id} —— 删除账户：级联清理本地域名/记录，不触碰平台侧。
pub async fn delete(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    load_account(&state, id).await?;
    sqlx::query("DELETE FROM accounts WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await?;
    tracing::info!(account_id = id, "账户已删除（本地数据级联清理）");
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/v1/accounts/{id}/verify —— 连通性验证并记录结果。
pub async fn verify(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let account = load_account(&state, id).await?;
    let cred_json = decrypt_credentials(&state, &account)?;
    let ptype = account.provider_type().ok_or_else(|| {
        AppError::Internal(format!("未知的 provider_type: {}", account.provider_type))
    })?;
    let provider = (state.provider_factory)(ptype, &cred_json)?;
    let (ok, message) = match provider.verify().await {
        Ok(()) => (1i64, "验证成功".to_string()),
        Err(e) => {
            tracing::warn!(account_id = id, error = %e, "账户连通性验证失败");
            (0i64, e.to_string())
        }
    };
    sqlx::query(
        "UPDATE accounts SET last_verified_at = ?, last_verify_ok = ?, last_verify_msg = ?, updated_at = ? WHERE id = ?",
    )
    .bind(now_utc())
    .bind(ok)
    .bind(&message)
    .bind(now_utc())
    .bind(id)
    .execute(&state.pool)
    .await?;
    Ok(Json(json!({"ok": ok == 1, "message": message})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ProviderError, mock::MockState};
    use crate::web::{login_for_test, router, test_app_with_mock};
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn req(
        app: &axum::Router,
        method: &str,
        uri: &str,
        body: Option<&str>,
        cookie: Option<&str>,
    ) -> axum::response::Response {
        let mut b = axum::http::Request::builder().method(method).uri(uri);
        if let Some(c) = cookie {
            b = b.header("cookie", c);
        }
        let r = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        app.clone().oneshot(r).await.unwrap()
    }

    async fn body_json(res: axum::response::Response) -> Value {
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    const ALIYUN_CREDS: &str = r#"{"name":"主账户","provider_type":"aliyun","credentials":{"access_key_id":"LTAIabcdef123456","access_key_secret":"top-secret-value-987654"}}"#;

    #[tokio::test]
    async fn account_crud_and_masking() {
        let (state, _mock) = test_app_with_mock().await;
        let app = router(state.clone());
        let cookie = login_for_test(&app).await;

        // 未登录 → 401
        let res = req(&app, "GET", "/api/v1/accounts", None, None).await;
        assert_eq!(res.status(), 401);

        // 创建成功
        let res = req(
            &app,
            "POST",
            "/api/v1/accounts",
            Some(ALIYUN_CREDS),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 201);
        let id = body_json(res).await["id"].as_i64().unwrap();

        // 缺失必填凭据 → 400 且指出字段
        let bad = r#"{"name":"坏账户","provider_type":"aliyun","credentials":{"access_key_id":"only-one"}}"#;
        let res = req(&app, "POST", "/api/v1/accounts", Some(bad), Some(&cookie)).await;
        assert_eq!(res.status(), 400);
        assert!(
            body_json(res).await["error"]["message"]
                .as_str()
                .unwrap()
                .contains("access_key_secret")
        );

        // 不支持的平台类型 → 400（serde 枚举拒绝）
        let bad2 = r#"{"name":"x","provider_type":"hetzner","credentials":{}}"#;
        let res = req(&app, "POST", "/api/v1/accounts", Some(bad2), Some(&cookie)).await;
        assert_eq!(res.status(), 400);

        // 列表与详情：凭据仅脱敏，无明文
        for uri in ["/api/v1/accounts", &format!("/api/v1/accounts/{id}")] {
            let res = req(&app, "GET", uri, None, Some(&cookie)).await;
            assert_eq!(res.status(), 200);
            let raw = body_json(res).await.to_string();
            assert!(
                !raw.contains("top-secret-value-987654"),
                "凭据明文泄漏: {raw}"
            );
            assert!(
                !raw.contains("LTAIabcdef123456"),
                "AccessKey ID 明文泄漏: {raw}"
            );
            assert!(
                raw.contains("****654") || raw.contains("****3456"),
                "应包含脱敏掩码: {raw}"
            );
        }

        // 账户列表分页：默认分页字段；指定页与每页条数；page 超出范围返回空 items
        let res = req(&app, "GET", "/api/v1/accounts", None, Some(&cookie)).await;
        let body = body_json(res).await;
        assert_eq!(body["total"], 1);
        assert_eq!(body["page"], 1);
        assert_eq!(body["page_size"], 10);
        assert_eq!(body["items"].as_array().unwrap().len(), 1);

        let res = req(
            &app,
            "GET",
            "/api/v1/accounts?page=1&page_size=10",
            None,
            Some(&cookie),
        )
        .await;
        let body = body_json(res).await;
        assert_eq!(body["total"], 1);
        assert_eq!(body["page"], 1);
        assert_eq!(body["page_size"], 10);
        assert_eq!(body["items"].as_array().unwrap().len(), 1);

        let res = req(
            &app,
            "GET",
            "/api/v1/accounts?page=2&page_size=10",
            None,
            Some(&cookie),
        )
        .await;
        let body = body_json(res).await;
        assert_eq!(body["total"], 1);
        assert_eq!(body["items"].as_array().unwrap().len(), 0);

        // 修改名称与状态
        let res = req(
            &app,
            "PATCH",
            &format!("/api/v1/accounts/{id}"),
            Some(r#"{"name":"改名","status":"disabled"}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 204);
        let res = req(
            &app,
            "GET",
            &format!("/api/v1/accounts/{id}"),
            None,
            Some(&cookie),
        )
        .await;
        let body = body_json(res).await;
        assert_eq!(body["name"], "改名");
        assert_eq!(body["status"], "disabled");

        // 更新凭据 → 掩码变化、验证结果清空
        let res = req(&app, "PATCH", &format!("/api/v1/accounts/{id}"),
            Some(r#"{"credentials":{"access_key_id":"NEWkey99999","access_key_secret":"NEWsecret0000"}}"#), Some(&cookie)).await;
        assert_eq!(res.status(), 204);
        let res = req(
            &app,
            "GET",
            &format!("/api/v1/accounts/{id}"),
            None,
            Some(&cookie),
        )
        .await;
        let body = body_json(res).await;
        assert_eq!(body["credentials_mask"], "****9999"); // 取第一个必填字段（access_key_id）末 4 位
        assert!(body["last_verify_ok"].is_null());

        // 级联删除：插入域名与记录后删账户，本地数据一并消失
        sqlx::query("INSERT INTO domains (account_id, name, provider_domain_id, status, synced_at) VALUES (?, 'a.com', '', '', ?)")
            .bind(id).bind(now_utc()).execute(&state.pool).await.unwrap();
        let domain_id: i64 = sqlx::query_scalar("SELECT id FROM domains WHERE account_id = ?")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO records (domain_id, account_id, provider_record_id, host, rtype, value, ttl, line, status, synced_at) VALUES (?, ?, 'p1', 'www', 'A', '1.1.1.1', 600, 'default', 'enabled', ?)")
            .bind(domain_id).bind(id).bind(now_utc()).execute(&state.pool).await.unwrap();

        let res = req(
            &app,
            "DELETE",
            &format!("/api/v1/accounts/{id}"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 204);
        let (domains, records): (i64, i64) =
            sqlx::query_as("SELECT (SELECT COUNT(*) FROM domains), (SELECT COUNT(*) FROM records)")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(
            (domains, records),
            (0, 0),
            "删除账户应级联清理本地域名与记录"
        );

        // 删除不存在的账户 → 404
        let res = req(&app, "DELETE", "/api/v1/accounts/999", None, Some(&cookie)).await;
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn account_verify_outcomes() {
        let (state, mock) = test_app_with_mock().await;
        let app = router(state.clone());
        let cookie = login_for_test(&app).await;

        let res = req(
            &app,
            "POST",
            "/api/v1/accounts",
            Some(ALIYUN_CREDS),
            Some(&cookie),
        )
        .await;
        let id = body_json(res).await["id"].as_i64().unwrap();

        // 1) 验证成功
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{id}/verify"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 200);
        assert_eq!(body_json(res).await["ok"], true);

        // 2) 凭据无效（Auth 错误）
        mock.state.lock().await.fail_verify = Some(ProviderError::Auth("AccessKey 无效".into()));
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{id}/verify"),
            None,
            Some(&cookie),
        )
        .await;
        let body = body_json(res).await;
        assert_eq!(body["ok"], false);
        assert!(body["message"].as_str().unwrap().contains("无效"));

        // 3) 网络错误
        mock.state.lock().await.fail_verify =
            Some(ProviderError::Network("connection refused".into()));
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{id}/verify"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(body_json(res).await["ok"], false);

        // 结果已记录到账户
        let account = sqlx::query_as::<_, AccountRow>("SELECT * FROM accounts WHERE id = ?")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(account.last_verify_ok, Some(0));
        assert!(
            account
                .last_verify_msg
                .unwrap()
                .contains("connection refused")
        );

        // 不存在的账户 → 404
        let res = req(
            &app,
            "POST",
            "/api/v1/accounts/999/verify",
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn mock_state_with_zone_helper() {
        // 确保 MockState::with_zone 组装可用（同步/记录测试的基础）
        let state = MockState::default().with_zone("example.com");
        assert_eq!(state.domains.len(), 1);
        assert_eq!(state.records["example.com"].len(), 2);
    }

    /// 火山引擎账户：添加成功、缺 access_key_secret 被拒、未知 provider_type 仍被拒。
    #[tokio::test]
    async fn create_volcengine_account() {
        let (state, _mock) = test_app_with_mock().await;
        let app = router(state.clone());
        let cookie = login_for_test(&app).await;

        // 1) 添加 volcengine 账户成功
        let ok = r#"{"name":"火山账户","provider_type":"volcengine","credentials":{"access_key_id":"AKLTvolc123456","access_key_secret":"volc-secret-987654"}}"#;
        let res = req(&app, "POST", "/api/v1/accounts", Some(ok), Some(&cookie)).await;
        assert_eq!(res.status(), 201);
        let id = body_json(res).await["id"].as_i64().unwrap();
        // 列表展示火山引擎徽章所需字段
        let res = req(&app, "GET", "/api/v1/accounts", None, Some(&cookie)).await;
        let items = body_json(res).await["items"].as_array().unwrap().clone();
        assert_eq!(items[0]["provider_type"], "volcengine");
        assert_eq!(items[0]["provider_name"], "火山引擎");
        assert!(
            !items[0].to_string().contains("volc-secret-987654"),
            "凭据明文泄漏"
        );

        // 2) 缺 access_key_secret → 400 且指出字段
        let missing = r#"{"name":"缺密钥","provider_type":"volcengine","credentials":{"access_key_id":"only-ak"}}"#;
        let res = req(
            &app,
            "POST",
            "/api/v1/accounts",
            Some(missing),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 400);
        assert!(
            body_json(res).await["error"]["message"]
                .as_str()
                .unwrap()
                .contains("access_key_secret")
        );

        // 3) 未知 provider_type 仍被拒
        let unknown = r#"{"name":"x","provider_type":"hetzner","credentials":{}}"#;
        let res = req(
            &app,
            "POST",
            "/api/v1/accounts",
            Some(unknown),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 400);

        // 4) 账户入库后 provider_type 可被解析（迁移后 CHECK 约束放行 volcengine）
        let row = sqlx::query_as::<_, AccountRow>("SELECT * FROM accounts WHERE id = ?")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(
            row.provider_type(),
            Some(crate::providers::ProviderType::Volcengine)
        );
    }
}
