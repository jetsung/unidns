use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use sqlx::SqlitePool;
use tokio::sync::Semaphore;

use crate::{
    models::{AccountRow, DomainRow, now_utc},
    providers::{DnsProvider, ProviderError, ProviderZone},
    web::{SharedState, accounts::decrypt_credentials},
};

/// 单账户同步结果（用于 API 汇总与界面展示）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncOutcome {
    pub account_id: i64,
    pub account_name: String,
    pub success: bool,
    pub message: String,
    pub domains: usize,
    pub records: usize,
}

pub(crate) fn build_provider_for_account(
    state: &SharedState,
    account: &AccountRow,
) -> Result<Box<dyn DnsProvider>, crate::error::AppError> {
    let cred_json = decrypt_credentials(state, account)?;
    let ptype = account.provider_type().ok_or_else(|| {
        crate::error::AppError::Internal(format!("未知的 provider_type: {}", account.provider_type))
    })?;
    (state.provider_factory)(ptype, &cred_json).map_err(crate::error::AppError::Provider)
}

/// 平台限流时的退避重试（一次）。
async fn with_rate_limit_retry<F, Fut, T>(f: F) -> Result<T, ProviderError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, ProviderError>>,
{
    match f().await {
        Err(ProviderError::RateLimited {
            retry_after,
            message,
        }) => {
            let wait = retry_after.unwrap_or(5).min(60);
            tracing::warn!(wait_secs = wait, %message, "平台限流，退避后重试一次");
            tokio::time::sleep(Duration::from_secs(wait)).await;
            f().await
        }
        other => other,
    }
}

fn retry_hint(e: &ProviderError) -> String {
    match e {
        ProviderError::Auth(m) => format!("凭据失效: {m}"),
        ProviderError::RateLimited { message, .. } => format!("平台限流: {message}"),
        ProviderError::Network(m) => format!("网络错误: {m}"),
        ProviderError::NotFound(m) => format!("资源不存在: {m}"),
        ProviderError::Unsupported(m) => m.clone(),
        ProviderError::Api { code, message } => format!("[{code}] {message}"),
    }
}

