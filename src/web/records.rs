use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    error::{ApiResult, AppError},
    models::{AccountRow, DomainRow, RecordRow, now_utc},
    providers::{ProviderRecord, ProviderZone, RecordInput},
    sync::build_provider_for_account,
};

use super::{ApiJson, SharedState};

const ALLOWED_TYPES: [&str; 8] = ["A", "AAAA", "CNAME", "MX", "TXT", "NS", "SRV", "CAA"];

// ---------- 校验 ----------

fn validate_record_input(input: &RecordInput) -> Result<(), AppError> {
    if input.host.trim().is_empty() {
        return Err(AppError::BadRequest(
            "主机记录（host）不能为空，根记录请使用 @".into(),
        ));
    }
    if !ALLOWED_TYPES.contains(&input.rtype.as_str()) {
        return Err(AppError::BadRequest(format!(
            "不支持的记录类型 {}，允许: {}",
            input.rtype,
            ALLOWED_TYPES.join("/")
        )));
    }
    if input.value.trim().is_empty() {
        return Err(AppError::BadRequest("记录值（value）不能为空".into()));
    }
    if input.ttl == 0 || input.ttl > 86400 {
        return Err(AppError::BadRequest("TTL 需在 1-86400 秒之间".into()));
    }
    if let Some(w) = input.weight
        && !(-1..=100).contains(&w)
    {
        return Err(AppError::BadRequest(
            "权重（weight）需在 -1~100 之间，留空表示平台默认".into(),
        ));
    }
    if let Some(p) = input.priority
        && !(0..=50).contains(&p)
    {
        return Err(AppError::BadRequest(
            "优先级（priority）需在 0~50 之间，留空表示平台默认".into(),
        ));
    }
    Ok(())
}

/// 火山引擎记录备注限制 ≤16 UTF-8 字符（按官方中文文档保守校验；超长拒绝、不调平台）。
fn check_volcengine_remark(
    ptype: Option<crate::providers::ProviderType>,
    remark: Option<&str>,
) -> Result<(), AppError> {
    if ptype != Some(crate::providers::ProviderType::Volcengine) {
        return Ok(());
    }
    let r = remark.unwrap_or("");
    if r.chars().count() > 16 {
        return Err(AppError::BadRequest(
            "火山引擎记录备注不能超过 16 个字符".into(),
        ));
    }
    Ok(())
}

// ---------- 数据加载 ----------

