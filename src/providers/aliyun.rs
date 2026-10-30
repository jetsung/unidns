use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha1::Sha1;

use super::{
    DnsProvider, ProviderDomain, ProviderError, ProviderRecord, ProviderZone, RecordInput,
};

const DEFAULT_ENDPOINT: &str = "https://alidns.aliyuncs.com";
const API_VERSION: &str = "2015-01-09";
const PAGE_SIZE: i64 = 100;

#[derive(serde::Deserialize, Clone)]
pub struct AliyunCredentials {
    pub access_key_id: String,
    pub access_key_secret: String,
}

pub struct AliyunProvider {
    client: reqwest::Client,
    credentials: AliyunCredentials,
    endpoint: String,
}

/// RFC3986 percent-encode：unreserved 字符（A-Za-z0-9 - _ . ~）之外全部 %XX 大写。
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 阿里云 RPC V1 签名（POP / HMAC-SHA1）：返回 Signature 值。
pub(crate) fn sign_pop(secret: &str, params: &BTreeMap<String, String>) -> String {
    let canonical = params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let string_to_sign = format!("GET&%2F&{}", percent_encode(&canonical));
    let mut mac = Hmac::<Sha1>::new_from_slice(format!("{secret}&").as_bytes())
        .expect("HMAC-SHA1 接受任意长度密钥");
    mac.update(string_to_sign.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

impl AliyunProvider {
    pub fn new(credentials: AliyunCredentials) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("HTTP client 构建失败"),
            credentials,
            endpoint: DEFAULT_ENDPOINT.to_string(),
        }
    }

    /// 测试或代理场景下覆盖 endpoint。
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// 实时探测单个域名的 NS 是否全部指向阿里云解析。
    /// 列表接口的 DnsServers 是添加域名时记录的值，注册商改 NS 后不会更新，不可靠。
    async fn describe_domain_ns_hosted(&self, domain: &str) -> Result<bool, ProviderError> {
        let mut params = BTreeMap::new();
        params.insert("DomainName".into(), domain.to_string());
        let body = self.call("DescribeDomainNs", params).await?;
        Ok(ns_is_all_aliyun(&body))
    }

    async fn call(
        &self,
        action: &str,
        mut params: BTreeMap<String, String>,
    ) -> Result<Value, ProviderError> {
        params.insert("Action".into(), action.into());
        params.insert("Format".into(), "JSON".into());
        params.insert("Version".into(), API_VERSION.into());
        params.insert("AccessKeyId".into(), self.credentials.access_key_id.clone());
        params.insert("SignatureMethod".into(), "HMAC-SHA1".into());
        params.insert("SignatureVersion".into(), "1.0".into());
        params.insert("SignatureNonce".into(), uuid_like_nonce());
        params.insert(
            "Timestamp".into(),
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        );
        let signature = sign_pop(&self.credentials.access_key_secret, &params);
        params.insert("Signature".into(), signature);

        let query = params
            .iter()
            .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        let resp = self
            .client
            .get(format!("{}/?{}", self.endpoint, query))
            .send()
            .await
            .map_err(ProviderError::network)?;
        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| ProviderError::Api {
            code: "BadResponse".into(),
            message: format!("响应不是合法 JSON: {e}"),
        })?;
        if !status.is_success() {
            return Err(map_error(&body));
        }
        Ok(body)
    }
}

fn uuid_like_nonce() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

fn map_error(body: &Value) -> ProviderError {
    let code = body
        .get("Code")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown")
        .to_string();
    let message = body
        .get("Message")
        .and_then(|v| v.as_str())
        .unwrap_or("阿里云返回未知错误")
        .to_string();
    match code.as_str() {
        "InvalidAccessKeyId.NotFound"
        | "InvalidAccessKeyId"
        | "SignatureDoesNotMatch"
        | "Forbidden"
        | "Unauthorized" => ProviderError::Auth(format!("阿里云凭据无效: {message}")),
        "Throttling" | "Throttling.User" | "Throttling.UserQos" | "FlowLimitExceeded" => {
            ProviderError::RateLimited {
                retry_after: None,
                message: format!("阿里云限流: {message}"),
            }
        }
        "InvalidDomain.NotFound" | "InvalidRecord.NotFound" | "DomainNotExists" => {
            ProviderError::NotFound(message)
        }
        _ => ProviderError::Api { code, message },
    }
}

