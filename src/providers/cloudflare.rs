use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    DnsProvider, ProviderDomain, ProviderError, ProviderRecord, ProviderZone, RecordInput,
};

const DEFAULT_BASE: &str = "https://api.cloudflare.com/client/v4";
const ZONE_PAGE_SIZE: i64 = 50;
const RECORD_PAGE_SIZE: i64 = 100;

#[derive(serde::Deserialize, Clone)]
pub struct CloudflareCredentials {
    pub api_token: String,
}

pub struct CloudflareProvider {
    client: reqwest::Client,
    api_token: String,
    base_url: String,
}

use super::{fqdn_to_host as host_of, host_to_fqdn as fqdn_of};

fn parse_record(v: &Value, zone: &str) -> Option<ProviderRecord> {
    Some(ProviderRecord {
        provider_record_id: v.get("id")?.as_str()?.to_string(),
        host: host_of(v.get("name")?.as_str()?, zone),
        rtype: v.get("type")?.as_str()?.to_string(),
        value: v.get("content")?.as_str()?.to_string(),
        ttl: v.get("ttl").and_then(|t| t.as_i64()).unwrap_or(1) as u64,
        line: None,
        proxied: v.get("proxied").and_then(|p| p.as_bool()),
        enabled: true,
        remark: v
            .get("comment")
            .and_then(|c| c.as_str())
            .filter(|c| !c.is_empty())
            .map(String::from),
        weight: None,
        priority: None,
    })
}

/// 只保留 NS 已指向 CloudFlare 的站点：status=active。
/// pending 表示 NS 未切换（解析仍在原服务商），moved/deleted 同理排除。
fn parse_zone(v: &Value) -> Option<ProviderDomain> {
    let status = v
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    if status != "active" {
        return None;
    }
    Some(ProviderDomain {
        name: v.get("name")?.as_str()?.to_string(),
        provider_domain_id: v.get("id")?.as_str()?.to_string(),
        status,
        // CloudFlare zone 无备注概念
        remark: None,
    })
}

impl CloudflareProvider {
    pub fn new(credentials: CloudflareCredentials) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_token: credentials.api_token,
            base_url: DEFAULT_BASE.to_string(),
        }
    }

    /// 测试或代理场景下覆盖 base URL。
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(Value, Option<Value>), ProviderError> {
        let mut req = self
            .client
            .request(method, format!("{}{}", self.base_url, path))
            .bearer_auth(&self.api_token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.map_err(ProviderError::network)?;
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let parsed: Value = resp.json().await.map_err(|e| ProviderError::Api {
            code: "BadResponse".into(),
            message: format!("响应不是合法 JSON: {e}"),
        })?;
        let ok = parsed
            .get("success")
            .and_then(|s| s.as_bool())
            .unwrap_or(false);
        if !ok {
            let first = parsed
                .get("errors")
                .and_then(|e| e.as_array())
                .and_then(|a| a.first().cloned());
            let code = first
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(|c| c.to_string().trim_matches('"').to_string())
                .unwrap_or_else(|| format!("HTTP {status}"));
            let message = first
                .and_then(|e| e.get("message").and_then(|m| m.as_str()).map(String::from))
                .unwrap_or_else(|| "CloudFlare 返回未知错误".into());
            return Err(match status.as_u16() {
                401 | 403 => ProviderError::Auth(format!("CloudFlare 凭据无效: {message}")),
                429 => ProviderError::RateLimited {
                    retry_after,
                    message: format!("CloudFlare 限流: {message}"),
                },
                404 => ProviderError::NotFound(message),
                _ => ProviderError::Api { code, message },
            });
        }
        let result = parsed.get("result").cloned().unwrap_or(Value::Null);
        let info = parsed.get("result_info").cloned();
        Ok((result, info))
    }

    fn total_pages(info: &Option<Value>) -> i64 {
        info.as_ref()
            .and_then(|i| i.get("total_pages"))
            .and_then(|t| t.as_i64())
            .unwrap_or(1)
            .max(1)
    }
}

#[async_trait]
impl DnsProvider for CloudflareProvider {
    async fn verify(&self) -> Result<(), ProviderError> {
        self.request(reqwest::Method::GET, "/zones?per_page=1", None)
            .await?;
        Ok(())
    }

