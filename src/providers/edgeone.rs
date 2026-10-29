use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    DnsProvider, ProviderDomain, ProviderError, ProviderRecord, ProviderZone, RecordInput,
    dnspod::{Tc3Input, tc3_authorization},
};

const CHINA_ENDPOINT: &str = "https://teo.tencentcloudapi.com";
const INTL_ENDPOINT: &str = "https://teo.intl.tencentcloudapi.com";
const API_VERSION: &str = "2022-09-01";
const ZONE_PAGE_SIZE: i64 = 200;
const RECORD_PAGE_SIZE: i64 = 1000;

/// EdgeOne 账户所属站点：国内站与国际站的端点与密钥体系相互独立。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeoSite {
    China,
    International,
}

impl TeoSite {
    pub fn endpoint(&self) -> &'static str {
        match self {
            TeoSite::China => CHINA_ENDPOINT,
            TeoSite::International => INTL_ENDPOINT,
        }
    }
}

#[derive(serde::Deserialize, Clone)]
pub struct EdgeoneCredentials {
    pub secret_id: String,
    pub secret_key: String,
}

pub struct EdgeoneProvider {
    client: reqwest::Client,
    credentials: EdgeoneCredentials,
    endpoint: String,
    /// 接口调用节流：两次调用之间的最小间隔（EdgeOne 有频率配额限制）。
    last_call: Arc<tokio::sync::Mutex<Instant>>,
}

/// EdgeOne API 的最小调用间隔。
const MIN_CALL_INTERVAL: Duration = Duration::from_millis(100);

