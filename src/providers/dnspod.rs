use async_trait::async_trait;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::time::Duration;

use super::{
    DnsProvider, ProviderDomain, ProviderError, ProviderRecord, ProviderZone, RecordInput,
};

const CHINA_ENDPOINT: &str = "https://dnspod.tencentcloudapi.com";
const INTL_ENDPOINT: &str = "https://dnspod.intl.tencentcloudapi.com";
const API_VERSION: &str = "2021-03-23";
const PAGE_SIZE: i64 = 100;

/// DNSPod 账户所属站点：国内站与国际站的端点和密钥体系相互独立。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnspodSite {
    China,
    International,
}

impl DnspodSite {
    pub fn endpoint(&self) -> &'static str {
        match self {
            DnspodSite::China => CHINA_ENDPOINT,
            DnspodSite::International => INTL_ENDPOINT,
        }
    }

    /// 默认解析线路名称：国内站为中文「默认」，国际站为 "Default"。
    /// 国际站线路名称大小写敏感（DescribeRecordLineList 首条 {"LineId":"0","Name":"Default"}），
    /// CreateRecord/ModifyRecord 的 RecordLine 传名称（不接受 LineId "0"）。
    pub fn default_line(&self) -> &'static str {
        match self {
            DnspodSite::China => "默认",
            DnspodSite::International => "Default",
        }
    }
}

#[derive(serde::Deserialize, Clone)]
pub struct DnspodCredentials {
    pub secret_id: String,
    pub secret_key: String,
}

pub struct DnspodProvider {
    client: reqwest::Client,
    credentials: DnspodCredentials,
    endpoint: String,
    site: DnspodSite,
}

pub(crate) struct Tc3Input<'a> {
    pub secret_id: &'a str,
    pub secret_key: &'a str,
    pub timestamp: i64,
    pub service: &'a str,
    pub host: &'a str,
    pub action: &'a str,
    pub payload: &'a str,
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(msg);
    let mut out = [0u8; 32];
    out.copy_from_slice(&mac.finalize().into_bytes());
    out
}

/// 腾讯云 TC3-HMAC-SHA256 签名：返回完整 Authorization 头。
pub(crate) fn tc3_authorization(input: &Tc3Input) -> String {
    let date = chrono::DateTime::from_timestamp(input.timestamp, 0)
        .expect("时间戳合法")
        .format("%Y-%m-%d")
        .to_string();

    let canonical_request = format!(
        "POST\n/\n\ncontent-type:application/json; charset=utf-8\nhost:{}\nx-tc-action:{}\n\ncontent-type;host;x-tc-action\n{}",
        input.host,
        input.action.to_lowercase(),
        sha256_hex(input.payload.as_bytes())
    );

    let credential_scope = format!("{}/{}/tc3_request", date, input.service);
    let string_to_sign = format!(
        "TC3-HMAC-SHA256\n{}\n{}\n{}",
        input.timestamp,
        credential_scope,
        sha256_hex(canonical_request.as_bytes())
    );

    let secret_date = hmac_sha256(
        format!("TC3{}", input.secret_key).as_bytes(),
        date.as_bytes(),
    );
    let secret_service = hmac_sha256(&secret_date, input.service.as_bytes());
    let secret_signing = hmac_sha256(&secret_service, b"tc3_request");
    let signature = hex::encode(hmac_sha256(&secret_signing, string_to_sign.as_bytes()));

    format!(
        "TC3-HMAC-SHA256 Credential={}/{}, SignedHeaders=content-type;host;x-tc-action, Signature={}",
        input.secret_id, credential_scope, signature
    )
}