    async fn list_domains(&self) -> Result<Vec<ProviderDomain>, ProviderError> {
        let mut zones = Vec::new();
        let mut page = 1i64;
        loop {
            let (result, info) = self
                .request(
                    reqwest::Method::GET,
                    &format!("/zones?per_page={ZONE_PAGE_SIZE}&page={page}"),
                    None,
                )
                .await?;
            zones.extend(
                result
                    .as_array()
                    .map(|a| a.iter().filter_map(parse_zone).collect::<Vec<_>>())
                    .unwrap_or_default(),
            );
            if page >= Self::total_pages(&info) {
                break;
            }
            page += 1;
        }
        Ok(zones)
    }

    async fn list_records(
        &self,
        zone: &ProviderZone,
    ) -> Result<Vec<ProviderRecord>, ProviderError> {
        let mut records = Vec::new();
        let mut page = 1i64;
        loop {
            let (result, info) = self
                .request(
                    reqwest::Method::GET,
                    &format!(
                        "/zones/{}/dns_records?per_page={RECORD_PAGE_SIZE}&page={page}",
                        zone.provider_domain_id
                    ),
                    None,
                )
                .await?;
            records.extend(
                result
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|r| parse_record(r, &zone.name))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            );
            if page >= Self::total_pages(&info) {
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
        let mut body = json!({
            "type": input.rtype,
            "name": fqdn_of(&input.host, &zone.name),
            "content": input.value,
            "ttl": input.ttl,
            "proxied": input.proxied.unwrap_or(false),
        });
        if let Some(r) = input.remark.as_deref().filter(|r| !r.is_empty()) {
            body["comment"] = Value::String(r.to_string());
        }
        let (result, _) = self
            .request(
                reqwest::Method::POST,
                &format!("/zones/{}/dns_records", zone.provider_domain_id),
                Some(body),
            )
            .await?;
        parse_record(&result, &zone.name).ok_or_else(|| ProviderError::Api {
            code: "BadResponse".into(),
            message: "CloudFlare 未返回记录内容".into(),
        })
    }

    async fn update_record(
        &self,
        zone: &ProviderZone,
        record: &ProviderRecord,
    ) -> Result<ProviderRecord, ProviderError> {
        let mut body = json!({
            "type": record.rtype,
            "name": fqdn_of(&record.host, &zone.name),
            "content": record.value,
            "ttl": record.ttl,
            "proxied": record.proxied.unwrap_or(false),
        });
        // comment 传空串表示清除备注
        body["comment"] = Value::String(record.remark.clone().unwrap_or_default());
        let _ = record.remark;
        let (result, _) = self
            .request(
                reqwest::Method::PUT,
                &format!(
                    "/zones/{}/dns_records/{}",
                    zone.provider_domain_id, record.provider_record_id
                ),
                Some(body),
            )
            .await?;
        let _ = result; // 更新成功即返回本地视图
        Ok(record.clone())
    }

    async fn delete_record(
        &self,
        zone: &ProviderZone,
        provider_record_id: &str,
    ) -> Result<(), ProviderError> {
        self.request(
            reqwest::Method::DELETE,
            &format!(
                "/zones/{}/dns_records/{provider_record_id}",
                zone.provider_domain_id
            ),
            None,
        )
        .await?;
        Ok(())
    }

    /// CloudFlare API 不提供 DNS 记录级启停。
    async fn set_record_status(
        &self,
        _zone: &ProviderZone,
        _provider_record_id: &str,
        _enabled: bool,
    ) -> Result<(), ProviderError> {
        Err(ProviderError::Unsupported(
            "CloudFlare 平台不支持解析记录的启用/停用".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, extract::Query, routing::any};
    use std::collections::HashMap;

    async fn spawn_mock(check_auth: bool) -> (String, ()) {
        let app = Router::new().route(
            "/zones",
            any(move |headers: axum::http::HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                    if check_auth && headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer good-token") {
                        return (
                            axum::http::StatusCode::UNAUTHORIZED,
                            axum::Json(json!({
                                "success": false,
                                "errors": [{"code": 9103, "message": "Invalid access token"}]
                            })),
                        );
                    }
                    let page = q.get("page").and_then(|p| p.parse::<i64>().ok()).unwrap_or(1);
                    match page {
                        1 => (axum::http::StatusCode::OK, axum::Json(json!({
                            "success": true,
                            "result": [{"id": "zone1", "name": "example.com", "status": "active"}],
                            "result_info": {"page": 1, "total_pages": 2}
                        }))),
                        2 => (axum::http::StatusCode::OK, axum::Json(json!({
                            "success": true,
                            "result": [{"id": "zone2", "name": "example.org", "status": "active"}],
                            "result_info": {"page": 2, "total_pages": 2}
                        }))),
                        _ => (axum::http::StatusCode::OK, axum::Json(json!({"success": true, "result": [], "result_info": {"page": page, "total_pages": 2}}))),
                    }
            }),
        )
        .route(
            "/zones/{zone_id}/dns_records",
            any(move |headers: axum::http::HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer good-token") {
                        return axum::Json(json!({"success": false, "errors": [{"code": 9103, "message": "Invalid access token"}]}));
                    }
                    let page = q.get("page").and_then(|p| p.parse::<i64>().ok()).unwrap_or(1);
                    match page {
                        1 => axum::Json(json!({
                            "success": true,
                            "result": [
                                {"id": "rec1", "type": "A", "name": "www.example.com", "content": "1.2.3.4", "ttl": 600, "proxied": true},
                                {"id": "rec2", "type": "AAAA", "name": "example.com", "content": "::1", "ttl": 1, "proxied": false}
                            ],
                            "result_info": {"page": 1, "total_pages": 2}
                        })),
                        _ => axum::Json(json!({
                            "success": true,
                            "result": [
                                {"id": "rec3", "type": "CNAME", "name": "api.example.com", "content": "target.example.net", "ttl": 300, "proxied": false}
                            ],
                            "result_info": {"page": 2, "total_pages": 2}
                        })),
                    }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), ())
    }