fn parse_domain(v: &Value) -> Option<ProviderDomain> {
    Some(ProviderDomain {
        name: v.get("DomainName")?.as_str()?.to_string(),
        provider_domain_id: String::new(),
        // 阿里云 DescribeDomains 返回 Remark 字段（域名备注）
        remark: v
            .get("Remark")
            .and_then(|r| r.as_str())
            .filter(|r| !r.is_empty())
            .map(String::from),
        // 阿里云 DescribeDomains 不返回 Status 字段（域名无启停概念），
        // 列出的均为已托管域名，统一记为 active。
        status: v
            .get("Status")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("active")
            .to_string(),
    })
}

/// DescribeDomainNs 响应中 AllAliDns=true 表示域名 NS 已全部指向阿里云解析（托管生效）。
fn ns_is_all_aliyun(body: &Value) -> bool {
    body.get("AllAliDns")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

fn parse_record(v: &Value) -> Option<ProviderRecord> {
    Some(ProviderRecord {
        provider_record_id: v.get("RecordId")?.as_str()?.to_string(),
        host: v.get("RR")?.as_str()?.to_string(),
        rtype: v.get("Type")?.as_str()?.to_string(),
        value: v.get("Value")?.as_str()?.to_string(),
        ttl: v.get("TTL").and_then(|t| t.as_i64()).unwrap_or(600) as u64,
        line: v.get("Line").and_then(|l| l.as_str()).map(String::from),
        proxied: None,
        enabled: v
            .get("Status")
            .and_then(|s| s.as_str())
            .map(|s| s.eq_ignore_ascii_case("Enable"))
            .unwrap_or(true),
        remark: v
            .get("Remark")
            .and_then(|r| r.as_str())
            .filter(|r| !r.is_empty())
            .map(String::from),
        weight: None,
        priority: None,
    })
}

#[async_trait]
impl DnsProvider for AliyunProvider {
    fn supports_record_remark(&self) -> bool {
        true
    }

    async fn verify(&self) -> Result<(), ProviderError> {
        let mut params = BTreeMap::new();
        params.insert("PageSize".into(), "1".into());
        params.insert("PageNumber".into(), "1".into());
        self.call("DescribeDomains", params).await?;
        Ok(())
    }

    async fn list_domains(&self) -> Result<Vec<ProviderDomain>, ProviderError> {
        let mut fetched = Vec::new();
        let mut page = 1i64;
        loop {
            let mut params = BTreeMap::new();
            params.insert("PageSize".into(), PAGE_SIZE.to_string());
            params.insert("PageNumber".into(), page.to_string());
            let body = self.call("DescribeDomains", params).await?;
            let batch: Vec<ProviderDomain> = body
                .pointer("/Domains/Domain")
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(parse_domain)
                .collect();
            let done = (batch.len() as i64) < PAGE_SIZE;
            fetched.extend(batch);
            if done {
                break;
            }
            page += 1;
        }
        // 逐域名实时探测 NS：只保留 NS 已全部指向阿里云解析（托管生效）的域名
        let mut domains = Vec::new();
        for d in fetched {
            match self.describe_domain_ns_hosted(&d.name).await {
                Ok(true) => domains.push(d),
                Ok(false) => {
                    tracing::debug!(domain = %d.name, "域名 NS 未指向阿里云解析，跳过");
                }
                Err(e) => {
                    tracing::warn!(domain = %d.name, error = %e, "NS 探测失败，保守保留该域名");
                    domains.push(d);
                }
            }
        }
        Ok(domains)
    }

    async fn list_records(
        &self,
        zone: &ProviderZone,
    ) -> Result<Vec<ProviderRecord>, ProviderError> {
        let mut records = Vec::new();
        let mut page = 1i64;
        loop {
            let mut params = BTreeMap::new();
            params.insert("DomainName".into(), zone.name.clone());
            params.insert("PageSize".into(), PAGE_SIZE.to_string());
            params.insert("PageNumber".into(), page.to_string());
            let body = self.call("DescribeDomainRecords", params).await?;
            let batch: Vec<ProviderRecord> = body
                .pointer("/DomainRecords/Record")
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
            page += 1;
        }
        Ok(records)
    }

    async fn create_record(
        &self,
        zone: &ProviderZone,
        input: &RecordInput,
    ) -> Result<ProviderRecord, ProviderError> {
        let mut params = BTreeMap::new();
        params.insert("DomainName".into(), zone.name.clone());
        params.insert("RR".into(), input.host.clone());
        params.insert("Type".into(), input.rtype.clone());
        params.insert("Value".into(), input.value.clone());
        params.insert("TTL".into(), input.ttl.to_string());
        params.insert(
            "Line".into(),
            input.line.clone().unwrap_or_else(|| "default".into()),
        );
        let body = self.call("AddDomainRecord", params).await?;
        let record_id = body
            .get("RecordId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProviderError::Api {
                code: "BadResponse".into(),
                message: "阿里云未返回 RecordId".into(),
            })?
            .to_string();
        let record = ProviderRecord {
            provider_record_id: record_id.clone(),
            host: input.host.clone(),
            rtype: input.rtype.clone(),
            value: input.value.clone(),
            ttl: input.ttl,
            line: Some(input.line.clone().unwrap_or_else(|| "default".into())),
            proxied: None,
            enabled: true,
            remark: input.remark.clone(),
            weight: None,
            priority: None,
        };
        // 阿里云创建接口不支持备注，创建后单独设置
        if let Some(r) = input.remark.as_deref().filter(|r| !r.is_empty()) {
            self.set_record_remark(zone, &record_id, r).await?;
        }
        Ok(record)
    }

    async fn update_record(
        &self,
        zone: &ProviderZone,
        record: &ProviderRecord,
    ) -> Result<ProviderRecord, ProviderError> {
        let mut params = BTreeMap::new();
        params.insert("RecordId".into(), record.provider_record_id.clone());
        params.insert("RR".into(), record.host.clone());
        params.insert("Type".into(), record.rtype.clone());
        params.insert("Value".into(), record.value.clone());
        params.insert("TTL".into(), record.ttl.to_string());
        params.insert(
            "Line".into(),
            record.line.clone().unwrap_or_else(|| "default".into()),
        );
        let _ = zone; // 阿里云以 RecordId 定位，无需域名
        self.call("UpdateDomainRecord", params).await?;
        // 主接口不支持备注：无论是否有备注输入，统一同步一次（幂等，空串清除）
        self.set_record_remark(
            zone,
            &record.provider_record_id,
            record.remark.as_deref().unwrap_or(""),
        )
        .await?;
        Ok(record.clone())
    }

    async fn delete_record(
        &self,
        _zone: &ProviderZone,
        provider_record_id: &str,
    ) -> Result<(), ProviderError> {
        let mut params = BTreeMap::new();
        params.insert("RecordId".into(), provider_record_id.to_string());
        self.call("DeleteDomainRecord", params).await?;
        Ok(())
    }

    async fn set_record_status(
        &self,
        _zone: &ProviderZone,
        provider_record_id: &str,
        enabled: bool,
    ) -> Result<(), ProviderError> {
        let mut params = BTreeMap::new();
        params.insert("RecordId".into(), provider_record_id.to_string());
        params.insert(
            "Status".into(),
            if enabled { "Enable" } else { "Disable" }.into(),
        );
        self.call("SetDomainRecordStatus", params).await?;
        Ok(())
    }

    /// 阿里云备注修改走专用接口 UpdateDomainRecordRemark。
    async fn set_record_remark(
        &self,
        _zone: &ProviderZone,
        provider_record_id: &str,
        remark: &str,
    ) -> Result<(), ProviderError> {
        let mut params = BTreeMap::new();
        params.insert("RecordId".into(), provider_record_id.to_string());
        params.insert("Remark".into(), remark.to_string());
        self.call("UpdateDomainRecordRemark", params).await?;
        Ok(())
    }

    /// 阿里云域名备注修改走专用接口 UpdateDomainRemark（Remark 上限 50 字符，空串清除）。
    async fn set_domain_remark(
        &self,
        domain_name: &str,
        remark: &str,
    ) -> Result<(), ProviderError> {
        let mut params = BTreeMap::new();
        params.insert("DomainName".into(), domain_name.to_string());
        params.insert("Remark".into(), remark.to_string());
        self.call("UpdateDomainRemark", params).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_rfc3986() {
        assert_eq!(
            percent_encode("默认线路"),
            "%E9%BB%98%E8%AE%A4%E7%BA%BF%E8%B7%AF"
        );
        assert_eq!(percent_encode("*.example.com"), "%2A.example.com");
        assert_eq!(percent_encode("aZ09-_.~"), "aZ09-_.~");
        assert_eq!(
            percent_encode("2026-01-01T00:00:00Z"),
            "2026-01-01T00%3A00%3A00Z"
        );
    }

    /// 基线向量由独立实现（Python，RFC3986 + HMAC-SHA1）生成。
    #[test]
    fn pop_signature_baseline() {
        let params: BTreeMap<String, String> = [
            ("AccessKeyId", "testkeyid"),
            ("Action", "DescribeDomains"),
            ("Format", "JSON"),
            ("PageNumber", "1"),
            ("PageSize", "20"),
            ("SignatureMethod", "HMAC-SHA1"),
            ("SignatureNonce", "nonce-abc"),
            ("SignatureVersion", "1.0"),
            ("Timestamp", "2026-01-01T00:00:00Z"),
            ("Version", "2015-01-09"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        assert_eq!(
            sign_pop("testsecret", &params),
            "UqACuUOUA1QlnatcSKhdUCNlDsA="
        );
    }

    #[test]
    fn all_ali_dns_judges_hosting() {
        // 真实响应样例：NS 已指向阿里云
        let hosted: Value = serde_json::from_str(
            r#"{"AllAliDns":true,"ExpectDnsServers":{"ExpectDnsServer":["ns1.alidns.com","ns2.alidns.com"]},
                "DnsServers":{"DnsServer":["ns1.alidns.com"]},"IncludeAliDns":false}"#,
        )
        .unwrap();
        assert!(ns_is_all_aliyun(&hosted));

        // 真实响应样例：NS 已切到 DNSPod
        let moved: Value = serde_json::from_str(
            r#"{"AllAliDns":false,"ExpectDnsServers":{"ExpectDnsServer":["ns1.alidns.com","ns2.alidns.com"]},
                "DnsServers":{"DnsServer":["c.dnspod.com","b.dnspod.com","a.dnspod.com"]},"IncludeAliDns":false}"#,
        )
        .unwrap();
        assert!(!ns_is_all_aliyun(&moved));

        // 缺字段时保守视为托管
        assert!(ns_is_all_aliyun(&serde_json::json!({})));

        // parse_domain 解析域名列表：Remark 备注与 Status 缺省（阿里云无启停概念，兜底 active）
        let d = serde_json::json!({
            "DomainName": "chunghwa.fun", "Remark": "主站域名",
            "DnsServers": {"DnsServer": ["dns17.hichina.com"]}
        });
        let parsed = parse_domain(&d).unwrap();
        assert_eq!(parsed.name, "chunghwa.fun");
        assert_eq!(parsed.remark.as_deref(), Some("主站域名"));
        assert_eq!(parsed.status, "active");
        let no_remark: Value = serde_json::json!({"DomainName": "a.com"});
        let parsed2 = parse_domain(&no_remark).unwrap();
        assert_eq!(parsed2.remark, None);
        assert_eq!(parsed2.status, "active");
    }

    #[test]
    fn record_parsing() {
        let v: Value = serde_json::json!({
            "RecordId": "12345",
            "RR": "www",
            "Type": "A",
            "Value": "1.2.3.4",
            "TTL": 600,
            "Line": "default",
            "Status": "Enable",
            "Remark": "测试备注"
        });
        let r = parse_record(&v).unwrap();
        assert_eq!(r.provider_record_id, "12345");
        assert_eq!(r.host, "www");
        assert_eq!(r.remark.as_deref(), Some("测试备注"));
        assert_eq!(r.rtype, "A");
        assert_eq!(r.value, "1.2.3.4");
        assert_eq!(r.ttl, 600);
        assert!(r.enabled);

        let v_disable: Value = serde_json::json!({
            "RecordId": "1", "RR": "@", "Type": "AAAA", "Value": "::1", "TTL": 300,
            "Line": "telecom", "Status": "Disable"
        });
        assert!(!parse_record(&v_disable).unwrap().enabled);
        assert_eq!(parse_record(&v_disable).unwrap().host, "@");
    }

    #[test]
    fn error_mapping() {
        let auth = serde_json::json!({"Code": "InvalidAccessKeyId.NotFound", "Message": "bad key"});
        assert!(matches!(map_error(&auth), ProviderError::Auth(_)));
        let throttle = serde_json::json!({"Code": "Throttling", "Message": "slow down"});
        assert!(matches!(
            map_error(&throttle),
            ProviderError::RateLimited { .. }
        ));
        let notfound = serde_json::json!({"Code": "InvalidRecord.NotFound", "Message": "no rec"});
        assert!(matches!(map_error(&notfound), ProviderError::NotFound(_)));
        let other = serde_json::json!({"Code": "SomeError", "Message": "boom"});
        assert!(matches!(map_error(&other), ProviderError::Api { .. }));
    }

    type Captured =
        std::sync::Arc<tokio::sync::Mutex<Vec<std::collections::HashMap<String, String>>>>;

    async fn capture_handler(
        axum::extract::State(captured): axum::extract::State<Captured>,
        uri: axum::http::Uri,
    ) -> axum::Json<Value> {
        let params = uri
            .query()
            .unwrap_or_default()
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        captured.lock().await.push(params);
        axum::Json(serde_json::json!({"RecordId": "1"}))
    }

    async fn spawn_capture() -> (String, Captured) {
        use axum::{Router, routing::any};
        let captured: Captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/", any(capture_handler))
            .with_state(captured.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), captured)
    }

    fn test_record(remark: Option<String>) -> ProviderRecord {
        ProviderRecord {
            provider_record_id: "1".into(),
            host: "www".into(),
            rtype: "A".into(),
            value: "1.2.3.4".into(),
            ttl: 600,
            line: Some("default".into()),
            proxied: None,
            enabled: true,
            remark,
            weight: None,
            priority: None,
        }
    }

    /// 创建后与每次修改后调用 UpdateDomainRecordRemark；空串清除备注。
    #[tokio::test]
    async fn create_and_update_write_remark_including_empty() {
        let (base, captured) = spawn_capture().await;
        let p = AliyunProvider::new(AliyunCredentials {
            access_key_id: "id".into(),
            access_key_secret: "key".into(),
        })
        .with_endpoint(&base);
        let zone = ProviderZone {
            name: "example.com".into(),
            provider_domain_id: String::new(),
        };
        let input = RecordInput {
            host: "www".into(),
            rtype: "A".into(),
            value: "1.2.3.4".into(),
            ttl: 600,
            line: None,
            proxied: None,
            remark: Some("prod".into()),
            weight: None,
            priority: None,
        };
        p.create_record(&zone, &input).await.unwrap();
        p.update_record(&zone, &test_record(Some("updated".into())))
            .await
            .unwrap();
        p.update_record(&zone, &test_record(Some(String::new())))
            .await
            .unwrap();

        let reqs = captured.lock().await;
        assert_eq!(reqs[0]["Action"], "AddDomainRecord");
        assert_eq!(reqs[1]["Action"], "UpdateDomainRecordRemark");
        assert_eq!(reqs[1]["Remark"], "prod");
        assert_eq!(reqs[2]["Action"], "UpdateDomainRecord");
        assert_eq!(reqs[3]["Action"], "UpdateDomainRecordRemark");
        assert_eq!(reqs[3]["Remark"], "updated");
        assert_eq!(reqs[5]["Action"], "UpdateDomainRecordRemark");
        assert_eq!(reqs[5]["Remark"], "", "清空备注需下发空串");
    }
}
