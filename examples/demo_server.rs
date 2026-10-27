//! 开发调试用演示服务器：内嵌一个可编程的内存 mock DNS 平台，
//! 供无真实凭据时在浏览器中完整体验界面流程（添加账户 → 验证 → 同步 → 记录管理）。
//!
//! 运行：`cargo run --example demo_server`，然后访问 http://127.0.0.1:18099
//! 在界面中添加任意账户（凭据可随意填写，如 api_token=demo），平台调用都会命中内存 mock。

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use async_trait::async_trait;
use tokio::sync::RwLock;
use unidns::providers::{
    DnsProvider, ProviderDomain, ProviderError, ProviderRecord, ProviderZone, RecordInput,
};
use unidns::web::{AppState, SharedState};

/// 内存 mock 平台：预置两个域名，支持全部写操作。
#[derive(Clone, Default)]
struct DemoProvider {
    store: Arc<RwLock<HashMap<String, Vec<ProviderRecord>>>>,
}

impl DemoProvider {
    fn new() -> Self {
        let store: HashMap<String, Vec<ProviderRecord>> = [
            (
                "demo.example.com",
                vec![
                    ProviderRecord {
                        provider_record_id: "1001".into(),
                        host: "www".into(),
                        rtype: "A".into(),
                        value: "203.0.113.10".into(),
                        ttl: 600,
                        line: Some("default".into()),
                        proxied: None,
                        enabled: true,
                        remark: None,
                    },
                    ProviderRecord {
                        provider_record_id: "1002".into(),
                        host: "@".into(),
                        rtype: "A".into(),
                        value: "203.0.113.1".into(),
                        ttl: 600,
                        line: Some("default".into()),
                        proxied: None,
                        enabled: true,
                        remark: None,
                    },
                ],
            ),
            (
                "demo.example.org",
                vec![ProviderRecord {
                    provider_record_id: "2001".into(),
                    host: "api".into(),
                    rtype: "CNAME".into(),
                    value: "cdn.demo.example.net".into(),
                    ttl: 300,
                    line: None,
                    proxied: Some(false),
                    remark: None,
                    enabled: true,
                }],
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        Self {
            store: Arc::new(RwLock::new(store)),
        }
    }
}

#[async_trait]
impl DnsProvider for DemoProvider {
    async fn verify(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    async fn list_domains(&self) -> Result<Vec<ProviderDomain>, ProviderError> {
        let store = self.store.read().await;
        Ok(store
            .keys()
            .map(|name| ProviderDomain {
                name: name.clone(),
                provider_domain_id: format!("zone-{name}"),
                status: "active".into(),
                remark: None,
            })
            .collect())
    }

    async fn list_records(
        &self,
        zone: &ProviderZone,
    ) -> Result<Vec<ProviderRecord>, ProviderError> {
        Ok(self
            .store
            .read()
            .await
            .get(&zone.name)
            .cloned()
            .unwrap_or_default())
    }

    async fn create_record(
        &self,
        zone: &ProviderZone,
        input: &RecordInput,
    ) -> Result<ProviderRecord, ProviderError> {
        let mut store = self.store.write().await;
        let id = format!("{}", 3000 + store.values().map(Vec::len).sum::<usize>());
        let record = ProviderRecord {
            provider_record_id: id,
            host: input.host.clone(),
            rtype: input.rtype.clone(),
            value: input.value.clone(),
            ttl: input.ttl,
            line: input.line.clone(),
            proxied: input.proxied,
            enabled: true,
            remark: input.remark.clone(),
        };
        store
            .entry(zone.name.clone())
            .or_default()
            .push(record.clone());
        Ok(record)
    }

    async fn update_record(
        &self,
        zone: &ProviderZone,
        record: &ProviderRecord,
    ) -> Result<ProviderRecord, ProviderError> {
        let mut store = self.store.write().await;
        let records = store.entry(zone.name.clone()).or_default();
        let existing = records
            .iter_mut()
            .find(|r| r.provider_record_id == record.provider_record_id)
            .ok_or_else(|| ProviderError::NotFound("平台侧记录不存在".into()))?;
        *existing = record.clone();
        Ok(record.clone())
    }

    async fn delete_record(
        &self,
        zone: &ProviderZone,
        provider_record_id: &str,
    ) -> Result<(), ProviderError> {
        let mut store = self.store.write().await;
        let records = store.entry(zone.name.clone()).or_default();
        let before = records.len();
        records.retain(|r| r.provider_record_id != provider_record_id);
        if records.len() == before {
            return Err(ProviderError::NotFound("平台侧记录不存在".into()));
        }
        Ok(())
    }

    async fn set_record_status(
        &self,
        zone: &ProviderZone,
        provider_record_id: &str,
        enabled: bool,
    ) -> Result<(), ProviderError> {
        let mut store = self.store.write().await;
        let records = store.entry(zone.name.clone()).or_default();
        let existing = records
            .iter_mut()
            .find(|r| r.provider_record_id == provider_record_id)
            .ok_or_else(|| ProviderError::NotFound("平台侧记录不存在".into()))?;
        existing.enabled = enabled;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("UNIDNS_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let demo = DemoProvider::new();
    let state: SharedState = Arc::new(AppState {
        pool: unidns::db::init_pool("sqlite::memory:").await?,
        master_key: Arc::new([0x42u8; 32]),
        sync_running: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        provider_factory: {
            let demo = demo.clone();
            Arc::new(move |_ptype, _cred| {
                let demo = demo.clone();
                Ok(Box::new(demo) as Box<dyn DnsProvider>)
            })
        },
    });

    let app = unidns::web::router(state);
    let addr: SocketAddr = "127.0.0.1:18099".parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        "演示服务（内存 mock 平台，预置 demo.example.com / demo.example.org）监听 http://{addr}"
    );
    axum::serve(listener, app).await?;
    Ok(())
}