impl DnspodProvider {
    pub fn new(credentials: DnspodCredentials, site: DnspodSite) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("HTTP client 构建失败"),
            credentials,
            endpoint: site.endpoint().to_string(),
            site,
        }
    }

    fn default_line(&self) -> String {
        self.site.default_line().to_string()
    }

    /// 测试或代理场景下覆盖 endpoint。
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    fn host_header(&self) -> String {
        self.endpoint
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string()
    }

    async fn call(&self, action: &str, payload: Value) -> Result<Value, ProviderError> {
        let body = serde_json::to_string(&payload).expect("JSON 序列化不会失败");
        let timestamp = chrono::Utc::now().timestamp();
        let host = self.host_header();
        let authorization = tc3_authorization(&Tc3Input {
            secret_id: &self.credentials.secret_id,
            secret_key: &self.credentials.secret_key,
            timestamp,
            service: "dnspod",
            host: &host,
            action,
            payload: &body,
        });

        let resp = self
            .client
            .post(format!("{}/", self.endpoint))
            .header("Authorization", authorization)
            .header("Content-Type", "application/json; charset=utf-8")
            .header("X-TC-Action", action)
            .header("X-TC-Timestamp", timestamp.to_string())
            .header("X-TC-Version", API_VERSION)
            .body(body)
            .send()
            .await
            .map_err(ProviderError::network)?;

        let status = resp.status();
        let parsed: Value = resp.json().await.map_err(|e| ProviderError::Api {
            code: "BadResponse".into(),
            message: format!("响应不是合法 JSON: {e}"),
        })?;
        if !status.is_success() {
            // 腾讯云在非 2xx 时也可能返回结构化错误
            if let Some(err) = parsed.pointer("/Response/Error") {
                return Err(map_error(err));
            }
            return Err(ProviderError::Api {
                code: format!("HTTP {status}"),
                message: "腾讯云返回非 2xx 响应且无错误详情".into(),
            });
        }
        if let Some(err) = parsed.pointer("/Response/Error") {
            return Err(map_error(err));
        }
        Ok(parsed["Response"].clone())
    }
}

fn map_error(err: &Value) -> ProviderError {
    let code = err
        .get("Code")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown")
        .to_string();
    let message = err
        .get("Message")
        .and_then(|v| v.as_str())
        .unwrap_or("腾讯云返回未知错误")
        .to_string();
    if code.starts_with("AuthFailure") || code.starts_with("InvalidCredential") {
        ProviderError::Auth(format!("腾讯云凭据无效: {message}"))
    } else if code == "UnauthorizedOperation" {
        ProviderError::Auth(format!(
            "腾讯云密钥无权限（CAM 未授权该接口或服务未开通）: {message}"
        ))
    } else if code.contains("LimitExceeded") || code.contains("RequestLimit") {
        ProviderError::RateLimited {
            retry_after: None,
            message: format!("腾讯云限流: {message}"),
        }
    } else if code.starts_with("ResourceNotFound") {
        ProviderError::NotFound(message)
    } else {
        ProviderError::Api { code, message }
    }
}

/// 域名的 NS 是否正确指向 DNSPod。
/// `DNSStatus` 为 "DNSERROR" 表示 NS 未按分配指向，解析托管无效；空或其他值视为正常。
fn is_dns_hosted(v: &Value) -> bool {
    v.get("DNSStatus")
        .and_then(|s| s.as_str())
        .map(|s| !s.eq_ignore_ascii_case("DNSERROR"))
        .unwrap_or(true)
}

