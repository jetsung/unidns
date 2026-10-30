use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    error::{ApiResult, AppError},
    models::{AccountRow, now_utc},
    sync,
};

use super::{ApiJson, SharedState};

/// POST /api/v1/sync —— 异步启动全量同步（所有已启用账户），立即返回任务列表。
/// 前端用返回的 run_id 轮询 GET /sync/runs/{id} 获取结果。
pub async fn sync_now(State(state): State<SharedState>) -> ApiResult<(StatusCode, Json<Value>)> {
    let started = sync::start_sync_all(&state).await.map_err(|skipped| {
        if skipped.is_empty() {
            AppError::BadRequest("没有可同步的已启用账户，或所有账户均有任务在执行".into())
        } else {
            AppError::BadRequest("所有账户均有同步任务在执行，请等待完成".into())
        }
    })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "message": "后台同步已开始",
            "started": started,
        })),
    ))
}

/// POST /api/v1/accounts/{id}/sync —— 异步启动单账户同步，立即返回 run_id。
pub async fn sync_one(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let account = sqlx::query_as::<_, AccountRow>("SELECT * FROM accounts WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(AppError::NotFound)?;
    if !account.enabled() {
        return Err(AppError::BadRequest(
            "账户已停用，停用账户不参与同步；请先启用账户".into(),
        ));
    }
    let run_id = sync::start_sync_account(&state, &account)
        .await
        .map_err(AppError::BadRequest)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "message": "后台同步已开始",
            "run_id": run_id,
            "account_id": account.id,
        })),
    ))
}

/// GET /api/v1/sync/runs/{id} —— 查询单次同步任务状态（前端轮询用）。
pub async fn get_run(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    #[derive(sqlx::FromRow)]
    struct Run {
        id: i64,
        account_id: Option<i64>,
        status: String,
        error: Option<String>,
        stats_json: Option<String>,
        started_at: String,
        finished_at: Option<String>,
    }
    let r = sqlx::query_as::<_, Run>(
        "SELECT id, account_id, status, error, stats_json, started_at, finished_at FROM sync_runs WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(json!({
        "id": r.id,
        "account_id": r.account_id,
        "status": r.status,
        "error": r.error,
        "stats": r.stats_json.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()),
        "started_at": r.started_at,
        "finished_at": r.finished_at,
    })))
}

#[derive(Deserialize)]
pub struct RunQuery {
    pub account_id: Option<i64>,
    pub limit: Option<i64>,
}

/// GET /api/v1/sync/runs —— 同步历史（可按账户过滤）。
pub async fn list_runs(
    State(state): State<SharedState>,
    Query(q): Query<RunQuery>,
) -> ApiResult<Json<Value>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    #[derive(sqlx::FromRow)]
    struct RunWithAccount {
        id: i64,
        account_id: Option<i64>,
        account_name: Option<String>,
        scope: String,
        started_at: String,
        finished_at: Option<String>,
        status: String,
        error: Option<String>,
        stats_json: Option<String>,
    }
    let rows = sqlx::query_as::<_, RunWithAccount>(
        "SELECT s.id, s.account_id, a.name AS account_name, s.scope, s.started_at, \
                s.finished_at, s.status, s.error, s.stats_json \
         FROM sync_runs s LEFT JOIN accounts a ON a.id = s.account_id \
         WHERE (? IS NULL OR s.account_id = ?) \
         ORDER BY s.id DESC LIMIT ?",
    )
    .bind(q.account_id)
    .bind(q.account_id)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;

    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "account_id": r.account_id,
                "account_name": r.account_name,
                "scope": r.scope,
                "started_at": r.started_at,
                "finished_at": r.finished_at,
                "status": r.status,
                "error": r.error,
                "stats": r.stats_json.as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok()),
            })
        })
        .collect();
    Ok(Json(json!({"items": items})))
}

#[derive(Deserialize)]
pub struct SyncSettingsRequest {
    pub interval_minutes: i64,
}

/// GET /api/v1/sync/settings —— 读取同步间隔。
pub async fn get_settings(State(state): State<SharedState>) -> ApiResult<Json<Value>> {
    let interval = sync::read_interval_minutes(&state).await;
    Ok(Json(json!({"interval_minutes": interval})))
}

