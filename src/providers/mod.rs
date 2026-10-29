use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod aliyun;
pub mod cloudflare;
pub mod dnspod;
pub mod edgeone;
#[cfg(test)]
pub mod mock;
pub mod volcengine;

/// 平台错误的统一归一化：上层据此映射 HTTP 状态与可读信息、决定重试策略。
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("平台认证失败: {0}")]
    Auth(String),
    #[error("平台限流: {message}")]
    RateLimited {
        retry_after: Option<u64>,
        message: String,
    },
    #[error("平台侧资源不存在: {0}")]
    NotFound(String),
    #[error("平台不支持该操作: {0}")]
    Unsupported(String),
    #[error("网络错误: {0}")]
    Network(String),
    #[error("平台 API 错误 [{code}]: {message}")]
    Api { code: String, message: String },
}

impl ProviderError {
    pub fn network(e: reqwest::Error) -> Self {
        ProviderError::Network(format!("{e}"))
    }
}

/// 支持的平台类型。腾讯云 DNSPod 与 EdgeOne 均区分国内站 / 国际站（端点与密钥体系不同）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderType {
    Aliyun,
    Dnspod,
    DnspodIntl,
    Edgeone,
    EdgeoneIntl,
    Cloudflare,
    Volcengine,
}

impl ProviderType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderType::Aliyun => "aliyun",
            ProviderType::Dnspod => "dnspod",
            ProviderType::DnspodIntl => "dnspod_intl",
            ProviderType::Edgeone => "edgeone",
            ProviderType::EdgeoneIntl => "edgeone_intl",
            ProviderType::Cloudflare => "cloudflare",
            ProviderType::Volcengine => "volcengine",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            ProviderType::Aliyun => "阿里云",
            ProviderType::Dnspod => "腾讯云 DNSPod（国内站）",
            ProviderType::DnspodIntl => "腾讯云 DNSPod（国际站）",
            ProviderType::Edgeone => "EdgeOne（国内站）",
            ProviderType::EdgeoneIntl => "EdgeOne（国际站）",
            ProviderType::Cloudflare => "CloudFlare",
            ProviderType::Volcengine => "火山引擎",
        }
    }

    pub fn required_credential_fields(&self) -> &'static [&'static str] {
        match self {
            ProviderType::Aliyun => &["access_key_id", "access_key_secret"],
            ProviderType::Dnspod | ProviderType::DnspodIntl => &["secret_id", "secret_key"],
            ProviderType::Edgeone | ProviderType::EdgeoneIntl => &["secret_id", "secret_key"],
            ProviderType::Cloudflare => &["api_token"],
            ProviderType::Volcengine => &["access_key_id", "access_key_secret"],
        }
    }
}

impl std::fmt::Display for ProviderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 平台侧域名。
#[derive(Debug, Clone)]
pub struct ProviderDomain {
    pub name: String,
    /// 平台侧域名标识：CloudFlare 为 zone id；阿里云 / DNSPod 为空（以域名本身定位）。
    pub provider_domain_id: String,
    pub status: String,
    /// 平台侧域名备注：仅 DNSPod 支持；不支持的平台为 None。
    pub remark: Option<String>,
}

/// 定位一个域名（zone）所需的信息。
#[derive(Debug, Clone)]
pub struct ProviderZone {
    pub name: String,
    pub provider_domain_id: String,
}

/// 平台侧解析记录（归一化后）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRecord {
    pub provider_record_id: String,
    /// 主机记录（相对域名的部分；根记录为 "@"）。
    pub host: String,
    pub rtype: String,
    pub value: String,
    pub ttl: u64,
    /// 解析线路（阿里云 / DNSPod；CloudFlare 为空）。
    pub line: Option<String>,
    /// CloudFlare 代理状态；其他平台为 None。
    pub proxied: Option<bool>,
    pub enabled: bool,
    /// 备注（阿里云 / DNSPod / CloudFlare 支持；EdgeOne 无）。
    pub remark: Option<String>,
    /// 权重（EdgeOne 适用，-1~100，-1 表示未设置）；其他平台为 None。
    pub weight: Option<i64>,
    /// 优先级（EdgeOne MX 记录适用，0~50，0 表示未设置）；其他平台为 None。
    pub priority: Option<i64>,
}

/// 创建/修改记录的入参。
#[derive(Debug, Clone, Deserialize)]
pub struct RecordInput {
    pub host: String,
    pub rtype: String,
    pub value: String,
    #[serde(default = "default_ttl")]
    pub ttl: u64,
    pub line: Option<String>,
    pub proxied: Option<bool>,
    pub remark: Option<String>,
    pub weight: Option<i64>,
    pub priority: Option<i64>,
}

