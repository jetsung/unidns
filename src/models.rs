use crate::providers::ProviderType;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AccountRow {
    pub id: i64,
    pub name: String,
    pub provider_type: String,
    pub credentials_enc: Vec<u8>,
    pub credentials_mask: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_verified_at: Option<String>,
    pub last_verify_ok: Option<i64>,
    pub last_verify_msg: Option<String>,
}

impl AccountRow {
    /// 未知值返回 `None`（不静默映射到某个平台，避免拿错误的 provider 调 API）。
    pub fn provider_type(&self) -> Option<ProviderType> {
        match self.provider_type.as_str() {
            "aliyun" => Some(ProviderType::Aliyun),
            "dnspod" => Some(ProviderType::Dnspod),
            "dnspod_intl" => Some(ProviderType::DnspodIntl),
            "edgeone" => Some(ProviderType::Edgeone),
            "edgeone_intl" => Some(ProviderType::EdgeoneIntl),
            "cloudflare" => Some(ProviderType::Cloudflare),
            _ => None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.status == "enabled"
    }
}

#[derive(Debug, sqlx::FromRow)]
pub struct DomainRow {
    pub id: i64,
    pub account_id: i64,
    pub name: String,
    pub provider_domain_id: String,
    pub status: String,
    pub synced_at: String,
    /// 本地备注（迁移 0004，仅 unidns 界面使用，不回写平台）。
    #[sqlx(default)]
    pub remark: String,
}

#[derive(Debug, sqlx::FromRow)]
pub struct RecordRow {
    pub id: i64,
    pub domain_id: i64,
    pub account_id: i64,
    pub provider_record_id: String,
    pub host: String,
    pub rtype: String,
    pub value: String,
    pub ttl: i64,
    pub line: String,
    pub proxied: Option<i64>,
    pub status: String,
    pub pending_sync: i64,
    pub remark: String,
    pub synced_at: String,
}

impl RecordRow {
    pub fn enabled(&self) -> bool {
        self.status == "enabled"
    }

    pub fn proxied_bool(&self) -> Option<bool> {
        self.proxied.map(|v| v != 0)
    }
}

#[derive(Debug, sqlx::FromRow)]
pub struct SyncRunRow {
    pub id: i64,
    pub account_id: Option<i64>,
    pub scope: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub stats_json: Option<String>,
}

pub fn now_utc() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