/// PUT /api/v1/sync/settings —— 调整同步间隔（下一个调度周期生效）。
pub async fn update_settings(
    State(state): State<SharedState>,
    ApiJson(req): ApiJson<SyncSettingsRequest>,
) -> ApiResult<StatusCode> {
    if req.interval_minutes < 0 || (req.interval_minutes > 0 && req.interval_minutes < 30) {
        return Err(AppError::BadRequest(
            "同步间隔为 0（停用自动同步）或至少 30 分钟".into(),
        ));
    }
    crate::db::set_setting(
        &state.pool,
        sync::SYNC_INTERVAL_KEY,
        &req.interval_minutes.to_string(),
    )
    .await?;
    tracing::info!(interval_minutes = req.interval_minutes, "同步间隔已调整");
    Ok(StatusCode::NO_CONTENT)
}

// ---------- 记录同步时间戳（供账户页展示，复用 now_utc 保证一致） ----------

#[allow(dead_code)]
fn _touch() -> String {
    now_utc()
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

    /// 轮询等待后台同步任务结束，返回终态（success/failed）。
    async fn wait_run_done(state: &SharedState, run_id: i64) -> String {
        for _ in 0..100 {
            let (status,): (String,) = sqlx::query_as("SELECT status FROM sync_runs WHERE id = ?")
                .bind(run_id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
            if status != "running" {
                return status;
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

    const CF_CREDS_A: &str = r#"{"name":"A","provider_type":"cloudflare","credentials":{"api_token":"token-aaaa-1111","mock_id":"a"}}"#;
    const CF_CREDS_B: &str = r#"{"name":"B","provider_type":"cloudflare","credentials":{"api_token":"token-bbbb-2222","mock_id":"b"}}"#;

    async fn create_account(app: &axum::Router, cookie: &str, body: &str) -> i64 {
        let res = req(app, "POST", "/api/v1/accounts", Some(body), Some(cookie)).await;
        assert_eq!(res.status(), 201, "创建账户失败");
        body_json(res).await["id"].as_i64().unwrap()
    }

    #[tokio::test]
    async fn single_account_sync_reconciles() {
        let (state, mocks) = test_app_with_multi_mock().await;
        let mock_a = mocks.get("a").unwrap().clone();
        {
            let mut guard = mock_a.state.lock().await;
            *guard = MockState::default().with_zone("example.com");
        }
        let app = router(state.clone());
        let cookie = login_for_test(&app).await;
        let id = create_account(&app, &cookie, CF_CREDS_A).await;

        // 1) 首次同步（异步启动 → 轮询完成）：域名与记录入库
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 202, "异步启动应返回 202");
        let body = body_json(res).await;
        let run_id = body["run_id"].as_i64().unwrap();
        assert_eq!(wait_run_done(&state, run_id).await, "success");
        let (domains, records): (i64, i64) =
            sqlx::query_as("SELECT (SELECT COUNT(*) FROM domains), (SELECT COUNT(*) FROM records)")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!((domains, records), (1, 2));

        // 2) 平台侧新增一条记录 + 修改一条 + 删除一条 → 再同步对账
        {
            let mut guard = mock_a.state.lock().await;
            let list = guard.records.get_mut("example.com").unwrap();
            list.push(crate::providers::ProviderRecord {
                provider_record_id: "rec-new".into(),
                host: "api".into(),
                rtype: "CNAME".into(),
                value: "cdn.example.net".into(),
                ttl: 300,
                line: None,
                proxied: Some(false),
                enabled: true,
                remark: None,
                weight: None,
                priority: None,
            });
            list[0].value = "9.9.9.9".into(); // 变更 rec-...-1
            list.remove(1); // 删除 rec-...-2（孤儿）
        }
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 202);
        let run_id = body_json(res).await["run_id"].as_i64().unwrap();
        assert_eq!(wait_run_done(&state, run_id).await, "success");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM records")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(count, 2, "平台为准：2 条");
        let values: Vec<(String, String)> = sqlx::query_as(
            "SELECT provider_record_id, value FROM records ORDER BY provider_record_id",
        )
        .fetch_all(&state.pool)
        .await
        .unwrap();
        assert!(
            values
                .iter()
                .any(|(pid, v)| pid == "rec-new" && v == "cdn.example.net"),
            "{values:?}"
        );
        assert!(
            values
                .iter()
                .any(|(pid, v)| pid.ends_with("-1") && v == "9.9.9.9"),
            "{values:?}"
        );
        assert!(
            !values.iter().any(|(pid, _)| pid.ends_with("-2")),
            "孤儿记录应被移除: {values:?}"
        );

        // 3) 同步失败：fail_list_domains → 记录失败原因，不影响已有数据
        mock_a.state.lock().await.fail_list_domains =
            Some(ProviderError::Auth("Token 失效".into()));
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 202);
        let body = body_json(res).await;
        let run_id = body["run_id"].as_i64().unwrap();
        assert_eq!(wait_run_done(&state, run_id).await, "failed");

        // sync_runs 历史记录了失败原因
        let res = req(&app, "GET", "/api/v1/sync/runs", None, Some(&cookie)).await;
        let runs = body_json(res).await["items"].as_array().unwrap().clone();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0]["status"], "failed");
        assert!(runs[0]["error"].as_str().unwrap().contains("失效"));
        assert_eq!(runs[2]["status"], "success");
    }

    #[tokio::test]
    async fn full_sync_partial_failure_and_disabled_skip() {
        let (state, mocks) = test_app_with_multi_mock().await;
        let mock_a = mocks.get("a").unwrap().clone();
        let mock_b = mocks.get("b").unwrap().clone();
        {
            let mut guard = mock_a.state.lock().await;
            *guard = MockState::default().with_zone("a.com");
        }
        mock_b.state.lock().await.fail_list_domains =
            Some(ProviderError::Network("connection refused".into()));

        let app = router(state.clone());
        let cookie = login_for_test(&app).await;
        let id_a = create_account(&app, &cookie, CF_CREDS_A).await;
        let id_b = create_account(&app, &cookie, CF_CREDS_B).await;
        // 第三个账户：停用，不应参与
        let id_c = create_account(
            &app,
            &cookie,
            r#"{"name":"C","provider_type":"dnspod","credentials":{"secret_id":"id-cccc-3333","secret_key":"sk-cccc","mock_id":"a"}}"#,
        )
        .await;
        let res = req(
            &app,
            "PATCH",
            &format!("/api/v1/accounts/{id_c}"),
            Some(r#"{"status":"disabled"}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 204);

        // 停用账户手动同步 → 400
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{id_c}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 400);

        // 全量同步（异步启动）：A 成功、B 失败、C 跳过（停用）
        let res = req(&app, "POST", "/api/v1/sync", None, Some(&cookie)).await;
        assert_eq!(res.status(), 202);
        let body = body_json(res).await;
        let started = body["started"].as_array().unwrap();
        assert_eq!(started.len(), 2, "停用账户不参与: {body}");
        let mut a_run = 0i64;
        let mut b_run = 0i64;
        for s in started {
            if s["account_id"].as_i64() == Some(id_a) {
                a_run = s["run_id"].as_i64().unwrap();
            }
            if s["account_id"].as_i64() == Some(id_b) {
                b_run = s["run_id"].as_i64().unwrap();
            }
        }
        assert_ne!(a_run, 0);
        assert_ne!(b_run, 0);
        assert_eq!(wait_run_done(&state, a_run).await, "success");
        assert_eq!(wait_run_done(&state, b_run).await, "failed");
        // B 的失败原因入库
        let b_err: String =
            sqlx::query_scalar("SELECT COALESCE(error,'') FROM sync_runs WHERE id = ?")
                .bind(b_run)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert!(b_err.contains("connection refused"), "{b_err}");

        // 按账户过滤同步历史
        let res = req(
            &app,
            "GET",
            &format!("/api/v1/sync/runs?account_id={id_a}"),
            None,
            Some(&cookie),
        )
        .await;
        let runs = body_json(res).await["items"].as_array().unwrap().clone();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["status"], "success");
        assert_eq!(runs[0]["account_name"], "A");

        // 新增：单次任务查询接口
        let res = req(
            &app,
            "GET",
            &format!("/api/v1/sync/runs/{}", runs[0]["id"]),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 200);
        assert_eq!(body_json(res).await["status"], "success");
    }

    #[tokio::test]
    async fn sync_skips_rejected_site_and_keeps_others() {
        let (state, mocks) = test_app_with_multi_mock().await;
        let mock = mocks.get("a").unwrap().clone();
        {
            let mut guard = mock.state.lock().await;
            guard.domains = vec![
                crate::providers::ProviderDomain {
                    name: "a.com".into(),
                    provider_domain_id: "z-a".into(),
                    status: "active".into(),
                    remark: None,
                },
                crate::providers::ProviderDomain {
                    name: "b.com".into(),
                    provider_domain_id: "z-b".into(),
                    status: "active".into(),
                    remark: None,
                },
            ];
            guard.records.insert(
                "a.com".into(),
                vec![crate::providers::ProviderRecord {
                    provider_record_id: "ra1".into(),
                    host: "www".into(),
                    rtype: "A".into(),
                    value: "1.1.1.1".into(),
                    ttl: 600,
                    line: None,
                    proxied: None,
                    enabled: true,
                    remark: None,
                    weight: None,
                    priority: None,
                }],
            );
            guard.records.insert(
                "b.com".into(),
                vec![crate::providers::ProviderRecord {
                    provider_record_id: "rb1".into(),
                    host: "www".into(),
                    rtype: "A".into(),
                    value: "2.2.2.2".into(),
                    ttl: 600,
                    line: None,
                    proxied: None,
                    enabled: true,
                    remark: None,
                    weight: None,
                    priority: None,
                }],
            );
            // 第一次记录拉取（a.com）被拒，b.com 正常
            guard.fail_list_records = Some(ProviderError::Auth("无权限".into()));
        }
        let app = router(state.clone());
        let cookie = login_for_test(&app).await;
        let res = req(&app, "POST", "/api/v1/accounts",
            Some(r#"{"name":"部分站点","provider_type":"cloudflare","credentials":{"api_token":"token-aaaa-1111","mock_id":"a"}}"#),
            Some(&cookie)).await;
        let account_id = body_json(res).await["id"].as_i64().unwrap();

        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{account_id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 202);
        let body = body_json(res).await;
        let run_id = body["run_id"].as_i64().unwrap();
        // 部分站点失败但整体成功（跳过机制）
        assert_eq!(wait_run_done(&state, run_id).await, "success");

        // 被拒站点记录为 0，正常站点有记录
        let (a_count, b_count): (i64, i64) = sqlx::query_as(
            "SELECT \
             (SELECT COUNT(*) FROM records r JOIN domains d ON d.id = r.domain_id WHERE d.name = 'a.com'), \
             (SELECT COUNT(*) FROM records r JOIN domains d ON d.id = r.domain_id WHERE d.name = 'b.com')",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        assert_eq!((a_count, b_count), (0, 1), "只有 b.com 的记录入库");

        // 同步历史的 error 字段记录了跳过详情
        let uri = format!("/api/v1/sync/runs?account_id={account_id}");
        let res = req(&app, "GET", &uri, None, Some(&cookie)).await;
        let runs = body_json(res).await["items"].as_array().unwrap().clone();
        assert_eq!(runs[0]["status"], "success");
        assert!(
            runs[0]["error"].as_str().unwrap_or("").contains("a.com"),
            "跳过详情应入历史: {runs:?}"
        );
    }

    #[tokio::test]
    async fn sync_settings_roundtrip() {
        let (state, _mocks) = test_app_with_multi_mock().await;
        let app = router(state.clone());
        let cookie = login_for_test(&app).await;

        // 默认值（0 = 停用自动同步）
        let res = req(&app, "GET", "/api/v1/sync/settings", None, Some(&cookie)).await;
        assert_eq!(body_json(res).await["interval_minutes"], 0);

        // 调整间隔（启用时最低 30 分钟）
        let res = req(
            &app,
            "PUT",
            "/api/v1/sync/settings",
            Some(r#"{"interval_minutes": 30}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 204);
        let res = req(&app, "GET", "/api/v1/sync/settings", None, Some(&cookie)).await;
        assert_eq!(body_json(res).await["interval_minutes"], 30);

        // 0 被接受（表示停用自动同步）
        let res = req(
            &app,
            "PUT",
            "/api/v1/sync/settings",
            Some(r#"{"interval_minutes": 0}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 204);

        // 1-29 被拒绝
        let res = req(
            &app,
            "PUT",
            "/api/v1/sync/settings",
            Some(r#"{"interval_minutes": 10}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 400);

        // 负数被拒绝
        let res = req(
            &app,
            "PUT",
            "/api/v1/sync/settings",
            Some(r#"{"interval_minutes": -1}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn sync_overwrite_aborts_running_sync() {
        let (state, mocks) = test_app_with_multi_mock().await;
        let mock_a = mocks.get("a").unwrap().clone();
        {
            let mut guard = mock_a.state.lock().await;
            *guard = MockState::default().with_zone("edgeone-demo.com");
            guard.delay_list_domains_ms = 800;
        }
        let app = router(state.clone());
        let cookie = login_for_test(&app).await;
        // EdgeOne 账户场景：覆盖中止语义同样适用
        let body = r#"{"name":"EO","provider_type":"edgeone","credentials":{"secret_id":"id","secret_key":"key","mock_id":"a"}}"#;
        let id = create_account(&app, &cookie, body).await;

        // 第一次同步启动后保持 running
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 202);
        let first_run = body_json(res).await["run_id"].as_i64().unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (status,): (String,) = sqlx::query_as("SELECT status FROM sync_runs WHERE id = ?")
            .bind(first_run)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(status, "running", "第一次同步应仍在进行中");

        // 再次触发：覆盖语义 → 第一次被中止标记失败（被新同步覆盖），第二次执行成功
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 202);
        let second_run = body_json(res).await["run_id"].as_i64().unwrap();
        assert_ne!(first_run, second_run);
        assert_eq!(wait_run_done(&state, second_run).await, "success");

        let (s, e): (String, String) =
            sqlx::query_as("SELECT status, COALESCE(error,'') FROM sync_runs WHERE id = ?")
                .bind(first_run)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(s, "failed", "被覆盖的同步应标记失败");
        assert_eq!(e, "被新同步覆盖", "失败原因应为“被新同步覆盖”");

        // 覆盖完成后可再次正常同步（无卡死、无残留锁）
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status(), 202);
        let third_run = body_json(res).await["run_id"].as_i64().unwrap();
        assert_eq!(wait_run_done(&state, third_run).await, "success");
    }

    #[tokio::test]
    async fn sync_remark_follows_provider_capability() {
        let (state, mocks) = test_app_with_multi_mock().await;
        let mock_a = mocks.get("a").unwrap().clone();
        {
            let mut guard = mock_a.state.lock().await;
            // rec-1 无平台备注，rec-2 带平台备注「演示备注」
            *guard = MockState::default().with_zone("remark.com");
        }
        let app = router(state.clone());
        let cookie = login_for_test(&app).await;
        let body = r#"{"name":"A","provider_type":"aliyun","credentials":{"access_key_id":"id","access_key_secret":"key","mock_id":"a"}}"#;
        let id = create_account(&app, &cookie, body).await;

        // 首次同步：平台备注入库
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        let run_id = body_json(res).await["run_id"].as_i64().unwrap();
        assert_eq!(wait_run_done(&state, run_id).await, "success");
        let r2: String = sqlx::query_scalar(
            "SELECT remark FROM records WHERE provider_record_id = 'rec-remark.com-2'",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        assert_eq!(r2, "演示备注", "平台备注应首次入库");

        // rec-1 本地填写备注；rec-2 本地清空备注
        sqlx::query("UPDATE records SET remark = '本地保护备注' WHERE provider_record_id = 'rec-remark.com-1'")
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE records SET remark = '' WHERE provider_record_id = 'rec-remark.com-2'")
            .execute(&state.pool)
            .await
            .unwrap();
        // 平台侧：rec-1 出现备注（如平台新写入），rec-2 备注被改动
        {
            let mut guard = mock_a.state.lock().await;
            let list = guard.records.get_mut("remark.com").unwrap();
            list[0].remark = Some("平台已改动".to_string());
            list[1].remark = Some("平台已改动".to_string());
        }

        // 再次同步
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        let run_id = body_json(res).await["run_id"].as_i64().unwrap();
        assert_eq!(wait_run_done(&state, run_id).await, "success");

        // 支持备注的平台：以上游为准（含平台侧改动/清空）
        let remarks: Vec<String> = sqlx::query_scalar(
            "SELECT remark FROM records WHERE domain_id = (SELECT id FROM domains WHERE name = 'remark.com') ORDER BY provider_record_id",
        )
        .fetch_all(&state.pool)
        .await
        .unwrap();
        assert_eq!(remarks.len(), 2);
        assert_eq!(remarks[0], "平台已改动", "支持备注的平台应以上游为准");
        assert_eq!(remarks[1], "平台已改动", "平台改动应同步到本地");

        // 不支持备注的平台：保留本地备注，不被上游空值覆盖
        mock_a.disable_record_remark();
        sqlx::query("UPDATE records SET remark = '本地保护备注' WHERE provider_record_id = 'rec-remark.com-1'")
            .execute(&state.pool)
            .await
            .unwrap();
        {
            let mut guard = mock_a.state.lock().await;
            let list = guard.records.get_mut("remark.com").unwrap();
            list[0].remark = None;
        }
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        let run_id = body_json(res).await["run_id"].as_i64().unwrap();
        assert_eq!(wait_run_done(&state, run_id).await, "success");
        let r1: String = sqlx::query_scalar(
            "SELECT remark FROM records WHERE provider_record_id = 'rec-remark.com-1'",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        assert_eq!(r1, "本地保护备注", "上游无备注字段时应保留本地备注");
    }

    #[tokio::test]
    async fn domain_remark_syncs_to_upstream() {
        let (state, mocks) = test_app_with_multi_mock().await;
        let mock_a = mocks.get("a").unwrap().clone();
        {
            let mut guard = mock_a.state.lock().await;
            *guard = MockState::default().with_zone("domain-remark.com");
        }
        let app = router(state.clone());
        let cookie = login_for_test(&app).await;
        // DNSPod 支持域名备注，mock 会记录回写调用
        let body = r#"{"name":"D","provider_type":"dnspod","credentials":{"secret_id":"id","secret_key":"key","mock_id":"a"}}"#;
        let id = create_account(&app, &cookie, body).await;

        // 同步入库域名
        let res = req(
            &app,
            "POST",
            &format!("/api/v1/accounts/{id}/sync"),
            None,
            Some(&cookie),
        )
        .await;
        let run_id = body_json(res).await["run_id"].as_i64().unwrap();
        assert_eq!(wait_run_done(&state, run_id).await, "success");
        let domain_id: i64 =
            sqlx::query_scalar("SELECT id FROM domains WHERE name = 'domain-remark.com'")
                .fetch_one(&state.pool)
                .await
                .unwrap();

        // 设置备注：上游应收到回写，本地入库
        let res = req(
            &app,
            "PATCH",
            &format!("/api/v1/domains/{domain_id}"),
            Some(r#"{"remark":"上游备注"}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status().as_u16(), 200);
        assert_eq!(
            mock_a
                .state
                .lock()
                .await
                .domain_remarks
                .get("domain-remark.com")
                .cloned(),
            Some("上游备注".into()),
            "DNSPod 域名备注应回写上游"
        );
        let local: String = sqlx::query_scalar("SELECT remark FROM domains WHERE id = ?")
            .bind(domain_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(local, "上游备注", "回写成功后本地应入库");

        // 清空备注：空值同样同步上游并清空本地
        let res = req(
            &app,
            "PATCH",
            &format!("/api/v1/domains/{domain_id}"),
            Some(r#"{"remark":""}"#),
            Some(&cookie),
        )
        .await;
        assert_eq!(res.status().as_u16(), 200);
        assert_eq!(
            mock_a
                .state
                .lock()
                .await
                .domain_remarks
                .get("domain-remark.com")
                .map(String::as_str),
            Some(""),
            "空值备注应同步上游清空"
        );
        let local: String = sqlx::query_scalar("SELECT remark FROM domains WHERE id = ?")
            .bind(domain_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(local, "", "本地备注应清空");
    }
}