async fn load_domain(state: &SharedState, id: i64) -> ApiResult<(DomainRow, AccountRow)> {
    let domain = sqlx::query_as::<_, DomainRow>(
        "SELECT d.* FROM domains d JOIN accounts a ON a.id = d.account_id WHERE d.id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(AppError::NotFound)?;
    let account = sqlx::query_as::<_, AccountRow>("SELECT * FROM accounts WHERE id = ?")
        .bind(domain.account_id)
        .fetch_one(&state.pool)
        .await?;
    Ok((domain, account))
}

async fn load_record(
    state: &SharedState,
    id: i64,
) -> ApiResult<(RecordRow, DomainRow, AccountRow)> {
    let record = sqlx::query_as::<_, RecordRow>("SELECT * FROM records WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(AppError::NotFound)?;
    let (domain, account) = load_domain(state, record.domain_id).await?;
    Ok((record, domain, account))
}

fn zone_of(domain: &DomainRow) -> ProviderZone {
    ProviderZone {
        name: domain.name.clone(),
        provider_domain_id: domain.provider_domain_id.clone(),
    }
}

fn provider_record_of(record: &RecordRow, updated: &RecordUpdate) -> ProviderRecord {
    ProviderRecord {
        provider_record_id: record.provider_record_id.clone(),
        host: updated.host.clone().unwrap_or_else(|| record.host.clone()),
        rtype: updated
            .rtype
            .clone()
            .unwrap_or_else(|| record.rtype.clone()),
        value: updated
            .value
            .clone()
            .unwrap_or_else(|| record.value.clone()),
        ttl: updated.ttl.unwrap_or(record.ttl as u64),
        line: updated.line.clone().or_else(|| {
            if record.line.is_empty() {
                None
            } else {
                Some(record.line.clone())
            }
        }),
        proxied: updated.proxied.or(record.proxied_bool()),
        enabled: record.enabled(),
        remark: updated.remark.clone().or_else(|| {
            if record.remark.is_empty() {
                None
            } else {
                Some(record.remark.clone())
            }
        }),
        weight: updated.weight.or(record.weight),
        priority: updated.priority.or(record.priority),
    }
}

async fn insert_local_record(
    state: &SharedState,
    domain: &DomainRow,
    r: &ProviderRecord,
) -> Result<i64, AppError> {
    let now = now_utc();
    let result = sqlx::query(
        "INSERT INTO records (domain_id, account_id, provider_record_id, host, rtype, value, ttl, line, proxied, status, pending_sync, remark, synced_at, weight, priority) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'enabled', 0, ?, ?, ?, ?)",
    )
    .bind(domain.id)
    .bind(domain.account_id)
    .bind(&r.provider_record_id)
    .bind(&r.host)
    .bind(&r.rtype)
    .bind(&r.value)
    .bind(r.ttl as i64)
    .bind(r.line.clone().unwrap_or_default())
    .bind(r.proxied.map(|p| p as i64))
    .bind(r.remark.clone().unwrap_or_default())
    .bind(&now)
    .bind(r.weight)
    .bind(r.priority)
    .execute(&state.pool)
    .await
    .map_err(AppError::Db)?;
    Ok(result.last_insert_rowid())
}

fn record_json(record: &RecordRow, domain: &DomainRow, account: &AccountRow) -> Value {
    json!({
        "id": record.id,
        "domain_id": record.domain_id,
        "domain_name": domain.name,
        "account_id": account.id,
        "provider_type": account.provider_type,
        "provider_name": account.provider_type().map(|p| p.display_name()).unwrap_or("未知"),
        "provider_record_id": record.provider_record_id,
        "host": record.host,
        "rtype": record.rtype,
        "value": record.value,
        "ttl": record.ttl,
        "line": record.line,
        "proxied": record.proxied_bool(),
        "status": record.status,
        "remark": record.remark,
        "pending_sync": record.pending_sync == 1,
        "synced_at": record.synced_at,
        "weight": record.weight,
        "priority": record.priority,
    })
}

// ---------- 查询 ----------

#[derive(Deserialize)]
pub struct DomainQuery {
    pub keyword: Option<String>,
    pub provider: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

/// GET /api/v1/domains —— 域名统一视图（平台/账户标注 + 记录数）。
pub async fn list_domains(
    State(state): State<SharedState>,
    Query(q): Query<DomainQuery>,
) -> ApiResult<Json<Value>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i64,
        account_id: i64,
        name: String,
        status: String,
        synced_at: String,
        remark: String,
        account_name: String,
        provider_type: String,
        record_count: i64,
    }
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(10).clamp(1, 200);
    let offset = (page - 1) * page_size;

    let where_clause = "(? IS NULL OR d.name LIKE '%' || ? || '%') \
                        AND (? IS NULL OR a.provider_type = ?)";
    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM domains d JOIN accounts a ON a.id = d.account_id WHERE {where_clause}"
    ))
    .bind(&q.keyword)
    .bind(&q.keyword)
    .bind(&q.provider)
    .bind(&q.provider)
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query_as::<_, Row>(&format!(
        "SELECT d.id, d.account_id, d.name, d.status, d.synced_at, d.remark, \
                a.name AS account_name, a.provider_type, \
                (SELECT COUNT(*) FROM records r WHERE r.domain_id = d.id) AS record_count \
         FROM domains d JOIN accounts a ON a.id = d.account_id \
         WHERE {where_clause} \
         ORDER BY d.name LIMIT ? OFFSET ?"
    ))
    .bind(&q.keyword)
    .bind(&q.keyword)
    .bind(&q.provider)
    .bind(&q.provider)
    .bind(page_size)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "name": r.name,
                "status": r.status,
                "synced_at": r.synced_at,
                "remark": r.remark,
                "account_id": r.account_id,
                "account_name": r.account_name,
                "provider_type": r.provider_type,
                "record_count": r.record_count,
            })
        })
        .collect();
    Ok(Json(
        json!({"total": total, "page": page, "page_size": page_size, "items": items}),
    ))
}

/// GET /api/v1/domains/{id}
pub async fn get_domain(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let (domain, account) = load_domain(&state, id).await?;
    Ok(Json(json!({
        "id": domain.id,
        "name": domain.name,
        "status": domain.status,
        "synced_at": domain.synced_at,
        "remark": domain.remark,
        "account_id": account.id,
        "account_name": account.name,
        "provider_type": account.provider_type,
        "provider_name": account.provider_type().map(|p| p.display_name()).unwrap_or("未知"),
    })))
}

/// PATCH /api/v1/domains/{id} —— 更新域名备注：先回写上游平台，再存本地。
/// DNSPod 与阿里云支持域名备注（空字符串清除上游备注）；CloudFlare/EdgeOne 域名无备注概念，仅本地保存。
/// 上游回写失败（非 Unsupported）则不写本地，保证本地与上游一致。
#[derive(Deserialize)]
pub struct UpdateDomainRemark {
    pub remark: String,
}