    fn provider(base: &str) -> CloudflareProvider {
        CloudflareProvider::new(CloudflareCredentials {
            api_token: "good-token".into(),
        })
        .with_base_url(base)
    }

    #[tokio::test]
    async fn verify_and_pagination() {
        let (base, _tx) = spawn_mock(true).await;
        let p = provider(&base);

        p.verify().await.expect("连通性验证应通过");

        let zones = p.list_domains().await.expect("拉取 zones 失败");
        assert_eq!(zones.len(), 2, "应跨两页聚合出 2 个 zone");
        assert_eq!(zones[0].name, "example.com");
        assert_eq!(zones[0].provider_domain_id, "zone1");

        let records = p
            .list_records(&ProviderZone {
                name: "example.com".into(),
                provider_domain_id: "zone1".into(),
            })
            .await
            .expect("拉取记录失败");
        assert_eq!(records.len(), 3, "应跨两页聚合出 3 条记录");
        assert_eq!(records[0].host, "www");
        assert_eq!(records[0].proxied, Some(true));
        assert_eq!(records[1].host, "@", "根记录 host 应为 @");
    }

    #[tokio::test]
    async fn auth_error_normalized() {
        let (base, _tx) = spawn_mock(true).await;
        let p = CloudflareProvider::new(CloudflareCredentials {
            api_token: "bad-token".into(),
        })
        .with_base_url(&base);
        let err = p.verify().await.expect_err("坏 token 应失败");
        assert!(matches!(err, ProviderError::Auth(_)), "{err:?}");
    }

    #[test]
    fn only_active_zones_are_parsed() {
        let active = serde_json::json!({"id": "z1", "name": "a.com", "status": "active"});
        assert_eq!(parse_zone(&active).unwrap().name, "a.com");
        // pending = NS 未指向 CloudFlare，排除
        let pending = serde_json::json!({"id": "z2", "name": "b.com", "status": "pending"});
        assert!(parse_zone(&pending).is_none());
    }

    #[test]
    fn host_fqdn_conversion() {
        assert_eq!(host_of("www.example.com", "example.com"), "www");
        assert_eq!(host_of("example.com", "example.com"), "@");
        assert_eq!(host_of("a.b.example.com", "example.com"), "a.b");
        assert_eq!(fqdn_of("www", "example.com"), "www.example.com");
        assert_eq!(fqdn_of("@", "example.com"), "example.com");
    }

    #[test]
    fn unsupported_status() {
        let p = CloudflareProvider::new(CloudflareCredentials {
            api_token: "t".into(),
        });
        let zone = ProviderZone {
            name: "example.com".into(),
            provider_domain_id: "z1".into(),
        };
        let f = p.set_record_status(&zone, "r1", false);
        // 未 await 也会立即返回 Unsupported（不发起网络请求）
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(f).unwrap_err();
        assert!(matches!(err, ProviderError::Unsupported(_)));
    }
}
