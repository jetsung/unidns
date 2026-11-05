//! 测试专用可编程 MockProvider：供账户验证、同步、记录操作的集成测试注入。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::{
    DnsProvider, ProviderDomain, ProviderError, ProviderRecord, ProviderZone, RecordInput,
};

#[derive(Debug, Default)]
pub struct MockState {
    pub domains: Vec<ProviderDomain>,
    /// zone 名称 -> 记录列表
    pub records: HashMap<String, Vec<ProviderRecord>>,
    pub next_id: u64,
    pub fail_verify: Option<ProviderError>,
    pub fail_list_domains: Option<ProviderError>,
    pub fail_list_records: Option<ProviderError>,
    pub fail_create: Option<ProviderError>,
    pub fail_update: Option<ProviderError>,
    pub fail_delete: Option<ProviderError>,
    pub fail_set_status: Option<ProviderError>,
    pub update_not_found: bool,
    pub delete_not_found: bool,
    pub set_status_not_found: bool,
    pub unsupported_set_status: bool,
    /// list_domains 前的延迟（毫秒）：供同步覆盖中止测试制造长时间 running 状态。
    pub delay_list_domains_ms: u64,
    pub calls: Vec<String>,
    /// 域名备注回写记录：domain_name -> remark，供测试断言上游回写是否发生。
    pub domain_remarks: HashMap<String, String>,
}

impl MockState {
    /// 预置一个 zone 与两条记录。
    pub fn with_zone(mut self, zone: &str) -> Self {
        self.domains.push(ProviderDomain {
            name: zone.into(),
            provider_domain_id: format!("zone-{zone}"),
            status: "active".into(),
            remark: None,
        });
        let records = vec![
            ProviderRecord {
                provider_record_id: format!("rec-{zone}-1"),
                host: "www".into(),
                rtype: "A".into(),
                value: "1.2.3.4".into(),
                ttl: 600,
                line: Some("default".into()),
                proxied: None,
                enabled: true,
                remark: None,
                weight: None,
                priority: None,
            },
            ProviderRecord {
                provider_record_id: format!("rec-{zone}-2"),
                host: "@".into(),
                rtype: "AAAA".into(),
                value: "::1".into(),
                ttl: 300,
                line: Some("default".into()),
                proxied: None,
                enabled: true,
                remark: Some("演示备注".into()),
                weight: None,
                priority: None,
            },
        ];
        self.records.insert(zone.into(), records);
        self
    }
}

#[derive(Clone)]
pub struct MockProvider {
    pub state: Arc<Mutex<MockState>>,
    record_remark_supported: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new(MockState::default())
    }
}

impl MockProvider {
    pub fn new(state: MockState) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            record_remark_supported: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    /// 模拟上游无记录备注字段的平台（备注仅本地保存）。
    pub fn disable_record_remark(&self) {
        self.record_remark_supported
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    async fn track(&self, call: &str) {
        self.state.lock().await.calls.push(call.into());
    }

    /// 上一次对账时的错误注入（把错误取出后清除，便于断言次数）。
    pub async fn take_calls(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().await.calls)
    }
}