/// 对账落库：以平台返回为准 upsert 域名与记录，并移除本地孤儿。
async fn reconcile(
    pool: &SqlitePool,
    account_id: i64,
    provider: &dyn DnsProvider,
    domains: Vec<crate::providers::ProviderDomain>,
) -> Result<(usize, usize, Vec<(String, String)>), ProviderError> {
    let now = now_utc();

    // --- 域名对账：upsert 平台域名并记录其数据库 id ---
    let mut platform_names: HashSet<String> = HashSet::new();
    let mut domain_db_ids: Vec<(String, i64)> = Vec::new();
    for d in &domains {
        platform_names.insert(d.name.clone());
        sqlx::query(
            "INSERT INTO domains (account_id, name, provider_domain_id, status, synced_at, remark) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(account_id, name) DO UPDATE SET \
                provider_domain_id = excluded.provider_domain_id, \
                status = excluded.status, \
                remark = CASE WHEN domains.remark IS NOT NULL AND domains.remark != '' \
                              THEN domains.remark ELSE excluded.remark END, \
                synced_at = excluded.synced_at",
        )
        .bind(account_id)
        .bind(&d.name)
        .bind(&d.provider_domain_id)
        .bind(&d.status)
        .bind(&now)
        .bind(d.remark.clone().unwrap_or_default())
        .execute(pool)
        .await
        .map_err(|e| ProviderError::Api {
            code: "LocalDb".into(),
            message: format!("写入域名失败: {e}"),
        })?;
        let db_id: i64 =
            sqlx::query_scalar("SELECT id FROM domains WHERE account_id = ? AND name = ?")
                .bind(account_id)
                .bind(&d.name)
                .fetch_one(pool)
                .await
                .map_err(|e| ProviderError::Api {
                    code: "LocalDb".into(),
                    message: format!("读取域名 id 失败: {e}"),
                })?;
        domain_db_ids.push((d.name.clone(), db_id));
    }
    let local_domains: Vec<DomainRow> =
        sqlx::query_as("SELECT * FROM domains WHERE account_id = ?")
            .bind(account_id)
            .fetch_all(pool)
            .await
            .map_err(|e| ProviderError::Api {
                code: "LocalDb".into(),
                message: format!("读取域名失败: {e}"),
            })?;
    for d in &local_domains {
        if !platform_names.contains(&d.name) {
            sqlx::query("DELETE FROM domains WHERE id = ?")
                .bind(d.id)
                .execute(pool)
                .await
                .map_err(|e| ProviderError::Api {
                    code: "LocalDb".into(),
                    message: format!("删除孤儿域名失败: {e}"),
                })?;
        }
    }

    // --- 记录对账 ---
    // 支持备注的平台以上游为准（含清空）；不支持备注的平台保留本地备注
    let remark_update = if provider.supports_record_remark() {
        "excluded.remark"
    } else {
        "CASE WHEN records.remark IS NOT NULL AND records.remark != '' \
         THEN records.remark ELSE excluded.remark END"
    };
    let record_upsert = format!(
        "INSERT INTO records (domain_id, account_id, provider_record_id, host, rtype, value, ttl, line, proxied, status, pending_sync, remark, synced_at, weight, priority) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?, ?) \
         ON CONFLICT(domain_id, provider_record_id) DO UPDATE SET \
            host = excluded.host, rtype = excluded.rtype, value = excluded.value, \
            ttl = excluded.ttl, line = excluded.line, proxied = excluded.proxied, \
            status = excluded.status, pending_sync = 0, \
            weight = excluded.weight, priority = excluded.priority, \
            remark = {remark_update}, \
            synced_at = excluded.synced_at"
    );
    let mut total_records = 0usize;
    let mut skipped: Vec<(String, String)> = Vec::new();
    for (domain_name, domain_db_id) in &domain_db_ids {
        let zone = ProviderZone {
            name: domain_name.clone(),
            provider_domain_id: String::new(),
        };
        let zone = domains
            .iter()
            .find(|d| &d.name == domain_name)
            .map(|d| ProviderZone {
                name: domain_name.clone(),
                provider_domain_id: d.provider_domain_id.clone(),
            })
            .unwrap_or(zone);
        let records = match with_rate_limit_retry(|| provider.list_records(&zone)).await {
            Ok(r) => r,
            Err(e) => {
                let reason = retry_hint(&e);
                tracing::warn!(account_id, site = %domain_name, %reason, "站点记录拉取被拒，跳过该站点");
                skipped.push((domain_name.clone(), reason));
                continue;
            }
        };
        let mut platform_ids: HashSet<String> = HashSet::new();
        for r in &records {
            platform_ids.insert(r.provider_record_id.clone());
            sqlx::query(&record_upsert)
                .bind(domain_db_id)
                .bind(account_id)
                .bind(&r.provider_record_id)
                .bind(&r.host)
                .bind(&r.rtype)
                .bind(&r.value)
                .bind(r.ttl as i64)
                .bind(r.line.clone().unwrap_or_default())
                .bind(r.proxied.map(|p| p as i64))
                .bind(if r.enabled { "enabled" } else { "disabled" })
                .bind(r.remark.clone().unwrap_or_default())
                .bind(&now)
                .bind(r.weight)
                .bind(r.priority)
                .execute(pool)
                .await
                .map_err(|e| ProviderError::Api {
                    code: "LocalDb".into(),
                    message: format!("写入记录失败: {e}"),
                })?;
        }
        let local_ids: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, provider_record_id FROM records WHERE domain_id = ?")
                .bind(domain_db_id)
                .fetch_all(pool)
                .await
                .map_err(|e| ProviderError::Api {
                    code: "LocalDb".into(),
                    message: format!("读取记录失败: {e}"),
                })?;
        for (rid, pid) in &local_ids {
            if !platform_ids.contains(pid) {
                sqlx::query("DELETE FROM records WHERE id = ?")
                    .bind(rid)
                    .execute(pool)
                    .await
                    .map_err(|e| ProviderError::Api {
                        code: "LocalDb".into(),
                        message: format!("删除孤儿记录失败: {e}"),
                    })?;
            }
        }
        total_records += records.len();
    }
    Ok((domains.len(), total_records, skipped))
}