pub async fn update_domain(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    ApiJson(body): ApiJson<UpdateDomainRemark>,
) -> ApiResult<Json<Value>> {
    let (domain, account) = load_domain(&state, id).await?;
    if body.remark.chars().count() > 500 {
        return Err(crate::error::AppError::BadRequest(
            "备注长度不能超过 500 字符".into(),
        ));
    }
    let provider = crate::sync::build_provider_for_account(&state, &account)?;
    match provider.set_domain_remark(&domain.name, &body.remark).await {
        Ok(_) => {}
        Err(crate::providers::ProviderError::Unsupported(_)) => {
            // 平台无域名备注能力，仅本地保存
        }
        Err(e) => return Err(e.into()),
    }
    sqlx::query("UPDATE domains SET remark = ? WHERE id = ?")
        .bind(&body.remark)
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(Json(json!({ "id": id, "remark": body.remark })))
}

#[derive(Deserialize)]
pub struct RecordQuery {
    /// 查询参数名为 type
    #[serde(rename = "type")]
    pub rtype: Option<String>,
    pub keyword: Option<String>,
    pub status: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

/// GET /api/v1/domains/{id}/records —— 域名下记录（类型/关键字/状态过滤 + 分页）。
pub async fn list_records(
    State(state): State<SharedState>,
    Path(domain_id): Path<i64>,
    Query(q): Query<RecordQuery>,
) -> ApiResult<Json<Value>> {
    let (domain, account) = load_domain(&state, domain_id).await?;
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(10).clamp(1, 200);
    let offset = (page - 1) * page_size;

    let where_clause = "(? IS NULL OR rtype = ?) AND (? IS NULL OR host LIKE '%' || ? || '%') \
                        AND (? IS NULL OR status = ?)";
    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM records WHERE domain_id = ? AND {where_clause}"
    ))
    .bind(domain_id)
    .bind(&q.rtype)
    .bind(&q.rtype)
    .bind(&q.keyword)
    .bind(&q.keyword)
    .bind(&q.status)
    .bind(&q.status)
    .fetch_one(&state.pool)
    .await?;

    let records = sqlx::query_as::<_, RecordRow>(&format!(
        "SELECT * FROM records WHERE domain_id = ? AND {where_clause} \
             ORDER BY host, id LIMIT ? OFFSET ?"
    ))
    .bind(domain_id)
    .bind(&q.rtype)
    .bind(&q.rtype)
    .bind(&q.keyword)
    .bind(&q.keyword)
    .bind(&q.status)
    .bind(&q.status)
    .bind(page_size)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(json!({
        "total": total,
        "page": page,
        "page_size": page_size,
        "items": records.iter().map(|r| record_json(r, &domain, &account)).collect::<Vec<_>>(),
    })))
}

// ---------- 写操作 ----------

/// POST /api/v1/domains/{id}/records —— 创建记录并实时回写平台。
pub async fn create_record(
    State(state): State<SharedState>,
    Path(domain_id): Path<i64>,
    ApiJson(input): ApiJson<RecordInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    validate_record_input(&input)?;
    let (domain, account) = load_domain(&state, domain_id).await?;
    if !account.enabled() {
        return Err(AppError::BadRequest(
            "所属账户已停用，无法操作解析记录".into(),
        ));
    }
    check_volcengine_remark(account.provider_type(), input.remark.as_deref())?;
    let provider = build_provider_for_account(&state, &account)?;
    let created = provider.create_record(&zone_of(&domain), &input).await?;
    let record_id = insert_local_record(&state, &domain, &created).await?;
    tracing::info!(record_id, domain = %domain.name, account_id = account.id, "创建解析记录并回写平台");
    Ok((
        StatusCode::CREATED,
        Json(json!({"id": record_id, "provider_record_id": created.provider_record_id})),
    ))
}

#[derive(Deserialize, Default)]
pub struct RecordUpdate {
    pub host: Option<String>,
    pub rtype: Option<String>,
    pub value: Option<String>,
    pub ttl: Option<u64>,
    pub line: Option<String>,
    pub proxied: Option<bool>,
    pub remark: Option<String>,
    pub weight: Option<i64>,
    pub priority: Option<i64>,
}