fn parse_domain(v: &Value) -> Option<ProviderDomain> {
    if !is_dns_hosted(v) {
        return None;
    }
    Some(ProviderDomain {
        name: v.get("Name")?.as_str()?.to_string(),
        provider_domain_id: v
            .get("DomainId")
            .map(|v| v.to_string().trim_matches('"').to_string())
            .unwrap_or_default(),
        remark: v
            .get("Remark")
            .and_then(|r| r.as_str())
            .filter(|r| !r.is_empty())
            .map(String::from),
        status: v
            .get("Status")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

fn parse_record(v: &Value) -> Option<ProviderRecord> {
    let record_id = match v.get("RecordId")? {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return None,
    };
    Some(ProviderRecord {
        provider_record_id: record_id,
        host: v
            .get("Name")
            .and_then(|n| n.as_str())
            .unwrap_or("@")
            .to_string(),
        rtype: v.get("Type")?.as_str()?.to_string(),
        value: v.get("Value")?.as_str()?.to_string(),
        ttl: v.get("TTL").and_then(|t| t.as_i64()).unwrap_or(600) as u64,
        line: v.get("Line").and_then(|l| l.as_str()).map(String::from),
        proxied: None,
        enabled: v
            .get("Status")
            .and_then(|s| s.as_str())
            .map(|s| s.eq_ignore_ascii_case("ENABLE"))
            .unwrap_or(true),
        remark: v
            .get("Remark")
            .and_then(|r| r.as_str())
            .filter(|r| !r.is_empty())
            .map(String::from),
    })
}

#[async_trait]
impl DnsProvider for DnspodProvider {
    async fn verify(&self) -> Result<(), ProviderError> {
        let payload = serde_json::json!({"Limit": 1, "Offset": 0});
        self.call("DescribeDomainList", payload).await?;
        Ok(())
    }

    async fn list_domains(&self) -> Result<Vec<ProviderDomain>, ProviderError> {
        let mut domains = Vec::new();
        let mut offset = 0i64;
        loop {
            let payload = serde_json::json!({"Limit": PAGE_SIZE, "Offset": offset});
            let body = self.call("DescribeDomainList", payload).await?;
            let batch: Vec<ProviderDomain> = body
                .get("DomainList")
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(parse_domain)
                .collect();
            let done = (batch.len() as i64) < PAGE_SIZE;
            domains.extend(batch);
            if done {
                break;
            }
            offset += PAGE_SIZE;
        }
        Ok(domains)
    }

    async fn list_records(
        &self,
        zone: &ProviderZone,
    ) -> Result<Vec<ProviderRecord>, ProviderError> {
        let mut records = Vec::new();
        let mut offset = 0i64;
        loop {
            let payload = serde_json::json!({
                "Domain": zone.name,
                "Limit": PAGE_SIZE,
                "Offset": offset,
            });
            let body = self.call("DescribeRecordList", payload).await?;
            // 无记录时 DNSPod 可能不返回 RecordList 字段
            let batch: Vec<ProviderRecord> = body
                .get("RecordList")
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(parse_record)
                .collect();
            let done = (batch.len() as i64) < PAGE_SIZE;
            records.extend(batch);
            if done {
                break;
            }
            offset += PAGE_SIZE;
        }
        Ok(records)
    }

    async fn create_record(
        &self,
        zone: &ProviderZone,
        input: &RecordInput,
    ) -> Result<ProviderRecord, ProviderError> {
        let mut payload = serde_json::json!({
            "Domain": zone.name,
            "SubDomain": input.host,
            "RecordType": input.rtype,
            "RecordLine": input.line.clone().unwrap_or_else(|| self.default_line()),
            "Value": input.value,
            "TTL": input.ttl,
        });
        if let Some(r) = input.remark.as_deref().filter(|r| !r.is_empty()) {
            payload["Remark"] = serde_json::Value::String(r.to_string());
        }
        let body = self.call("CreateRecord", payload).await?;
        let record_id = match body.get("RecordId") {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ProviderError::Api {
                    code: "BadResponse".into(),
                    message: "腾讯云未返回 RecordId".into(),
                });
            }
        };
        Ok(ProviderRecord {
            provider_record_id: record_id,
            host: input.host.clone(),
            rtype: input.rtype.clone(),
            value: input.value.clone(),
            ttl: input.ttl,
            line: Some(input.line.clone().unwrap_or_else(|| self.default_line())),
            proxied: None,
            enabled: true,
            remark: input.remark.clone(),
        })
    }

    async fn update_record(
        &self,
        zone: &ProviderZone,
        record: &ProviderRecord,
    ) -> Result<ProviderRecord, ProviderError> {
        let mut payload = serde_json::json!({
            "Domain": zone.name,
            "RecordId": record.provider_record_id.parse::<i64>().map_err(|_| ProviderError::Api {
                code: "BadRecordId".into(),
                message: format!("腾讯云记录 ID 必须是数字: {}", record.provider_record_id),
            })?,
            "SubDomain": record.host,
            "RecordType": record.rtype,
            "RecordLine": record.line.clone().unwrap_or_else(|| self.default_line()),
            "Value": record.value,
            "TTL": record.ttl,
        });
        if let Some(r) = record.remark.as_deref() {
            payload["Remark"] = serde_json::Value::String(r.to_string());
        }
        self.call("ModifyRecord", payload).await?;
        Ok(record.clone())
    }

    async fn delete_record(
        &self,
        zone: &ProviderZone,
        provider_record_id: &str,
    ) -> Result<(), ProviderError> {
        let payload = serde_json::json!({
            "Domain": zone.name,
            "RecordId": provider_record_id.parse::<i64>().map_err(|_| ProviderError::Api {
                code: "BadRecordId".into(),
                message: format!("腾讯云记录 ID 必须是数字: {provider_record_id}"),
            })?,
        });
        self.call("DeleteRecord", payload).await?;
        Ok(())
    }

    async fn set_record_status(
        &self,
        zone: &ProviderZone,
        provider_record_id: &str,
        enabled: bool,
    ) -> Result<(), ProviderError> {
        let payload = serde_json::json!({
            "Domain": zone.name,
            "RecordId": provider_record_id.parse::<i64>().map_err(|_| ProviderError::Api {
                code: "BadRecordId".into(),
                message: format!("腾讯云记录 ID 必须是数字: {provider_record_id}"),
            })?,
            "Status": if enabled { "ENABLE" } else { "DISABLE" },
        });
        // 2021-03-23 版 DNSPod API 的记录启停 Action 为 ModifyRecordStatus
        self.call("ModifyRecordStatus", payload).await?;
        Ok(())
    }

    /// 更新域名备注（空字符串清除上游备注）。国内站与国际站共用 ModifyDomainRemark。
    async fn set_domain_remark(
        &self,
        domain_name: &str,
        remark: &str,
    ) -> Result<(), ProviderError> {
        let payload = serde_json::json!({
            "Domain": domain_name,
            "Remark": remark,
        });
        self.call("ModifyDomainRemark", payload).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 基线向量由独立实现（Python，官方 TC3-HMAC-SHA256 算法）生成。
    #[test]
    fn tc3_signature_baseline() {
        let auth = tc3_authorization(&Tc3Input {
            secret_id: "AKIDtest",
            secret_key: "testkey",
            timestamp: 1551113065,
            service: "dnspod",
            host: "dnspod.tencentcloudapi.com",
            action: "DescribeRecordList",
            payload: r#"{"Domain":"example.com","Limit":20,"Offset":0}"#,
        });
        assert_eq!(
            auth,
            "TC3-HMAC-SHA256 Credential=AKIDtest/2019-02-25/dnspod/tc3_request, \
             SignedHeaders=content-type;host;x-tc-action, \
             Signature=2ea8fa24e4ccf049105b13ee826fa606dfa6c1f160fa1e7151568725e8e329d4"
        );
    }

    #[test]
    fn dnserror_domains_are_excluded() {
        // DNSStatus = DNSERROR：NS 未正确指向 DNSPod，排除
        let err = serde_json::json!({
            "Name": "asfd.cn", "Status": "ENABLE", "DNSStatus": "DNSERROR", "DomainId": 1
        });
        assert!(parse_domain(&err).is_none());

        // 空状态（正常托管）与缺失字段：保留
        let ok = serde_json::json!({
            "Name": "x.idev.top", "Status": "ENABLE", "DNSStatus": "", "DomainId": 2
        });
        assert_eq!(parse_domain(&ok).unwrap().name, "x.idev.top");
        let missing = serde_json::json!({"Name": "a.com", "Status": "ENABLE"});
        assert_eq!(parse_domain(&missing).unwrap().name, "a.com");
    }

    #[test]
    fn site_switches_endpoint_and_default_line() {
        let cn = DnspodProvider::new(
            DnspodCredentials {
                secret_id: "a".into(),
                secret_key: "b".into(),
            },
            DnspodSite::China,
        );
        let intl = DnspodProvider::new(
            DnspodCredentials {
                secret_id: "a".into(),
                secret_key: "b".into(),
            },
            DnspodSite::International,
        );
        assert_eq!(cn.endpoint, "https://dnspod.tencentcloudapi.com");
        assert_eq!(cn.default_line(), "默认");
        assert_eq!(intl.endpoint, "https://dnspod.intl.tencentcloudapi.com");
        assert_eq!(intl.default_line(), "Default");
    }

    #[test]
    fn record_parsing_supports_numeric_and_string_ids() {
        let numeric = serde_json::json!({
            "RecordId": 12345, "Name": "www", "Type": "A", "Value": "1.2.3.4",
            "TTL": 600, "Line": "默认", "Status": "ENABLE", "Remark": "官网主页"
        });
        let r = parse_record(&numeric).unwrap();
        assert_eq!(r.provider_record_id, "12345");
        assert_eq!(r.line.as_deref(), Some("默认"));
        assert_eq!(r.remark.as_deref(), Some("官网主页"));
        assert!(r.enabled);

        let disabled = serde_json::json!({
            "RecordId": "99", "Name": "@", "Type": "CNAME", "Value": "a.b.c",
            "TTL": 300, "Status": "DISABLE"
        });
        let r = parse_record(&disabled).unwrap();
        assert!(!r.enabled);
    }

    #[test]
    fn error_mapping() {
        let auth = serde_json::json!({"Code": "AuthFailure.SignatureFailure", "Message": "bad"});
        assert!(matches!(map_error(&auth), ProviderError::Auth(_)));
        let limit = serde_json::json!({"Code": "RequestLimitExceeded", "Message": "slow"});
        assert!(matches!(
            map_error(&limit),
            ProviderError::RateLimited { .. }
        ));
        let notfound =
            serde_json::json!({"Code": "ResourceNotFound.NoDataOfRecord", "Message": "gone"});
        assert!(matches!(map_error(&notfound), ProviderError::NotFound(_)));
        let other =
            serde_json::json!({"Code": "FailedOperation.DomainNotVerified", "Message": "x"});
        assert!(matches!(map_error(&other), ProviderError::Api { .. }));
    }
}