/// 启动单账户异步同步：创建 running 状态的 sync_run 并后台执行。
/// 返回 run_id 供前端轮询；同一账户并发同步会被拒绝。
pub async fn start_sync_account(state: &SharedState, account: &AccountRow) -> Result<i64, String> {
    let mut running = state.sync_running.lock().await;
    // 覆盖语义：同账户再次触发同步时，中止进行中的任务并标记其 sync_runs 为失败
    if let Some((old_handle, old_run_id)) = running.remove(&account.id) {
        old_handle.abort();
        let _ = sqlx::query(
            "UPDATE sync_runs SET finished_at = ?, status = 'failed', error = '被新同步覆盖' \
             WHERE id = ? AND status = 'running'",
        )
        .bind(now_utc())
        .bind(old_run_id)
        .execute(&state.pool)
        .await;
        tracing::info!(
            account_id = account.id,
            run_id = old_run_id,
            "进行中的同步被新同步覆盖中止"
        );
    }
    let run_id: i64 = sqlx::query(
        "INSERT INTO sync_runs (account_id, scope, started_at, status) VALUES (?, 'account', ?, 'running')",
    )
    .bind(account.id)
    .bind(now_utc())
    .execute(&state.pool)
    .await
    .map(|r| r.last_insert_rowid())
    .unwrap_or(-1);

    let st = state.clone();
    let acct = account.clone();
    let handle = tokio::spawn(async move {
        let outcome = run_sync_account(&st, &acct, run_id).await;
        tracing::info!(
            account_id = acct.id,
            run_id,
            success = outcome.success,
            "后台同步任务结束"
        );
        let mut running = st.sync_running.lock().await;
        // 仅当登记的仍是本任务时才移除（可能已被新同步覆盖替换）
        if running.get(&acct.id).map(|(_, r)| *r) == Some(run_id) {
            running.remove(&acct.id);
        }
    });
    running.insert(account.id, (handle, run_id));
    Ok(run_id)
}

/// 执行单账户同步（run 已由调用方创建）：完成后更新 sync_run 终态。
async fn run_sync_account(state: &SharedState, account: &AccountRow, run_id: i64) -> SyncOutcome {
    tracing::info!(
        account_id = account.id,
        run_id,
        "开始同步账户 {}",
        account.name
    );
    let result = match build_provider_for_account(state, account) {
        Err(e) => Err(e.to_string()),
        Ok(provider) => match with_rate_limit_retry(|| provider.list_domains()).await {
            Err(e) => Err(format!("拉取域名列表失败: {}", retry_hint(&e))),
            Ok(domains) => {
                match reconcile(&state.pool, account.id, provider.as_ref(), domains).await {
                    Err(e) => Err(retry_hint(&e)),
                    Ok((domains, records, skipped)) => Ok((domains, records, skipped)),
                }
            }
        },
    };

    let (success, message, domains, records, extra_error) = match result {
        Ok((d, r, skipped)) if skipped.is_empty() => (true, "同步成功".to_string(), d, r, None),
        Ok((d, _r, skipped)) if skipped.len() >= d => (
            false,
            format!(
                "全部 {d} 个站点的记录拉取均被平台拒绝，例如: {}",
                skipped[0].1
            ),
            0,
            0,
            None,
        ),
        Ok((d, r, skipped)) => {
            let detail = skipped
                .iter()
                .map(|(n, e)| format!("{n}（{e}）"))
                .collect::<Vec<_>>()
                .join("；");
            (
                true,
                format!("同步成功（{} 个站点被平台拒绝，已跳过）", skipped.len()),
                d,
                r,
                Some(format!("跳过站点: {detail}")),
            )
        }
        Err(m) => (false, m, 0, 0, None),
    };
    let stats = serde_json::json!({"domains": domains, "records": records}).to_string();
    if run_id > 0 {
        let _ = sqlx::query(
            "UPDATE sync_runs SET finished_at = ?, status = ?, error = ?, stats_json = ? WHERE id = ?",
        )
        .bind(now_utc())
        .bind(if success { "success" } else { "failed" })
        .bind(extra_error.clone().or_else(|| if success { None } else { Some(message.clone()) }))
        .bind(&stats)
        .bind(run_id)
        .execute(&state.pool)
        .await;
    }
    tracing::info!(
        account_id = account.id,
        success,
        domains,
        records,
        "账户同步完成"
    );
    SyncOutcome {
        account_id: account.id,
        account_name: account.name.clone(),
        success,
        message: message.clone(),
        domains,
        records,
    }
}

/// 启动的同步任务信息（供 API 返回与前端轮询）。
#[derive(Debug, serde::Serialize)]
pub struct StartedSync {
    pub account_id: i64,
    pub account_name: String,
    pub run_id: i64,
}