impl EdgeoneProvider {
    pub fn new(credentials: EdgeoneCredentials, site: TeoSite) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("HTTP client 构建失败"),
            credentials,
            endpoint: site.endpoint().to_string(),
            last_call: Arc::new(tokio::sync::Mutex::new(
                Instant::now() - MIN_CALL_INTERVAL - MIN_CALL_INTERVAL,
            )),
        }
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
        // 节流：与上一次调用的间隔不足 MIN_CALL_INTERVAL 时等待补足
        {
            let mut last = self.last_call.lock().await;
            let elapsed = last.elapsed();
            if elapsed < MIN_CALL_INTERVAL {
                tokio::time::sleep(MIN_CALL_INTERVAL - elapsed).await;
            }
            *last = Instant::now();
        }
        let body = serde_json::to_string(&payload).expect("JSON 序列化不会失败");
        let timestamp = chrono::Utc::now().timestamp();
        let host = self.host_header();
        let authorization = tc3_authorization(&Tc3Input {
            secret_id: &self.credentials.secret_id,
            secret_key: &self.credentials.secret_key,
            timestamp,
            service: "teo",
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
            if let Some(err) = parsed.pointer("/Response/Error") {
                return Err(map_error(err));
            }
            return Err(ProviderError::Api {
                code: format!("HTTP {status}"),
                message: "EdgeOne 返回非 2xx 响应且无错误详情".into(),
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
        .unwrap_or("EdgeOne 返回未知错误")
        .to_string();
    if code.starts_with("AuthFailure") || code.starts_with("InvalidCredential") {
        ProviderError::Auth(format!("EdgeOne 凭据无效: {message}"))
    } else if code == "UnauthorizedOperation" {
        ProviderError::Auth(format!(
            "EdgeOne 密钥无权限（CAM 未授权该接口或服务未开通）: {message}"
        ))
    } else if code.contains("LimitExceeded") || code.contains("RequestLimit") {
        ProviderError::RateLimited {
            retry_after: None,
            message: format!("EdgeOne 限流: {message}"),
        }
    } else if code.starts_with("ResourceNotFound") || code.starts_with("ResourceUnavailable") {
        ProviderError::NotFound(message)
    } else {
        ProviderError::Api { code, message }
    }
}

/// EdgeOne 系统自动创建的特殊站点（非用户域名），同步时应排除。
/// `default-pages-zone` 是 EdgeOne Pages 托管服务的默认站点。
fn is_system_zone(name: &str) -> bool {
    name.eq_ignore_ascii_case("default-pages-zone")
}

/// 只有 NS 接入的站点才托管 DNS（有解析记录管理能力）。
/// CNAME 接入、EdgeOne Pages、无域名接入等模式的站点解析仍在用户原 DNS 服务商，
/// DescribeDnsRecords 对它们不可用，同步时应排除。
fn is_dns_managed_zone(v: &Value) -> bool {
    v.get("NSDetail").map(|d| !d.is_null()).unwrap_or(false)
}

fn parse_zone(v: &Value) -> Option<ProviderDomain> {
    if !is_dns_managed_zone(v) {
        return None;
    }
    Some(ProviderDomain {
        name: v.get("ZoneName")?.as_str()?.to_string(),
        provider_domain_id: v.get("ZoneId")?.as_str()?.to_string(),
        // EdgeOne 站点无备注概念
        remark: None,
        status: v
            .get("Status")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// EdgeOne 默认权重归一：-1 视为未设置。
fn normalize_weight(weight: Option<i64>) -> Option<i64> {
    match weight {
        Some(-1) => None,
        other => other,
    }
}

/// EdgeOne 默认优先级归一：0 视为未设置。
fn normalize_priority(priority: Option<i64>) -> Option<i64> {
    match priority {
        Some(0) => None,
        other => other,
    }
}

/// 按 EdgeOne 平台规则附加 Weight/Priority：不下发 Location；
/// weight 仅对 A/AAAA/CNAME 下发，priority 仅对 MX 下发（非 None 才下发）。
fn apply_weight_priority(
    payload: &mut serde_json::Map<String, Value>,
    rtype: &str,
    weight: Option<i64>,
    priority: Option<i64>,
) {
    if matches!(rtype, "A" | "AAAA" | "CNAME")
        && let Some(w) = weight
    {
        payload.insert("Weight".into(), w.into());
    }
    if rtype.eq_ignore_ascii_case("MX")
        && let Some(p) = priority
    {
        payload.insert("Priority".into(), p.into());
    }
}

fn parse_record(v: &Value, zone_name: &str) -> Option<ProviderRecord> {
    Some(ProviderRecord {
        provider_record_id: v.get("RecordId")?.as_str()?.to_string(),
        host: super::fqdn_to_host(v.get("Name")?.as_str()?, zone_name),
        rtype: v.get("Type")?.as_str()?.to_string(),
        value: v.get("Content")?.as_str()?.to_string(),
        ttl: v.get("TTL").and_then(|t| t.as_i64()).unwrap_or(300) as u64,
        // EdgeOne 的解析线路不纳入统一视图
        line: None,
        // EdgeOne 的加速代理在站点接入层，DNS 记录无 proxied 概念
        proxied: None,
        // EdgeOne 无解析记录备注
        remark: None,
        enabled: v
            .get("Status")
            .and_then(|s| s.as_str())
            .map(|s| s.eq_ignore_ascii_case("enable"))
            .unwrap_or(true),
        weight: normalize_weight(v.get("Weight").and_then(|w| w.as_i64())),
        priority: normalize_priority(v.get("Priority").and_then(|p| p.as_i64())),
    })
}

#[async_trait]
impl DnsProvider for EdgeoneProvider {
    async fn verify(&self) -> Result<(), ProviderError> {
        let payload = json!({"Limit": 1, "Offset": 0});
        self.call("DescribeZones", payload).await?;
        Ok(())
    }

    async fn list_domains(&self) -> Result<Vec<ProviderDomain>, ProviderError> {
        let mut zones = Vec::new();
        let mut offset = 0i64;
        loop {
            let payload = json!({"Limit": ZONE_PAGE_SIZE, "Offset": offset});
            let body = self.call("DescribeZones", payload).await?;
            let raw: Vec<Value> = body
                .get("Zones")
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();
            let fetched = raw.len() as i64;
            let total = body.get("TotalCount").and_then(|t| t.as_i64()).unwrap_or(0);
            zones.extend(
                raw.iter()
                    .filter_map(parse_zone)
                    .filter(|d| !is_system_zone(&d.name)),
            );
            // 终止以原始返回条数为准（TotalCount 含被过滤的非 NS 站点，
            // 用过滤后数量比较会在存在此类站点时死循环刷接口）
            if fetched == 0 || offset + fetched >= total {
                break;
            }
            offset += fetched;
        }
        Ok(zones)
    }

    async fn list_records(
        &self,
        zone: &ProviderZone,
    ) -> Result<Vec<ProviderRecord>, ProviderError> {
        let mut records = Vec::new();
        let mut offset = 0i64;
        loop {
            let payload = json!({
                "ZoneId": zone.provider_domain_id,
                "Limit": RECORD_PAGE_SIZE,
                "Offset": offset,
            });
            let body = self.call("DescribeDnsRecords", payload).await?;
            let raw: Vec<Value> = body
                .get("DnsRecords")
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();
            let fetched = raw.len() as i64;
            let total = body.get("TotalCount").and_then(|t| t.as_i64()).unwrap_or(0);
            records.extend(raw.iter().filter_map(|r| parse_record(r, &zone.name)));
            // 终止以原始返回条数为准，避免过滤导致死循环（同 list_domains）
            if fetched == 0 || offset + fetched >= total {
                break;
            }
            offset += fetched;
        }
        Ok(records)
    }

    async fn create_record(
        &self,
        zone: &ProviderZone,
        input: &RecordInput,
    ) -> Result<ProviderRecord, ProviderError> {
        let mut payload = json!({
            "ZoneId": zone.provider_domain_id,
            "Name": super::host_to_fqdn(&input.host, &zone.name),
            "Type": input.rtype,
            "Content": input.value,
            "TTL": input.ttl,
        });
        apply_weight_priority(
            payload.as_object_mut().expect("payload 为对象"),
            &input.rtype,
            input.weight,
            input.priority,
        );
        let body = self.call("CreateDnsRecord", payload).await?;
        let record_id = body
            .get("RecordId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProviderError::Api {
                code: "BadResponse".into(),
                message: "EdgeOne 未返回 RecordId".into(),
            })?
            .to_string();
        Ok(ProviderRecord {
            provider_record_id: record_id,
            host: input.host.clone(),
            rtype: input.rtype.clone(),
            value: input.value.clone(),
            ttl: input.ttl,
            // EdgeOne 的解析线路不纳入统一视图
            line: None,
            proxied: None,
            // EdgeOne 无备注概念
            remark: None,
            enabled: true,
            weight: normalize_weight(input.weight),
            priority: normalize_priority(input.priority),
        })
    }

    async fn update_record(
        &self,
        zone: &ProviderZone,
        record: &ProviderRecord,
    ) -> Result<ProviderRecord, ProviderError> {
        let mut item = json!({
            "RecordId": record.provider_record_id,
            "Name": super::host_to_fqdn(&record.host, &zone.name),
            "Type": record.rtype,
            "Content": record.value,
            "TTL": record.ttl,
        });
        apply_weight_priority(
            item.as_object_mut().expect("item 为对象"),
            &record.rtype,
            record.weight,
            record.priority,
        );
        let payload = json!({
            "ZoneId": zone.provider_domain_id,
            "DnsRecords": [item],
        });
        self.call("ModifyDnsRecords", payload).await?;
        Ok(ProviderRecord {
            line: None,
            weight: normalize_weight(record.weight),
            priority: normalize_priority(record.priority),
            ..record.clone()
        })
    }

    async fn delete_record(
        &self,
        zone: &ProviderZone,
        provider_record_id: &str,
    ) -> Result<(), ProviderError> {
        let payload = json!({
            "ZoneId": zone.provider_domain_id,
            "RecordIds": [provider_record_id],
        });
        self.call("DeleteDnsRecords", payload).await?;
        Ok(())
    }

    /// EdgeOne 支持记录级启停：ModifyDnsRecordsStatus（批量接口，单条启用/停用各传一个数组）。
    async fn set_record_status(
        &self,
        zone: &ProviderZone,
        provider_record_id: &str,
        enabled: bool,
    ) -> Result<(), ProviderError> {
        let payload = if enabled {
            json!({
                "ZoneId": zone.provider_domain_id,
                "RecordsToEnable": [provider_record_id],
            })
        } else {
            json!({
                "ZoneId": zone.provider_domain_id,
                "RecordsToDisable": [provider_record_id],
            })
        };
        self.call("ModifyDnsRecordsStatus", payload).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_endpoints() {
        let cred = EdgeoneCredentials {
            secret_id: "a".into(),
            secret_key: "b".into(),
        };
        let cn = EdgeoneProvider::new(cred.clone(), TeoSite::China);
        let intl = EdgeoneProvider::new(cred, TeoSite::International);
        assert_eq!(cn.endpoint, "https://teo.tencentcloudapi.com");
        assert_eq!(intl.endpoint, "https://teo.intl.tencentcloudapi.com");
    }

    #[test]
    fn only_ns_zones_are_parsed() {
        // NS 接入：保留
        let ns = serde_json::json!({
            "ZoneId": "zone-ns1", "ZoneName": "fx4.cn", "Status": "active",
            "NSDetail": {"NameServers": ["ns1.teodns.com."]}
        });
        assert_eq!(parse_zone(&ns).unwrap().name, "fx4.cn");

        // CNAME 接入：排除（无 DNS 托管）
        let cname = serde_json::json!({
            "ZoneId": "zone-c1", "ZoneName": "asfd.cn", "Status": "active",
            "NSDetail": Value::Null,
            "CNAMEDetail": {"CnameStatus": "finished"}
        });
        assert!(parse_zone(&cname).is_none());

        // 系统默认站点：排除
        let pages = serde_json::json!({
            "ZoneId": "zone-p1", "ZoneName": "default-pages-zone", "Status": "active",
            "PagesDetail": {}
        });
        assert!(parse_zone(&pages).is_none());
    }

    #[test]
    fn system_zone_filter() {
        assert!(is_system_zone("default-pages-zone"));
        assert!(is_system_zone("Default-Pages-Zone"));
        assert!(!is_system_zone("fx4.cn"));
        assert!(!is_system_zone("example.com"));
    }

    #[test]
    fn record_parsing() {
        let v = serde_json::json!({
            "RecordId": "record-3277epj0rm2y",
            "Name": "www.example.com",
            "Type": "A",
            "Content": "1.1.1.1",
            "TTL": 300,
            "Location": "Default",
            "Status": "enable"
        });
        let r = parse_record(&v, "example.com").unwrap();
        assert_eq!(r.provider_record_id, "record-3277epj0rm2y");
        assert_eq!(r.host, "www");
        // EdgeOne 不再读取 Location 作为线路字段
        assert_eq!(r.line, None);
        assert!(r.enabled);
        assert_eq!(r.proxied, None);

        let root = serde_json::json!({
            "RecordId": "record-root",
            "Name": "example.com",
            "Type": "A",
            "Content": "2.2.2.2",
            "TTL": 300,
            "Status": "disable"
        });
        let r = parse_record(&root, "example.com").unwrap();
        assert_eq!(r.host, "@");
        assert!(!r.enabled);
    }

    #[test]
    fn parse_record_reads_weight_and_priority() {
        let v = serde_json::json!({
            "RecordId": "rec-mx",
            "Name": "example.com",
            "Type": "MX",
            "Content": "mx1.example.com",
            "TTL": 300,
            "Priority": 10,
            "Weight": 40,
            "Location": "Default",
            "Status": "enable"
        });
        let r = parse_record(&v, "example.com").unwrap();
        assert_eq!(r.weight, Some(40));
        assert_eq!(r.priority, Some(10));
        assert_eq!(r.line, None);
    }

    #[test]
    fn weight_priority_normalization() {
        assert_eq!(normalize_weight(Some(-1)), None);
        assert_eq!(normalize_weight(Some(40)), Some(40));
        assert_eq!(normalize_priority(Some(0)), None);
        assert_eq!(normalize_priority(Some(10)), Some(10));

        let v = serde_json::json!({
            "RecordId": "rec-def",
            "Name": "example.com",
            "Type": "A",
            "Content": "1.1.1.1",
            "TTL": 300,
            "Weight": -1,
            "Priority": 0,
            "Status": "enable"
        });
        let r = parse_record(&v, "example.com").unwrap();
        assert_eq!(r.weight, None);
        assert_eq!(r.priority, None);
    }

    #[test]
    fn create_payload_includes_weight_conditionally() {
        let zone = || ProviderZone {
            name: "example.com".into(),
            provider_domain_id: "zone-1".into(),
        };
        let input = |rtype: &str, weight: Option<i64>, priority: Option<i64>| RecordInput {
            host: "@".into(),
            rtype: rtype.into(),
            value: "1.1.1.1".into(),
            ttl: 300,
            line: None,
            proxied: None,
            remark: None,
            weight,
            priority,
        };

        // A 记录带权重：下发 Weight，不下发 Location / Priority
        let mut payload = json!({
            "ZoneId": "zone-1",
            "Name": "example.com",
            "Type": "A",
            "Content": "1.1.1.1",
            "TTL": 300,
        });
        let i = input("A", Some(40), Some(5));
        apply_weight_priority(
            payload.as_object_mut().unwrap(),
            &i.rtype,
            i.weight,
            i.priority,
        );
        assert!(!payload.as_object().unwrap().contains_key("Location"));
        assert_eq!(payload["Weight"], 40);
        assert!(!payload.as_object().unwrap().contains_key("Priority"));

        // TXT 记录：不下发权重
        let mut payload = json!({"Type": "TXT"});
        let i = input("TXT", Some(40), None);
        apply_weight_priority(
            payload.as_object_mut().unwrap(),
            &i.rtype,
            i.weight,
            i.priority,
        );
        assert!(!payload.as_object().unwrap().contains_key("Weight"));

        // MX 记录：下发 Priority，不下发 Weight
        let mut payload = json!({"Type": "MX"});
        let i = input("MX", None, Some(10));
        apply_weight_priority(
            payload.as_object_mut().unwrap(),
            &i.rtype,
            i.weight,
            i.priority,
        );
        assert_eq!(payload["Priority"], 10);
        assert!(!payload.as_object().unwrap().contains_key("Weight"));

        // 留空（None）表示使用平台默认，不下发
        let mut payload = json!({"Type": "A"});
        let i = input("A", None, None);
        apply_weight_priority(
            payload.as_object_mut().unwrap(),
            &i.rtype,
            i.weight,
            i.priority,
        );
        assert!(!payload.as_object().unwrap().contains_key("Weight"));
        assert!(!payload.as_object().unwrap().contains_key("Priority"));

        // 显式清空：下发平台默认值 -1 / 0
        let mut payload = json!({"Type": "A"});
        let i = input("A", Some(-1), None);
        apply_weight_priority(
            payload.as_object_mut().unwrap(),
            &i.rtype,
            i.weight,
            i.priority,
        );
        assert_eq!(payload["Weight"], -1);

        // 抑制未使用告警
        let _ = zone();
    }

    #[test]
    fn error_mapping() {
        let auth = serde_json::json!({"Code": "AuthFailure.SignatureFailure", "Message": "bad"});
        assert!(matches!(map_error(&auth), ProviderError::Auth(_)));
        let unauthorized =
            serde_json::json!({"Code": "UnauthorizedOperation", "Message": "操作无权限。"});
        assert!(matches!(map_error(&unauthorized), ProviderError::Auth(_)));
        let notfound = serde_json::json!({"Code": "ResourceNotFound.DnsRecord", "Message": "gone"});
        assert!(matches!(map_error(&notfound), ProviderError::NotFound(_)));
        let limit = serde_json::json!({"Code": "RequestLimitExceeded", "Message": "slow"});
        assert!(matches!(
            map_error(&limit),
            ProviderError::RateLimited { .. }
        ));
        let other = serde_json::json!({"Code": "FailedOperation", "Message": "x"});
        assert!(matches!(map_error(&other), ProviderError::Api { .. }));
    }

    /// 通过本地假 EdgeOne API 全链路验证：提交默认值 -1/0 时，
    /// payload 照常下发，适配器返回值归一为空（line 恒空）。
    #[tokio::test]
    async fn clear_weight_priority_via_api_is_normalized() {
        use std::sync::{Arc, Mutex as StdMutex};

        let seen: Arc<StdMutex<Vec<Value>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen_for_handler = seen.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let seen = seen_for_handler.clone();
                    async move {
                        let payload: Value = serde_json::from_slice(&body).unwrap();
                        seen.lock().unwrap().push(payload);
                        let action = headers
                            .get("x-tc-action")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_lowercase();
                        let resp = match action.as_str() {
                            "creatednsrecord" => json!({"Response": {"RecordId": "rec-1"}}),
                            _ => json!({"Response": {}}),
                        };
                        axum::Json(resp)
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let provider = EdgeoneProvider::new(
            EdgeoneCredentials {
                secret_id: "id".into(),
                secret_key: "key".into(),
            },
            TeoSite::China,
        )
        .with_endpoint(format!("http://{addr}"));
        let zone = ProviderZone {
            name: "example.com".into(),
            provider_domain_id: "zone-1".into(),
        };

        // 创建 A 记录：清空权重（-1）时下发 -1，返回值归一为空，线路为空
        let input = RecordInput {
            host: "www".into(),
            rtype: "A".into(),
            value: "1.1.1.1".into(),
            ttl: 300,
            line: None,
            proxied: None,
            remark: None,
            weight: Some(-1),
            priority: None,
        };
        let created = provider.create_record(&zone, &input).await.unwrap();
        assert_eq!(created.weight, None);
        assert_eq!(created.priority, None);
        assert_eq!(created.line, None);

        // 创建 MX 记录：清空优先级（0）时下发 0，返回值归一为空
        let input = RecordInput {
            rtype: "MX".into(),
            value: "mx1.example.com".into(),
            weight: None,
            priority: Some(0),
            ..input
        };
        let created = provider.create_record(&zone, &input).await.unwrap();
        assert_eq!(created.weight, None);
        assert_eq!(created.priority, None);

        // 修改记录：返回值同样归一
        let record = ProviderRecord {
            provider_record_id: "rec-1".into(),
            host: "www".into(),
            rtype: "A".into(),
            value: "1.1.1.1".into(),
            ttl: 300,
            line: Some("default".into()),
            proxied: None,
            enabled: true,
            remark: None,
            weight: Some(-1),
            priority: None,
        };
        let updated = provider.update_record(&zone, &record).await.unwrap();
        assert_eq!(updated.weight, None);
        assert_eq!(updated.line, None);

        // 断言下发的 payload：不含 Location，含 Weight=-1 / Priority=0
        let payloads = seen.lock().unwrap();
        let create_a = payloads
            .iter()
            .find(|p| p.get("DnsRecords").is_none() && p["Type"] == "A")
            .unwrap();
        assert!(!create_a.as_object().unwrap().contains_key("Location"));
        assert_eq!(create_a["Weight"], -1);
        assert!(!create_a.as_object().unwrap().contains_key("Priority"));
        let create_mx = payloads
            .iter()
            .find(|p| p.get("DnsRecords").is_none() && p["Type"] == "MX")
            .unwrap();
        assert_eq!(create_mx["Priority"], 0);
        assert!(!create_mx.as_object().unwrap().contains_key("Weight"));
        let modify = payloads
            .iter()
            .find(|p| p.get("DnsRecords").is_some())
            .unwrap();
        let item = &modify["DnsRecords"][0];
        assert!(!item.as_object().unwrap().contains_key("Location"));
        assert_eq!(item["Weight"], -1);
    }
}