/// PATCH /api/v1/records/{id} —— 修改记录并回写平台。
pub async fn update_record(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    ApiJson(update): ApiJson<RecordUpdate>,
) -> ApiResult<StatusCode> {
    if update.host.is_none()
        && update.rtype.is_none()
        && update.value.is_none()
        && update.ttl.is_none()
        && update.line.is_none()
        && update.proxied.is_none()
        && update.remark.is_none()
        && update.weight.is_none()
        && update.priority.is_none()
    {
        return Err(AppError::BadRequest("至少提供一个要修改的字段".into()));
    }
    let (record, domain, account) = load_record(&state, id).await?;
    if !account.enabled() {
        return Err(AppError::BadRequest(
            "所属账户已停用，无法操作解析记录".into(),
        ));
    }
    let merged = provider_record_of(&record, &update);
    validate_record_input(&RecordInput {
        host: merged.host.clone(),
        rtype: merged.rtype.clone(),
        value: merged.value.clone(),
        ttl: merged.ttl,
        line: merged.line.clone(),
        proxied: merged.proxied,
        remark: merged.remark.clone(),
        weight: merged.weight,
        priority: merged.priority,
    })?;
    check_volcengine_remark(account.provider_type(), merged.remark.as_deref())?;
    let provider = build_provider_for_account(&state, &account)?;
    match provider.update_record(&zone_of(&domain), &merged).await {
        // 以适配器返回值写库，确保 EdgeOne 的归一结果（line 为空等）落盘
        Ok(updated) => {
            sqlx::query(
                "UPDATE records SET host = ?, rtype = ?, value = ?, ttl = ?, line = ?, proxied = ?, remark = ?, weight = ?, priority = ?, pending_sync = 0, synced_at = ? WHERE id = ?",
            )
            .bind(&updated.host)
            .bind(&updated.rtype)
            .bind(&updated.value)
            .bind(updated.ttl as i64)
            .bind(updated.line.clone().unwrap_or_default())
            .bind(updated.proxied.map(|p| p as i64))
            .bind(updated.remark.clone().unwrap_or_default())
            .bind(updated.weight)
            .bind(updated.priority)
            .bind(now_utc())
            .bind(id)
            .execute(&state.pool)
            .await?;
            tracing::info!(record_id = id, "修改解析记录并回写平台");
            Ok(StatusCode::NO_CONTENT)
        }
        // 平台侧记录已不存在：返回失败并标记待同步，等待下次同步修复
        Err(crate::providers::ProviderError::NotFound(m)) => {
            mark_pending_sync(&state, id).await;
            Err(AppError::BadRequest(format!(
                "记录在平台侧不存在（{m}），已标记待同步，下次同步将自动对齐"
            )))
        }
        Err(e) => Err(e.into()),
    }
}

async fn mark_pending_sync(state: &SharedState, record_id: i64) {
    let _ = sqlx::query("UPDATE records SET pending_sync = 1 WHERE id = ?")
        .bind(record_id)
        .execute(&state.pool)
        .await;
}

#[derive(Deserialize)]
pub struct StatusRequest {
    pub enabled: bool,
}