/// 启动全量异步同步：对所有已启用账户逐个 start（被占用/停用的跳过）。
pub async fn start_sync_all(state: &SharedState) -> Result<Vec<StartedSync>, Vec<StartedSync>> {
    let accounts: Vec<AccountRow> =
        match sqlx::query_as("SELECT * FROM accounts WHERE status = 'enabled' ORDER BY id")
            .fetch_all(&state.pool)
            .await
        {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(error = %e, "读取账户列表失败");
                return Err(Vec::new());
            }
        };
    let mut started = Vec::new();
    for account in accounts {
        match start_sync_account(state, &account).await {
            Ok(run_id) => started.push(StartedSync {
                account_id: account.id,
                account_name: account.name.clone(),
                run_id,
            }),
            Err(reason) => {
                tracing::warn!(account_id = account.id, %reason, "账户同步启动被跳过");
            }
        }
    }
    if started.is_empty() {
        Err(started)
    } else {
        Ok(started)
    }
}

/// 全量同步（阻塞执行）：所有已启用账户；账户间并发（上限 3），单账户失败不影响其余。
/// 仅供测试与内部使用；API 路径使用 start_sync_all 异步启动。
pub async fn sync_all(state: &SharedState) -> Vec<SyncOutcome> {
    let accounts: Vec<AccountRow> =
        match sqlx::query_as("SELECT * FROM accounts WHERE status = 'enabled' ORDER BY id")
            .fetch_all(&state.pool)
            .await
        {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(error = %e, "读取账户列表失败");
                return Vec::new();
            }
        };
    let semaphore = Arc::new(Semaphore::new(3));
    let mut handles = Vec::new();
    for account in accounts {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("信号量未关闭");
        let state = state.clone();
        handles.push(tokio::spawn(async move {
            // 阻塞版内部自行创建 run 记录
            let run_id: i64 = sqlx::query(
                "INSERT INTO sync_runs (account_id, scope, started_at, status) VALUES (?, 'account', ?, 'running')",
            )
            .bind(account.id)
            .bind(now_utc())
            .execute(&state.pool)
            .await
            .map(|r| r.last_insert_rowid())
            .unwrap_or(-1);
            let outcome = run_sync_account(&state, &account, run_id).await;
            drop(permit);
            outcome
        }));
    }
    let mut outcomes = Vec::new();
    for h in handles {
        match h.await {
            Ok(o) => outcomes.push(o),
            Err(e) => tracing::error!(error = %e, "同步任务异常退出"),
        }
    }
    outcomes
}

// ---------- 定时调度（6.3） ----------

pub const SYNC_INTERVAL_KEY: &str = "sync_interval_minutes";