#[async_trait]
impl DnsProvider for MockProvider {
    fn supports_record_remark(&self) -> bool {
        self.record_remark_supported
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn verify(&self) -> Result<(), ProviderError> {
        self.track("verify").await;
        match self.state.lock().await.fail_verify.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn list_domains(&self) -> Result<Vec<ProviderDomain>, ProviderError> {
        self.track("list_domains").await;
        if self.state.lock().await.delay_list_domains_ms > 0 {
            let delay = self.state.lock().await.delay_list_domains_ms;
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        let mut guard = self.state.lock().await;
        if let Some(e) = guard.fail_list_domains.take() {
            return Err(e);
        }
        Ok(guard.domains.clone())
    }

    async fn list_records(
        &self,
        zone: &ProviderZone,
    ) -> Result<Vec<ProviderRecord>, ProviderError> {
        self.track("list_records").await;
        let mut guard = self.state.lock().await;
        if let Some(e) = guard.fail_list_records.take() {
            return Err(e);
        }
        Ok(guard.records.get(&zone.name).cloned().unwrap_or_default())
    }

    async fn create_record(
        &self,
        zone: &ProviderZone,
        input: &RecordInput,
    ) -> Result<ProviderRecord, ProviderError> {
        self.track("create_record").await;
        let mut guard = self.state.lock().await;
        if let Some(e) = guard.fail_create.take() {
            return Err(e);
        }
        guard.next_id += 1;
        let record = ProviderRecord {
            provider_record_id: format!("mock-{}", guard.next_id),
            host: input.host.clone(),
            rtype: input.rtype.clone(),
            value: input.value.clone(),
            ttl: input.ttl,
            line: input.line.clone(),
            proxied: input.proxied,
            enabled: true,
            // 上游无备注字段时平台记录不携带备注
            remark: if self.supports_record_remark() {
                input.remark.clone()
            } else {
                None
            },
            weight: input.weight,
            priority: input.priority,
        };
        guard
            .records
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
        self.track("update_record").await;
        let supports_remark = self.supports_record_remark();
        let mut guard = self.state.lock().await;
        if let Some(e) = guard.fail_update.take() {
            return Err(e);
        }
        if guard.update_not_found {
            return Err(ProviderError::NotFound("平台侧记录不存在".into()));
        }
        let records = guard.records.entry(zone.name.clone()).or_default();
        let existing = records
            .iter_mut()
            .find(|r| r.provider_record_id == record.provider_record_id);
        match existing {
            Some(r) => {
                let mut stored = record.clone();
                if !supports_remark {
                    // 上游无备注字段：平台侧记录不保留备注
                    stored.remark = None;
                }
                *r = stored.clone();
                Ok(stored)
            }
            None => Err(ProviderError::NotFound("记录不存在".into())),
        }
    }

    async fn delete_record(
        &self,
        zone: &ProviderZone,
        provider_record_id: &str,
    ) -> Result<(), ProviderError> {
        self.track("delete_record").await;
        let mut guard = self.state.lock().await;
        if let Some(e) = guard.fail_delete.take() {
            return Err(e);
        }
        if guard.delete_not_found {
            return Err(ProviderError::NotFound("平台侧记录不存在".into()));
        }
        let records = guard.records.entry(zone.name.clone()).or_default();
        records.retain(|r| r.provider_record_id != provider_record_id);
        Ok(())
    }

    async fn set_record_status(
        &self,
        zone: &ProviderZone,
        provider_record_id: &str,
        enabled: bool,
    ) -> Result<(), ProviderError> {
        self.track("set_record_status").await;
        let mut guard = self.state.lock().await;
        if let Some(e) = guard.fail_set_status.take() {
            return Err(e);
        }
        if guard.unsupported_set_status {
            return Err(ProviderError::Unsupported("平台不支持启停".into()));
        }
        if guard.set_status_not_found {
            return Err(ProviderError::NotFound("平台侧记录不存在".into()));
        }
        let records = guard.records.entry(zone.name.clone()).or_default();
        let existing = records
            .iter_mut()
            .find(|r| r.provider_record_id == provider_record_id);
        match existing {
            Some(r) => {
                r.enabled = enabled;
                Ok(())
            }
            None => Err(ProviderError::NotFound("记录不存在".into())),
        }
    }

    /// 记录域名备注回写（空串清除），供测试断言上游同步是否发生。
    async fn set_domain_remark(
        &self,
        domain_name: &str,
        remark: &str,
    ) -> Result<(), ProviderError> {
        self.track("set_domain_remark").await;
        let mut guard = self.state.lock().await;
        guard
            .domain_remarks
            .insert(domain_name.to_string(), remark.to_string());
        Ok(())
    }
}