fn default_ttl() -> u64 {
    600
}

/// 记录名（FQDN）→ 相对站点的 host（根记录为 "@"）。CloudFlare / EdgeOne 记录名为 FQDN。
pub(crate) fn fqdn_to_host(name: &str, zone: &str) -> String {
    if name.eq_ignore_ascii_case(zone) {
        return "@".into();
    }
    let suffix = format!(".{}", zone.to_lowercase());
    match name.to_lowercase().strip_suffix(&suffix) {
        Some(host) => host.to_string(),
        None => name.to_string(),
    }
}

/// 相对 host → FQDN 记录名（根记录 "@" → zone 本身）。
pub(crate) fn host_to_fqdn(host: &str, zone: &str) -> String {
    if host == "@" {
        zone.to_string()
    } else {
        format!("{host}.{zone}")
    }
}

/// DNS 平台适配器：上层业务面向该抽象编程，新增平台只需实现本 trait。
/// 列表方法在适配器内部消化分页，一次返回全量。
#[async_trait]
pub trait DnsProvider: Send + Sync {
    /// 最小只读调用，用于凭据连通性验证。
    async fn verify(&self) -> Result<(), ProviderError>;
    async fn list_domains(&self) -> Result<Vec<ProviderDomain>, ProviderError>;
    async fn list_records(&self, zone: &ProviderZone)
    -> Result<Vec<ProviderRecord>, ProviderError>;
    async fn create_record(
        &self,
        zone: &ProviderZone,
        input: &RecordInput,
    ) -> Result<ProviderRecord, ProviderError>;
    async fn update_record(
        &self,
        zone: &ProviderZone,
        record: &ProviderRecord,
    ) -> Result<ProviderRecord, ProviderError>;
    async fn delete_record(
        &self,
        zone: &ProviderZone,
        provider_record_id: &str,
    ) -> Result<(), ProviderError>;
    async fn set_record_status(
        &self,
        zone: &ProviderZone,
        provider_record_id: &str,
        enabled: bool,
    ) -> Result<(), ProviderError>;

    /// 单独更新记录备注（空字符串表示清除）。
    /// 平台不支持备注时返回 Unsupported；支持备注的平台在 create/update 中直接携带。
    async fn set_record_remark(
        &self,
        zone: &ProviderZone,
        provider_record_id: &str,
        remark: &str,
    ) -> Result<(), ProviderError> {
        let _ = (self, zone, provider_record_id, remark);
        Err(ProviderError::Unsupported("平台不支持解析记录备注".into()))
    }

    /// 更新域名备注（空字符串表示清除上游备注）。
    /// DNSPod（ModifyDomainRemark）与阿里云（UpdateDomainRemark）支持；
    /// CloudFlare / EdgeOne 域名无备注概念，返回 Unsupported。
    async fn set_domain_remark(
        &self,
        domain_name: &str,
        remark: &str,
    ) -> Result<(), ProviderError> {
        let _ = (self, domain_name, remark);
        Err(ProviderError::Unsupported("平台不支持域名备注".into()))
    }
}

/// Provider 工厂：按平台类型与（解密后的）凭据 JSON 构造适配器。
/// 生产环境使用 `default_provider_factory`；测试注入 MockProvider。
pub type ProviderFactory = std::sync::Arc<
    dyn Fn(ProviderType, &str) -> Result<Box<dyn DnsProvider>, ProviderError> + Send + Sync,
>;

pub fn default_provider_factory() -> ProviderFactory {
    std::sync::Arc::new(|ptype, cred| build_provider(ptype, cred, None))
}