/// POST /api/v1/records/{id}/status —— 启用/停用记录。
pub async fn set_status(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    ApiJson(req): ApiJson<StatusRequest>,
) -> ApiResult<StatusCode> {
    let (record, domain, account) = load_record(&state, id).await?;
    if !account.enabled() {
        return Err(AppError::BadRequest(
            "所属账户已停用，无法操作解析记录".into(),
        ));
    }
    let provider = build_provider_for_account(&state, &account)?;
    provider
        .set_record_status(&zone_of(&domain), &record.provider_record_id, req.enabled)
        .await?;
    // 平台侧已生效后更新本地；NotFound/Unsupported 由 ProviderError 映射向上返回
    sqlx::query("UPDATE records SET status = ?, pending_sync = 0 WHERE id = ?")
        .bind(if req.enabled { "enabled" } else { "disabled" })
        .bind(id)
        .execute(&state.pool)
        .await?;
    tracing::info!(
        record_id = id,
        enabled = req.enabled,
        "解析记录启停已回写平台"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/v1/records/{id} —— 删除记录并回写平台；平台侧已不存在时按幂等成功处理。
pub async fn delete_record(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let (record, domain, account) = load_record(&state, id).await?;
    if !account.enabled() {
        return Err(AppError::BadRequest(
            "所属账户已停用，无法操作解析记录".into(),
        ));
    }
    let provider = build_provider_for_account(&state, &account)?;
    match provider
        .delete_record(&zone_of(&domain), &record.provider_record_id)
        .await
    {
        Ok(()) | Err(crate::providers::ProviderError::NotFound(_)) => {
            sqlx::query("DELETE FROM records WHERE id = ?")
                .bind(id)
                .execute(&state.pool)
                .await?;
            tracing::info!(record_id = id, "删除解析记录并回写平台");
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ProviderError, mock::MockState};
    use crate::web::{login_for_test, router, test_app_with_multi_mock};
    use axum::body::Body;
    use http_body_util::BodyExt;
    use std::time::Duration;
    use tower::ServiceExt;

    /// 等待某账户最近一次同步任务完成（异步同步启动后用）。
    async fn wait_sync_done(pool: &sqlx::SqlitePool, account_id: i64) {
        for _ in 0..100 {
            let (status,): (String,) = sqlx::query_as(
                "SELECT status FROM sync_runs WHERE account_id = ? ORDER BY id DESC LIMIT 1",
            )
            .bind(account_id)
            .fetch_one(pool)
            .await
            .unwrap();
            if status != "running" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("同步任务等待超时");
    }

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

    #[tokio::test]
    async fn create_record_writeback_and_validation() {
        let (state, mocks) = test_app_with_multi_mock().await;
        let mock = mocks.get("a").unwrap().clone();
        {
            let mut guard = mock.state.lock().await;
            *guard = MockState::default().with_zone("example.com");
        }
        let app = router(state.clone());
        let cookie = login_for_test(&app).await;
        let res = req(&app, "POST", "/api/v1/accounts",
            Some(r#"{"name":"测试","provider_type":"cloudflare","credentials":{"api_token":"token-aaaa-1111","mock_id":"a"}}"#),
            Some(&cookie)).await;
        let account_id = body_json(res).await["id"].as_i64().unwrap();
        req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{account_id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        wait_sync_done(&state.pool, account_id).await;
        let domain_id: i64 = sqlx::query_scalar("SELECT id FROM domains LIMIT 1")
            .fetch_one(&state.pool)
            .await
            .unwrap();

        // 参数校验拒绝：类型不支持 / 值为空 / TTL 越界，且不触发平台调用
        for bad in [
            r#"{"host":"www","rtype":"PTR","value":"1.2.3.4","ttl":600}"#,
            r#"{"host":"www","rtype":"A","value":"  ","ttl":600}"#,
            r#"{"host":"www","rtype":"A","value":"1.2.3.4","ttl":99999}"#,
        ] {
            let res = req(
                &app,
                "POST",
                &format!("/api/v1/domains/{domain_id}/records"),
                Some(bad),
                Some(&cookie),
            )
            .await;
            assert_eq!(res.status(), 400, "{bad}");
            let msg = body_json(res).await["error"]["message"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(!msg.is_empty());
        }
        assert!(
            !mock
                .take_calls()
                .await
                .contains(&"create_record".to_string()),
            "参数校验失败不应调用平台"
        );

        // 创建成功：平台回写 + 本地入库
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/domains/{domain_id}/records"),
            Some(r#"{"host":"api","rtype":"CNAME","value":"cdn.example.net","ttl":300}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 201);
        let body = body_json(res).await;
        assert!(
            body["provider_record_id"]
                .as_str()
                .unwrap()
                .starts_with("mock-")
        );
        let new_id = body["id"].as_i64().unwrap();
        let row = sqlx::query_as::<_, RecordRow>("SELECT * FROM records WHERE id = ?")
            .bind(new_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(row.host, "api");
        assert_eq!(row.value, "cdn.example.net");

        // 平台报错透传（Auth）→ 本地不产生记录
        mock.state.lock().await.fail_create = Some(ProviderError::Auth("权限不足".into()));
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/domains/{domain_id}/records"),
            Some(r#"{"host":"api2","rtype":"A","value":"2.2.2.2","ttl":600}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 400);
        assert!(
            body_json(res).await["error"]["message"]
                .as_str()
                .unwrap()
                .contains("权限不足")
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM records WHERE host = 'api2'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "平台失败时本地不应产生记录");
    }

    #[tokio::test]
    async fn record_weight_priority_validation_and_update() {
        let (state, mocks) = test_app_with_multi_mock().await;
        let mock = mocks.get("a").unwrap().clone();
        {
            let mut guard = mock.state.lock().await;
            *guard = MockState::default().with_zone("example.com");
        }
        let app = router(state.clone());
        let cookie = login_for_test(&app).await;
        let res = req(&app, "POST", "/api/v1/accounts",
            Some(r#"{"name":"测试","provider_type":"cloudflare","credentials":{"api_token":"token-aaaa-1111","mock_id":"a"}}"#),
            Some(&cookie)).await;
        let account_id = body_json(res).await["id"].as_i64().unwrap();
        req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{account_id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        wait_sync_done(&state.pool, account_id).await;
        let domain_id: i64 = sqlx::query_scalar("SELECT id FROM domains LIMIT 1")
            .fetch_one(&state.pool)
            .await
            .unwrap();

        // 创建时越界拒绝：weight 需在 -1~100，priority 需在 0~50
        for bad in [
            r#"{"host":"www","rtype":"A","value":"1.2.3.4","ttl":600,"weight":101}"#,
            r#"{"host":"www","rtype":"A","value":"1.2.3.4","ttl":600,"weight":-2}"#,
            r#"{"host":"www","rtype":"A","value":"1.2.3.4","ttl":600,"priority":51}"#,
            r#"{"host":"www","rtype":"A","value":"1.2.3.4","ttl":600,"priority":-1}"#,
        ] {
            let res = req(
                &app,
                "POST",
                &format!("/api/v1/domains/{domain_id}/records"),
                Some(bad),
                Some(&cookie),
            )
            .await;
            assert_eq!(res.status(), 400, "{bad}");
        }

        // 创建成功后再验证修改时越界同样拒绝，且不调用平台
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/domains/{domain_id}/records"),
            Some(r#"{"host":"www","rtype":"A","value":"1.2.3.4","ttl":600}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 201);
        let new_id = body_json(res).await["id"].as_i64().unwrap();
        mock.take_calls().await;

        for bad in [
            r#"{"weight":101}"#,
            r#"{"weight":-2}"#,
            r#"{"priority":51}"#,
            r#"{"priority":-1}"#,
        ] {
            let res = req(
                &app,
                "PATCH",
                &format!("/api/v1/records/{new_id}"),
                Some(bad),
                Some(&cookie),
            )
            .await;
            assert_eq!(res.status(), 400, "{bad}");
        }
        assert!(
            mock.take_calls().await.is_empty(),
            "越界校验失败不应调用平台"
        );

        // 清空权重（提交 -1）与设置优先级可写库（EdgeOne 适配器会把 -1/0 归一为空，
        // 由 providers::edgeone::tests::clear_weight_priority_via_api_is_normalized 覆盖）
        let res = req(
            &app,
            "PATCH",
            &format!("/api/v1/records/{new_id}"),
            Some(r#"{"weight":-1,"priority":10}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 204);
        let row = sqlx::query_as::<_, RecordRow>("SELECT * FROM records WHERE id = ?")
            .bind(new_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(row.weight, Some(-1));
        assert_eq!(row.priority, Some(10));
    }

    /// 火山引擎记录备注限制 ≤16 UTF-8 字符：创建与修改均拒绝，且不调用平台。
    #[tokio::test]
    async fn volcengine_remark_length_rejected() {
        let (state, mocks) = test_app_with_multi_mock().await;
        let mock = mocks.get("a").unwrap().clone();
        {
            let mut guard = mock.state.lock().await;
            *guard = MockState::default().with_zone("example.com");
        }
        let app = router(state.clone());
        let cookie = login_for_test(&app).await;
        let res = req(&app, "POST", "/api/v1/accounts",
            Some(r#"{"name":"火山测试","provider_type":"volcengine","credentials":{"access_key_id":"ak-aaaa-1111","access_key_secret":"sk-aaaa-2222","mock_id":"a"}}"#),
            Some(&cookie)).await;
        assert_eq!(res.status(), 201);
        let account_id = body_json(res).await["id"].as_i64().unwrap();
        req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{account_id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        wait_sync_done(&state.pool, account_id).await;
        let domain_id: i64 = sqlx::query_scalar("SELECT id FROM domains LIMIT 1")
            .fetch_one(&state.pool)
            .await
            .unwrap();

        // 创建时：17 字符（含中文按字符计）超长拒绝；16 字符以内放行
        mock.take_calls().await; // 清空账户创建/同步产生的历史平台调用
        let too_long = format!(
            r#"{{"host":"www","rtype":"A","value":"1.2.3.4","ttl":600,"remark":"{}"}}"#,
            "备".repeat(17)
        );
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/domains/{domain_id}/records"),
            Some(&too_long),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 400, "17 字符备注应被拒绝");
        assert!(
            body_json(res).await["error"]["message"]
                .as_str()
                .unwrap()
                .contains("16")
        );
        assert!(mock.take_calls().await.is_empty(), "超长备注不应调用平台");

        // 创建成功（16 字符），再验证修改时超长同样拒绝且不调用平台
        let ok16 = format!(
            r#"{{"host":"www","rtype":"A","value":"1.2.3.4","ttl":600,"remark":"{}"}}"#,
            "注".repeat(16)
        );
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/domains/{domain_id}/records"),
            Some(&ok16),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 201, "16 字符备注应放行");
        let record_id = body_json(res).await["id"].as_i64().unwrap();
        mock.take_calls().await;

        let patch = format!(r#"{{"remark":"{}"}}"#, "长".repeat(17));
        let res = req(
            &app,
            "PATCH",
            &format!("/api/v1/records/{record_id}"),
            Some(&patch),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 400, "修改时 17 字符备注应被拒绝");
        assert!(mock.take_calls().await.is_empty(), "超长备注不应调用平台");

        // 非 volcengine 平台不受 16 字符限制（对照：cloudflare 长备注正常走平台）
        let mock_b = mocks.get("b").unwrap().clone();
        {
            let mut guard = mock_b.state.lock().await;
            *guard = MockState::default().with_zone("example.com");
        }
        let res = req(&app, "POST", "/api/v1/accounts",
            Some(r#"{"name":"CF对照","provider_type":"cloudflare","credentials":{"api_token":"token-aaaa-1111","mock_id":"b"}}"#),
            Some(&cookie)).await;
        let cf_account = body_json(res).await["id"].as_i64().unwrap();
        req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{cf_account}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        wait_sync_done(&state.pool, cf_account).await;
        let cf_domain: i64 = sqlx::query_scalar(
            "SELECT d.id FROM domains d JOIN accounts a ON a.id = d.account_id \
             WHERE a.provider_type = 'cloudflare' LIMIT 1",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        let long = format!(
            r#"{{"host":"www","rtype":"A","value":"1.2.3.4","ttl":600,"remark":"{}"}}"#,
            "长".repeat(30)
        );
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/domains/{cf_domain}/records"),
            Some(&long),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 201, "非火山引擎平台不受 16 字符备注限制");
    }

    #[tokio::test]
    async fn update_status_delete_scenarios() {
        let (state, mocks) = test_app_with_multi_mock().await;
        let mock = mocks.get("a").unwrap().clone();
        {
            let mut guard = mock.state.lock().await;
            *guard = MockState::default().with_zone("example.com");
        }
        let app = router(state.clone());
        let cookie = login_for_test(&app).await;
        let res = req(&app, "POST", "/api/v1/accounts",
            Some(r#"{"name":"测试","provider_type":"cloudflare","credentials":{"api_token":"token-aaaa-1111","mock_id":"a"}}"#),
            Some(&cookie)).await;
        let account_id = body_json(res).await["id"].as_i64().unwrap();
        req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{account_id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        wait_sync_done(&state.pool, account_id).await;
        let (rec1_id, rec1_pid): (i64, String) =
            sqlx::query_as("SELECT id, provider_record_id FROM records WHERE host = 'www'")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        let (rec2_id, _): (i64, String) =
            sqlx::query_as("SELECT id, provider_record_id FROM records WHERE host = '@'")
                .fetch_one(&state.pool)
                .await
                .unwrap();

        // 修改值并回写
        let res = req(
            &app,
            "PATCH",
            &format!("/api/v1/records/{rec1_id}"),
            Some(r#"{"value":"9.9.9.9"}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 204);
        let row = sqlx::query_as::<_, RecordRow>("SELECT * FROM records WHERE id = ?")
            .bind(rec1_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(row.value, "9.9.9.9");
        {
            let guard = mock.state.lock().await;
            assert_eq!(
                guard.records["example.com"][0].value, "9.9.9.9",
                "平台侧应已回写"
            );
        }

        // 平台侧记录不存在：修改失败 + pending 标记 + 明确提示
        mock.state.lock().await.update_not_found = true;
        let res = req(
            &app,
            "PATCH",
            &format!("/api/v1/records/{rec2_id}"),
            Some(r#"{"value":"::2"}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 400);
        assert!(
            body_json(res).await["error"]["message"]
                .as_str()
                .unwrap()
                .contains("平台侧不存在")
        );
        let row = sqlx::query_as::<_, RecordRow>("SELECT * FROM records WHERE id = ?")
            .bind(rec2_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(row.pending_sync, 1, "应标记待同步");
        mock.state.lock().await.update_not_found = false;

        // 启停：成功 / 不支持
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/records/{rec1_id}/status"),
            Some(r#"{"enabled":false}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 204);
        let row = sqlx::query_as::<_, RecordRow>("SELECT * FROM records WHERE id = ?")
            .bind(rec1_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(row.status, "disabled");
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/records/{rec1_id}/status"),
            Some(r#"{"enabled":true}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 204);

        mock.state.lock().await.unsupported_set_status = true;
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/records/{rec1_id}/status"),
            Some(r#"{"enabled":false}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 409);
        assert!(
            body_json(res).await["error"]["message"]
                .as_str()
                .unwrap()
                .contains("不支持")
        );
        mock.state.lock().await.unsupported_set_status = false;

        // 删除：平台 NotFound 时幂等成功
        mock.state.lock().await.delete_not_found = true;
        let res = req(
            &app,
            "DELETE",
            &format!("/api/v1/records/{rec2_id}"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 204);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM records WHERE id = ?")
            .bind(rec2_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        mock.state.lock().await.delete_not_found = false;

        // 正常删除
        let res = req(
            &app,
            "DELETE",
            &format!("/api/v1/records/{rec1_id}"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 204);
        {
            let guard = mock.state.lock().await;
            assert!(
                !guard.records["example.com"]
                    .iter()
                    .any(|r| r.provider_record_id == rec1_pid)
            );
        }

        // 不存在的记录 → 404
        let res = req(&app, "DELETE", "/api/v1/records/9999", None, Some(&cookie)).await;
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn domain_and_record_queries() {
        let (state, mocks) = test_app_with_multi_mock().await;
        let mock = mocks.get("a").unwrap().clone();
        {
            let mut guard = mock.state.lock().await;
            *guard = MockState::default().with_zone("example.com");
        }
        let app = router(state.clone());
        let cookie = login_for_test(&app).await;
        let res = req(&app, "POST", "/api/v1/accounts",
            Some(r#"{"name":"测试","provider_type":"cloudflare","credentials":{"api_token":"token-aaaa-1111","mock_id":"a"}}"#),
            Some(&cookie)).await;
        let account_id = body_json(res).await["id"].as_i64().unwrap();
        req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{account_id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        wait_sync_done(&state.pool, account_id).await;
        let domain_id: i64 = sqlx::query_scalar("SELECT id FROM domains LIMIT 1")
            .fetch_one(&state.pool)
            .await
            .unwrap();

        // 域名列表：标注平台与记录数；keyword / provider 过滤；分页字段
        let res = req(&app, "GET", "/api/v1/domains", None, Some(&cookie)).await;
        let body = body_json(res).await;
        assert_eq!(body["total"], 1);
        assert_eq!(body["page"], 1);
        assert_eq!(body["page_size"], 10);
        let items = body["items"].as_array().unwrap().clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["provider_type"], "cloudflare");
        assert_eq!(items[0]["record_count"], 2);

        let res = req(
            &app,
            "GET",
            "/api/v1/domains?keyword=nothing",
            None,
            Some(&cookie),
        )
        .await;
        let body = body_json(res).await;
        assert_eq!(body["total"], 0);
        assert_eq!(body["items"].as_array().unwrap().len(), 0);
        let res = req(
            &app,
            "GET",
            "/api/v1/domains?provider=aliyun",
            None,
            Some(&cookie),
        )
        .await;
        let body = body_json(res).await;
        assert_eq!(body["total"], 0);
        assert_eq!(body["items"].as_array().unwrap().len(), 0);

        // 域名列表分页：指定页与每页条数；page 超出范围返回空 items
        let res = req(
            &app,
            "GET",
            "/api/v1/domains?page=1&page_size=1",
            None,
            Some(&cookie),
        )
        .await;
        let body = body_json(res).await;
        assert_eq!(body["total"], 1);
        assert_eq!(body["page"], 1);
        assert_eq!(body["page_size"], 1);
        assert_eq!(body["items"].as_array().unwrap().len(), 1);

        let res = req(
            &app,
            "GET",
            "/api/v1/domains?page=2&page_size=1",
            None,
            Some(&cookie),
        )
        .await;
        let body = body_json(res).await;
        assert_eq!(body["total"], 1);
        assert_eq!(body["items"].as_array().unwrap().len(), 0);

        // page_size 超限截断为 200
        let res = req(
            &app,
            "GET",
            "/api/v1/domains?page_size=999",
            None,
            Some(&cookie),
        )
        .await;
        let body = body_json(res).await;
        assert_eq!(body["page_size"], 200);
        assert_eq!(body["items"].as_array().unwrap().len(), 1);

        // 分页与过滤同时生效
        let res = req(
            &app,
            "GET",
            "/api/v1/domains?keyword=example&page=1&page_size=20",
            None,
            Some(&cookie),
        )
        .await;
        let body = body_json(res).await;
        assert_eq!(body["total"], 1);
        assert_eq!(body["items"].as_array().unwrap().len(), 1);

        // 记录过滤与分页
        let res = req(
            &app,
            "GET",
            &format!("/api/v1/domains/{domain_id}/records?type=A"),
            None,
            Some(&cookie),
        )
        .await;
        let body = body_json(res).await;
        assert_eq!(body["total"], 1);
        assert_eq!(body["items"].as_array().unwrap()[0]["rtype"], "A");

        let res = req(
            &app,
            "GET",
            &format!("/api/v1/domains/{domain_id}/records?keyword=www"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(body_json(res).await["total"], 1);

        let res = req(
            &app,
            "GET",
            &format!("/api/v1/domains/{domain_id}/records?page=1&page_size=1"),
            None,
            Some(&cookie),
        )
        .await;
        let body = body_json(res).await;
        assert_eq!(body["total"], 2);
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        let res = req(
            &app,
            "GET",
            &format!("/api/v1/domains/{domain_id}/records?page=2&page_size=1"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(body_json(res).await["items"].as_array().unwrap().len(), 1);

        // 详情
        let res = req(
            &app,
            "GET",
            &format!("/api/v1/domains/{domain_id}"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(body_json(res).await["name"], "example.com");
    }
}