pub async fn read_interval_minutes(state: &SharedState) -> u64 {
    let v = crate::db::get_setting(&state.pool, SYNC_INTERVAL_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // 0 = 停用自动同步；启用时最低 30 分钟，1-29 视为停用（与 API 校验一致）
    if (1..30).contains(&v) { 0 } else { v }
}

/// 调度判定：到达间隔（自上次全量同步）应执行。
pub fn should_run(last_run: Option<Instant>, now: Instant, interval_secs: u64) -> bool {
    match last_run {
        None => true,
        Some(last) => now.duration_since(last) >= Duration::from_secs(interval_secs),
    }
}

/// 定时全量同步调度器：每分钟检查一次，间隔设置实时生效。
pub fn spawn_scheduler(state: SharedState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // 首个 tick 立即返回，跳过
        let mut last_run: Option<Instant> = None;
        loop {
            ticker.tick().await;
            let interval_secs = read_interval_minutes(&state).await * 60;
            // 间隔为 0 表示停用自动同步：跳过本轮，不判定也不更新 last_run
            if interval_secs == 0 {
                continue;
            }
            if !should_run(last_run, Instant::now(), interval_secs) {
                continue;
            }
            tracing::info!("定时全量同步开始（间隔 {} 分钟）", interval_secs / 60);
            let outcomes = sync_all(&state).await;
            let failed = outcomes.iter().filter(|o| !o.success).count();
            tracing::info!(total = outcomes.len(), failed, "定时全量同步结束");
            last_run = Some(Instant::now());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_should_run_logic() {
        let now = Instant::now();
        // 从未同步过：应执行
        assert!(should_run(None, now, 3600));
        // 未到间隔：不执行
        assert!(!should_run(Some(now), now, 3600));
        assert!(!should_run(
            Some(now - Duration::from_secs(3599)),
            now,
            3600
        ));
        // 到达/超过间隔：执行
        assert!(should_run(Some(now - Duration::from_secs(3600)), now, 3600));
        assert!(should_run(Some(now - Duration::from_secs(7200)), now, 3600));
        // 调整间隔生效（改小后立刻满足）
        assert!(should_run(Some(now - Duration::from_secs(120)), now, 60));
    }

    #[tokio::test]
    async fn rate_limit_retry_once() {
        let attempts = std::cell::Cell::new(0u32);
        let r = with_rate_limit_retry(|| async {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                Err(ProviderError::RateLimited {
                    retry_after: Some(0),
                    message: "限流".into(),
                })
            } else {
                Ok(42)
            }
        })
        .await;
        assert_eq!(r.unwrap(), 42);
        assert_eq!(attempts.get(), 2, "限流应且仅应重试一次");
    }

    async fn insert_test_account(pool: &SqlitePool, provider_type: &str) -> i64 {
        sqlx::query(
            "INSERT INTO accounts (name, provider_type, credentials_enc, credentials_mask, status, created_at, updated_at) \
             VALUES ('测试账户', ?, X'', '', 'enabled', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .bind(provider_type)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
    }

    async fn remark_of(pool: &SqlitePool, provider_record_id: &str) -> String {
        sqlx::query_scalar("SELECT remark FROM records WHERE provider_record_id = ?")
            .bind(provider_record_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// 支持备注的平台以上游为准（含清空）；不支持备注的平台保留本地备注。
    #[tokio::test]
    async fn sync_remark_follows_provider_capability() {
        use crate::providers::mock::MockState;

        // 支持备注的平台
        let (state, mock) = crate::web::test_app_with_mock().await;
        {
            let mut guard = mock.state.lock().await;
            *guard = MockState::default().with_zone("example.com");
        }
        let account_id = insert_test_account(&state.pool, "volcengine").await;
        let domains = mock.list_domains().await.unwrap();
        reconcile(&state.pool, account_id, &mock, domains)
            .await
            .unwrap();

        sqlx::query(
            "UPDATE records SET remark = '本地保护' WHERE provider_record_id = 'rec-example.com-1'",
        )
        .execute(&state.pool)
        .await
        .unwrap();
        {
            let mut guard = mock.state.lock().await;
            let records = guard.records.get_mut("example.com").unwrap();
            // rec-1 平台侧清空（None），rec-2 平台侧改为「平台值」
            records[1].remark = Some("平台值".into());
        }
        let domains = mock.list_domains().await.unwrap();
        reconcile(&state.pool, account_id, &mock, domains)
            .await
            .unwrap();
        assert_eq!(
            remark_of(&state.pool, "rec-example.com-1").await,
            "",
            "支持备注的平台应以上游为准（含清空）"
        );
        assert_eq!(remark_of(&state.pool, "rec-example.com-2").await, "平台值");

        // 不支持备注的平台（EdgeOne 场景）
        let (state, mock) = crate::web::test_app_with_mock().await;
        mock.disable_record_remark();
        {
            let mut guard = mock.state.lock().await;
            *guard = MockState::default().with_zone("example.com");
        }
        let account_id = insert_test_account(&state.pool, "edgeone_intl").await;
        let domains = mock.list_domains().await.unwrap();
        reconcile(&state.pool, account_id, &mock, domains)
            .await
            .unwrap();

        sqlx::query(
            "UPDATE records SET remark = '本地保护' WHERE provider_record_id = 'rec-example.com-1'",
        )
        .execute(&state.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE records SET remark = '旧本地' WHERE provider_record_id = 'rec-example.com-2'",
        )
        .execute(&state.pool)
        .await
        .unwrap();
        {
            let mut guard = mock.state.lock().await;
            let records = guard.records.get_mut("example.com").unwrap();
            records[0].remark = None;
            records[1].remark = Some("平台值".into());
        }
        let domains = mock.list_domains().await.unwrap();
        reconcile(&state.pool, account_id, &mock, domains)
            .await
            .unwrap();
        assert_eq!(
            remark_of(&state.pool, "rec-example.com-1").await,
            "本地保护",
            "不支持备注的平台应保留本地备注"
        );
        assert_eq!(
            remark_of(&state.pool, "rec-example.com-2").await,
            "旧本地",
            "不支持备注的平台不应被上游值覆盖"
        );
    }
}