/// 适配器工厂：按平台类型与凭据 JSON 构造 provider。
/// `endpoint_override` 用于测试注入本地 mock 服务。
pub fn build_provider(
    ptype: ProviderType,
    credentials_json: &str,
    endpoint_override: Option<&str>,
) -> Result<Box<dyn DnsProvider>, ProviderError> {
    fn bad(ptype: &str, e: serde_json::Error) -> ProviderError {
        ProviderError::Api {
            code: "InvalidCredentials".into(),
            message: format!("{ptype} 凭据字段不完整: {e}"),
        }
    }
    match ptype {
        ProviderType::Aliyun => {
            let cred: aliyun::AliyunCredentials =
                serde_json::from_str(credentials_json).map_err(|e| bad("阿里云", e))?;
            let mut p = aliyun::AliyunProvider::new(cred);
            if let Some(ep) = endpoint_override {
                p = p.with_endpoint(ep);
            }
            Ok(Box::new(p))
        }
        ProviderType::Dnspod | ProviderType::DnspodIntl => {
            let cred: dnspod::DnspodCredentials =
                serde_json::from_str(credentials_json).map_err(|e| bad("腾讯云", e))?;
            let site = if ptype == ProviderType::Dnspod {
                dnspod::DnspodSite::China
            } else {
                dnspod::DnspodSite::International
            };
            let mut p = dnspod::DnspodProvider::new(cred, site);
            if let Some(ep) = endpoint_override {
                p = p.with_endpoint(ep);
            }
            Ok(Box::new(p))
        }
        ProviderType::Edgeone | ProviderType::EdgeoneIntl => {
            let cred: edgeone::EdgeoneCredentials =
                serde_json::from_str(credentials_json).map_err(|e| bad("EdgeOne", e))?;
            let site = if ptype == ProviderType::Edgeone {
                edgeone::TeoSite::China
            } else {
                edgeone::TeoSite::International
            };
            let mut p = edgeone::EdgeoneProvider::new(cred, site);
            if let Some(ep) = endpoint_override {
                p = p.with_endpoint(ep);
            }
            Ok(Box::new(p))
        }
        ProviderType::Cloudflare => {
            let cred: cloudflare::CloudflareCredentials =
                serde_json::from_str(credentials_json).map_err(|e| bad("CloudFlare", e))?;
            let mut p = cloudflare::CloudflareProvider::new(cred);
            if let Some(base) = endpoint_override {
                p = p.with_base_url(base);
            }
            Ok(Box::new(p))
        }
        ProviderType::Volcengine => {
            let cred: volcengine::VolcengineCredentials =
                serde_json::from_str(credentials_json).map_err(|e| bad("火山引擎", e))?;
            let mut p = volcengine::VolcengineProvider::new(cred);
            if let Some(ep) = endpoint_override {
                p = p.with_endpoint(ep);
            }
            Ok(Box::new(p))
        }
    }
}

/// 真实凭据集成测试（默认忽略）：
/// 提供对应环境变量后执行 `cargo test -- --ignored`。
/// - 阿里云:   UNIDNS_TEST_ALIYUN_AK_ID / UNIDNS_TEST_ALIYUN_AK_SECRET
/// - 腾讯云:   UNIDNS_TEST_DNSPOD_SECRET_ID / UNIDNS_TEST_DNSPOD_SECRET_KEY
/// - CloudFlare: UNIDNS_TEST_CF_TOKEN
/// - 火山引擎: UNIDNS_TEST_VOLC_AK_ID / UNIDNS_TEST_VOLC_AK_SECRET
#[cfg(test)]
mod real_credential_tests {
    use super::*;

    fn opt(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.trim().is_empty())
    }

    #[tokio::test]
    #[ignore = "需要真实凭据（UNIDNS_TEST_ALIYUN_AK_ID / UNIDNS_TEST_ALIYUN_AK_SECRET）"]
    async fn aliyun_real_verify() {
        let (Some(id), Some(secret)) = (
            opt("UNIDNS_TEST_ALIYUN_AK_ID"),
            opt("UNIDNS_TEST_ALIYUN_AK_SECRET"),
        ) else {
            eprintln!("未提供阿里云测试凭据，跳过");
            return;
        };
        let cred = serde_json::json!({"access_key_id": id, "access_key_secret": secret});
        let provider = build_provider(ProviderType::Aliyun, &cred.to_string(), None).unwrap();
        provider.verify().await.expect("阿里云连通性验证失败");
    }

    #[tokio::test]
    #[ignore = "需要真实凭据（UNIDNS_TEST_DNSPOD_SECRET_ID / UNIDNS_TEST_DNSPOD_SECRET_KEY）"]
    async fn dnspod_real_verify() {
        let (Some(id), Some(key)) = (
            opt("UNIDNS_TEST_DNSPOD_SECRET_ID"),
            opt("UNIDNS_TEST_DNSPOD_SECRET_KEY"),
        ) else {
            eprintln!("未提供腾讯云测试凭据，跳过");
            return;
        };
        // UNIDNS_TEST_DNSPOD_SITE=intl 时使用国际站端点（密钥体系相互独立）
        let site = match opt("UNIDNS_TEST_DNSPOD_SITE").as_deref() {
            Some(s) if s.eq_ignore_ascii_case("intl") => ProviderType::DnspodIntl,
            _ => ProviderType::Dnspod,
        };
        let cred = serde_json::json!({"secret_id": id, "secret_key": key});
        let provider = build_provider(site, &cred.to_string(), None).unwrap();
        provider.verify().await.expect("腾讯云连通性验证失败");
    }

    #[tokio::test]
    #[ignore = "需要真实凭据（UNIDNS_TEST_CF_TOKEN）"]
    async fn cloudflare_real_verify() {
        let Some(token) = opt("UNIDNS_TEST_CF_TOKEN") else {
            eprintln!("未提供 CloudFlare 测试凭据，跳过");
            return;
        };
        let cred = serde_json::json!({"api_token": token});
        let provider = build_provider(ProviderType::Cloudflare, &cred.to_string(), None).unwrap();
        provider.verify().await.expect("CloudFlare 连通性验证失败");
    }

    #[tokio::test]
    #[ignore = "需要真实凭据（UNIDNS_TEST_VOLC_AK_ID / UNIDNS_TEST_VOLC_AK_SECRET）"]
    async fn volcengine_real_verify() {
        let (Some(id), Some(secret)) = (
            opt("UNIDNS_TEST_VOLC_AK_ID"),
            opt("UNIDNS_TEST_VOLC_AK_SECRET"),
        ) else {
            eprintln!("未提供火山引擎测试凭据，跳过");
            return;
        };
        let cred = serde_json::json!({"access_key_id": id, "access_key_secret": secret});
        let provider = build_provider(ProviderType::Volcengine, &cred.to_string(), None).unwrap();
        provider.verify().await.expect("火山引擎连通性验证失败");
        let domains = provider.list_domains().await.expect("火山引擎拉取域名失败");
        eprintln!("火山引擎账户下共 {} 个域名", domains.len());
        if let Some(d) = domains.first() {
            let zone = ProviderZone {
                name: d.name.clone(),
                provider_domain_id: d.provider_domain_id.clone(),
            };
            let records = provider
                .list_records(&zone)
                .await
                .expect("火山引擎拉取解析记录失败");
            eprintln!("域名 {} 共 {} 条解析记录", d.name, records.len());
        }
    }

    /// 任务 9.2 端到端冒烟：对每个已提供凭据的平台执行
    /// 验证 → 拉取域名 → 创建 TXT 测试记录 → 修改 → 停用/启用 → 删除。
    /// 测试记录主机名为 _unidns-smoke-<时间戳>，删除后平台侧不残留。
    #[tokio::test]
    #[ignore = "真实凭据端到端冒烟（会对平台执行创建/修改/删除操作）"]
    async fn real_e2e_smoke() {
        let cases: Vec<(&str, ProviderType, serde_json::Value)> = {
            let mut v = Vec::new();
            if let (Some(id), Some(secret)) = (
                opt("UNIDNS_TEST_ALIYUN_AK_ID"),
                opt("UNIDNS_TEST_ALIYUN_AK_SECRET"),
            ) {
                v.push((
                    "阿里云",
                    ProviderType::Aliyun,
                    serde_json::json!({"access_key_id": id, "access_key_secret": secret}),
                ));
            }
            if let (Some(id), Some(key)) = (
                opt("UNIDNS_TEST_DNSPOD_SECRET_ID"),
                opt("UNIDNS_TEST_DNSPOD_SECRET_KEY"),
            ) {
                let site = match opt("UNIDNS_TEST_DNSPOD_SITE").as_deref() {
                    Some(s) if s.eq_ignore_ascii_case("intl") => ProviderType::DnspodIntl,
                    _ => ProviderType::Dnspod,
                };
                v.push((
                    "腾讯云 DNSPod",
                    site,
                    serde_json::json!({"secret_id": id, "secret_key": key}),
                ));
            }
            if let Some(token) = opt("UNIDNS_TEST_CF_TOKEN") {
                v.push((
                    "CloudFlare",
                    ProviderType::Cloudflare,
                    serde_json::json!({"api_token": token}),
                ));
            }
            if let (Some(id), Some(secret)) = (
                opt("UNIDNS_TEST_VOLC_AK_ID"),
                opt("UNIDNS_TEST_VOLC_AK_SECRET"),
            ) {
                v.push((
                    "火山引擎",
                    ProviderType::Volcengine,
                    serde_json::json!({"access_key_id": id, "access_key_secret": secret}),
                ));
            }
            v
        };
        assert!(!cases.is_empty(), "未提供任何平台测试凭据");
        let ts = chrono::Utc::now().timestamp();
        for (name, ptype, cred) in cases {
            eprintln!("[{name}] 开始端到端冒烟…");
            let provider = build_provider(ptype, &cred.to_string(), None).unwrap();
            provider
                .verify()
                .await
                .unwrap_or_else(|e| panic!("[{name}] 验证失败: {e}"));
            let domains = provider
                .list_domains()
                .await
                .unwrap_or_else(|e| panic!("[{name}] 拉取域名失败: {e}"));
            let Some(domain) = domains.first() else {
                eprintln!("[{name}] 账户下无域名，跳过记录操作");
                continue;
            };
            let zone = ProviderZone {
                name: domain.name.clone(),
                provider_domain_id: domain.provider_domain_id.clone(),
            };
            eprintln!(
                "[{name}] 使用域名 {}（共 {} 个）",
                domain.name,
                domains.len()
            );

            let host = format!("_unidns-smoke-{ts}");
            let input = RecordInput {
                host: host.clone(),
                rtype: "TXT".into(),
                value: "unidns-smoke".into(),
                ttl: 600,
                line: None,
                proxied: None,
                remark: None,
                weight: None,
                priority: None,
            };
            let created = provider
                .create_record(&zone, &input)
                .await
                .unwrap_or_else(|e| panic!("[{name}] 创建测试记录失败: {e}"));
            eprintln!("[{name}] 创建成功 id={}", created.provider_record_id);

            let mut modified = created.clone();
            modified.value = "unidns-smoke-updated".into();
            provider
                .update_record(&zone, &modified)
                .await
                .unwrap_or_else(|e| panic!("[{name}] 修改测试记录失败: {e}"));
            eprintln!("[{name}] 修改成功");

            // 记录备注回写（支持的平台执行，不支持则跳过）
            match provider
                .set_record_remark(&zone, &created.provider_record_id, "unidns-smoke")
                .await
            {
                Ok(_) => eprintln!("[{name}] 记录备注设置成功"),
                Err(ProviderError::Unsupported(m)) => {
                    eprintln!("[{name}] 平台不支持记录备注，跳过（{m}）")
                }
                Err(e) => panic!("[{name}] 设置记录备注失败: {e}"),
            }
            match provider
                .set_domain_remark(&domain.name, "unidns-smoke")
                .await
            {
                Ok(_) => eprintln!("[{name}] 域名备注设置成功"),
                Err(ProviderError::Unsupported(m)) => {
                    eprintln!("[{name}] 平台不支持域名备注，跳过（{m}）")
                }
                Err(e) => panic!("[{name}] 设置域名备注失败: {e}"),
            }

            match provider
                .set_record_status(&zone, &created.provider_record_id, false)
                .await
            {
                Ok(_) => {
                    provider
                        .set_record_status(&zone, &created.provider_record_id, true)
                        .await
                        .unwrap_or_else(|e| panic!("[{name}] 恢复启用失败: {e}"));
                    eprintln!("[{name}] 停用/启用成功");
                }
                Err(ProviderError::Unsupported(m)) => {
                    eprintln!("[{name}] 平台不支持启停，跳过（{m}）")
                }
                Err(e) => panic!("[{name}] 停用失败: {e}"),
            }

            provider
                .delete_record(&zone, &created.provider_record_id)
                .await
                .unwrap_or_else(|e| panic!("[{name}] 删除测试记录失败: {e}"));
            eprintln!("[{name}] 删除成功，冒烟通过 ✓");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_type_serde_roundtrip() {
        for (ty, s) in [
            (ProviderType::Aliyun, "aliyun"),
            (ProviderType::Dnspod, "dnspod"),
            (ProviderType::DnspodIntl, "dnspod_intl"),
            (ProviderType::Edgeone, "edgeone"),
            (ProviderType::EdgeoneIntl, "edgeone_intl"),
            (ProviderType::Cloudflare, "cloudflare"),
        ] {
            assert_eq!(serde_json::to_string(&ty).unwrap(), format!("\"{s}\""));
            let back: ProviderType = serde_json::from_str(&format!("\"{s}\"")).unwrap();
            assert_eq!(back, ty);
        }
        assert!(serde_json::from_str::<ProviderType>("\"others\"").is_err());
    }

    #[test]
    fn record_input_defaults_ttl() {
        let input: RecordInput =
            serde_json::from_str(r#"{"host":"www","rtype":"A","value":"1.2.3.4"}"#).unwrap();
        assert_eq!(input.ttl, 600);
    }
}
