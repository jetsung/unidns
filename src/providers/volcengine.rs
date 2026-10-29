use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::aliyun::percent_encode;
use super::{
    DnsProvider, ProviderDomain, ProviderError, ProviderRecord, ProviderZone, RecordInput,
};

const DEFAULT_ENDPOINT: &str = "https://dns.volcengineapi.com";
const API_VERSION: &str = "2018-08-01";
const PAGE_SIZE: i64 = 500;
const REGION: &str = "Global"; // 官方服务地址表：云解析 DNS 为 Global 服务（RegionID=Global）
const SERVICE: &str = "dns";

#[derive(serde::Deserialize, Clone)]
pub struct VolcengineCredentials {
    pub access_key_id: String,
    pub access_key_secret: String,
}

pub struct VolcengineProvider {
    client: reqwest::Client,
    credentials: VolcengineCredentials,
    endpoint: String,
}

impl VolcengineProvider {
    pub fn new(credentials: VolcengineCredentials) -> Self {
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

    /// endpoint 的 host[:port]，用于签名 CanonicalHeaders 的 host 头。
    fn host(&self) -> String {
        self.endpoint
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string()
    }

    /// GET RPC 调用：Action/Version 与业务参数全部进 query。
    async fn call_get(
        &self,
        action: &str,
        params: BTreeMap<String, String>,
    ) -> Result<Value, ProviderError> {
        let mut query = BTreeMap::new();
        query.insert("Action".to_string(), action.to_string());
        query.insert("Version".to_string(), API_VERSION.to_string());
        query.extend(params);
        let canonical_query = canonical_query_string(&query);
        let signed = sign_v4(
            &self.credentials.access_key_id,
            &self.credentials.access_key_secret,
            "GET",
            &self.host(),
            &canonical_query,
            None,
            &x_date_now(),
        );
        let url = format!("{}/?{}", self.endpoint, canonical_query);
        let resp = self
            .client
            .get(url)
            .header("X-Date", &signed.x_date)
            .header("Authorization", &signed.authorization)
            .send()
            .await
            .map_err(ProviderError::network)?;
        parse_response(resp).await
    }

    /// POST RPC 调用：query 仅含 Action/Version，业务参数放 JSON body。
    async fn call_post(&self, action: &str, body: Value) -> Result<Value, ProviderError> {
        let mut query = BTreeMap::new();
        query.insert("Action".to_string(), action.to_string());
        query.insert("Version".to_string(), API_VERSION.to_string());
        let canonical_query = canonical_query_string(&query);
        let body_str = serde_json::to_string(&body).map_err(|e| ProviderError::Api {
            code: "BadRequest".into(),
            message: format!("请求体序列化失败: {e}"),
        })?;
        let signed = sign_v4(
            &self.credentials.access_key_id,
            &self.credentials.access_key_secret,
            "POST",
            &self.host(),
            &canonical_query,
            Some(body_str.as_bytes()),
            &x_date_now(),
        );
        let url = format!("{}/?{}", self.endpoint, canonical_query);
        let mut req = self
            .client
            .post(url)
            .header("X-Date", &signed.x_date)
            .header("Authorization", &signed.authorization)
            .header(
                "X-Content-Sha256",
                signed.x_content_sha256.as_deref().unwrap_or(""),
            )
            .header("Content-Type", "application/json");
        req = req.body(body_str);
        let resp = req.send().await.map_err(ProviderError::network)?;
        parse_response(resp).await
    }

    /// ListZones 精确查找域名，返回 (ZID, ZoneName)。
    async fn find_zone(&self, domain_name: &str) -> Result<(i64, String), ProviderError> {
        let mut params = BTreeMap::new();
        params.insert("Key".into(), domain_name.to_string());
        params.insert("SearchMode".into(), "exact".into());
        params.insert("PageSize".into(), PAGE_SIZE.to_string());
        params.insert("PageNumber".into(), "1".into());
        let body = self.call_get("ListZones", params).await?;
        let zones = body
            .pointer("/Result/Zones")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        for z in zones {
            let name = z.get("ZoneName").and_then(|v| v.as_str()).unwrap_or("");
            if name.eq_ignore_ascii_case(domain_name) {
                let zid =
                    z.get("ZID")
                        .and_then(|v| v.as_i64())
                        .ok_or_else(|| ProviderError::Api {
                            code: "BadResponse".into(),
                            message: "火山引擎 ListZones 未返回 ZID".into(),
                        })?;
                return Ok((zid, name.to_string()));
            }
        }
        Err(ProviderError::NotFound(format!(
            "火山引擎未找到域名 {domain_name}"
        )))
    }
}

// ---------- V4 签名（HMAC-SHA256） ----------

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// 当前 UTC 时间，格式 YYYYMMDDTHHMMSSZ。
fn x_date_now() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// 规范化 query：按 key ASCII 升序，RFC3986 编码后 k=v 用 & 连接。
fn canonical_query_string(params: &BTreeMap<String, String>) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// CanonicalRequest = METHOD\n/\n<query>\n<headers>\n\n<signed headers>\n<payload hash>。
fn canonical_request(
    method: &str,
    canonical_query: &str,
    canonical_headers: &str,
    signed_headers: &str,
    payload_hash: &str,
) -> String {
    format!("{method}\n/\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}")
}

/// StringToSign = HMAC-SHA256\n<X-Date>\n<scope>\n<Hex(SHA256(CanonicalRequest))>。
fn string_to_sign(x_date: &str, credential_scope: &str, canonical_req: &str) -> String {
    format!(
        "HMAC-SHA256\n{x_date}\n{credential_scope}\n{}",
        sha256_hex(canonical_req.as_bytes())
    )
}

/// 派生签名密钥：首步以 SK 字符串为密钥，其后各步以上一步的**原始字节**为密钥
/// （与官方签名示例 Demo 一致；官方文档正文"Hex 作密钥"的说法与其自身示例数值矛盾）。
fn derive_signing_key(sk: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    fn hmac_bytes(key: &[u8], msg: &str) -> Vec<u8> {
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC-SHA256 接受任意长度密钥");
        mac.update(msg.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }
    let k_date = hmac_bytes(sk.as_bytes(), date);
    let k_region = hmac_bytes(&k_date, region);
    let k_service = hmac_bytes(&k_region, service);
    hmac_bytes(&k_service, "request")
}

struct SignedRequest {
    x_date: String,
    authorization: String,
    /// POST 请求的 X-Content-Sha256（body SHA256 hex）；GET 为 None。
    x_content_sha256: Option<String>,
}

/// 火山引擎 V4 签名：GET 空 payload（None），POST 携带 body（Some）。
fn sign_v4(
    ak: &str,
    sk: &str,
    method: &str,
    host: &str,
    canonical_query: &str,
    payload: Option<&[u8]>,
    x_date: &str,
) -> SignedRequest {
    let payload_hash = match payload {
        Some(b) => sha256_hex(b),
        None => sha256_hex(b""),
    };
    let (canonical_headers, signed_headers) = match payload {
        Some(_) => (
            format!("host:{host}\nx-content-sha256:{payload_hash}\nx-date:{x_date}\n"),
            "host;x-content-sha256;x-date",
        ),
        None => (format!("host:{host}\nx-date:{x_date}\n"), "host;x-date"),
    };
    let canonical_req = canonical_request(
        method,
        canonical_query,
        &canonical_headers,
        signed_headers,
        &payload_hash,
    );
    let date = &x_date[..8];
    let credential_scope = format!("{date}/{REGION}/{SERVICE}/request");
    let sts = string_to_sign(x_date, &credential_scope, &canonical_req);
    let key = derive_signing_key(sk, date, REGION, SERVICE);
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).expect("HMAC-SHA256 接受任意长度密钥");
    mac.update(sts.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());
    SignedRequest {
        x_date: x_date.to_string(),
        authorization: format!(
            "HMAC-SHA256 Credential={ak}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}"
        ),
        x_content_sha256: payload.map(|_| payload_hash),
    }
}

/// 解析响应：火山引擎错误在 ResponseMetadata.Error 中。
async fn parse_response(resp: reqwest::Response) -> Result<Value, ProviderError> {
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| ProviderError::Api {
        code: "BadResponse".into(),
        message: format!("响应不是合法 JSON: {e}"),
    })?;
    if let Some(err) = body.pointer("/ResponseMetadata/Error") {
        return Err(map_error(err));
    }
    if !status.is_success() {
        return Err(ProviderError::Api {
            code: format!("HTTP{status}"),
            message: "火山引擎返回非成功状态且无错误详情".into(),
        });
    }
    Ok(body)
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
        .unwrap_or("火山引擎返回未知错误")
        .to_string();
    match code.as_str() {
        "InvalidAccessKey"
        | "SignatureDoesNotMatch"
        | "MissingAuthenticationToken"
        | "InvalidCredentials"
        | "ServiceNotAuthorized"
        | "Unauthorized" => ProviderError::Auth(format!("火山引擎凭据无效: {message}")),
        "RequestLimitExceeded" | "Throttling" | "FlowLimitExceeded" => ProviderError::RateLimited {
            retry_after: None,
            message: format!("火山引擎限流: {message}"),
        },
        "ErrZoneNotFound" | "ErrRecordNotFound" => ProviderError::NotFound(message),
        _ => ProviderError::Api { code, message },
    }
}

/// Stage → 统一域名状态：0=正常(enable)、1=待找回(pending)、其余原值透传。
fn stage_to_status(stage: i64) -> String {
    match stage {
        0 => "enable".into(),
        1 => "pending".into(),
        other => other.to_string(),
    }
}

fn parse_domain(v: &Value) -> Option<ProviderDomain> {
    // ListZones 实际响应不含 Stage（设计阶段的文档口径），状态字段为 CacheStage；
    // 两者都缺时按"已托管即正常"兜底为 0（与 CloudFlare/阿里云行为一致）。
    let stage = v
        .get("Stage")
        .and_then(|s| s.as_i64())
        .or_else(|| v.get("CacheStage").and_then(|s| s.as_i64()))
        .unwrap_or(0);
    Some(ProviderDomain {
        name: v.get("ZoneName")?.as_str()?.to_string(),
        provider_domain_id: v.get("ZID")?.as_i64()?.to_string(),
        status: stage_to_status(stage),
        remark: v
            .get("Remark")
            .and_then(|r| r.as_str())
            .filter(|r| !r.is_empty())
            .map(String::from),
    })
}

fn parse_record(v: &Value) -> Option<ProviderRecord> {
    Some(ProviderRecord {
        provider_record_id: v.get("RecordID")?.as_str()?.to_string(),
        host: v.get("Host")?.as_str()?.to_string(),
        rtype: v.get("Type")?.as_str()?.to_string(),
        value: v.get("Value")?.as_str()?.to_string(),
        ttl: v.get("TTL").and_then(|t| t.as_i64()).unwrap_or(600) as u64,
        line: v.get("Line").and_then(|l| l.as_str()).map(String::from),
        proxied: None,
        enabled: v.get("Enable").and_then(|e| e.as_bool()).unwrap_or(true),
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
impl DnsProvider for VolcengineProvider {
    async fn verify(&self) -> Result<(), ProviderError> {
        let mut params = BTreeMap::new();
        params.insert("PageSize".into(), "1".into());
        self.call_get("ListZones", params).await?;
        Ok(())
    }

    async fn list_domains(&self) -> Result<Vec<ProviderDomain>, ProviderError> {
        let mut domains = Vec::new();
        let mut page = 1i64;
        loop {
            let mut params = BTreeMap::new();
            params.insert("PageSize".into(), PAGE_SIZE.to_string());
            params.insert("PageNumber".into(), page.to_string());
            let body = self.call_get("ListZones", params).await?;
            let batch: Vec<ProviderDomain> = body
                .pointer("/Result/Zones")
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
            page += 1;
        }
        Ok(domains)
    }

    async fn list_records(
        &self,
        zone: &ProviderZone,
    ) -> Result<Vec<ProviderRecord>, ProviderError> {
        let zid: i64 = zone
            .provider_domain_id
            .parse()
            .map_err(|_| ProviderError::Api {
                code: "BadZoneId".into(),
                message: format!("火山引擎域名标识非法: {}", zone.provider_domain_id),
            })?;
        let mut records = Vec::new();
        let mut page = 1i64;
        loop {
            let mut params = BTreeMap::new();
            params.insert("ZID".into(), zid.to_string());
            params.insert("PageSize".into(), PAGE_SIZE.to_string());
            params.insert("PageNumber".into(), page.to_string());
            let body = self.call_get("ListRecords", params).await?;
            let batch: Vec<ProviderRecord> = body
                .pointer("/Result/Records")
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
        let zid: i64 = zone
            .provider_domain_id
            .parse()
            .map_err(|_| ProviderError::Api {
                code: "BadZoneId".into(),
                message: format!("火山引擎域名标识非法: {}", zone.provider_domain_id),
            })?;
        let line = input
            .line
            .clone()
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "default".into());
        let mut body = serde_json::json!({
            "ZID": zid,
            "Host": input.host,
            "Type": input.rtype,
            "Value": input.value,
            "Line": line,
            "TTL": input.ttl,
        });
        if let Some(r) = input.remark.as_deref().filter(|r| !r.is_empty()) {
            body["Remark"] = Value::String(r.to_string());
        }
        let resp = self.call_post("CreateRecord", body).await?;
        let result = resp
            .get("Result")
            .cloned()
            .ok_or_else(|| ProviderError::Api {
                code: "BadResponse".into(),
                message: "火山引擎 CreateRecord 未返回 Result".into(),
            })?;
        Ok(ProviderRecord {
            provider_record_id: result
                .get("RecordID")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            host: result
                .get("Host")
                .and_then(|v| v.as_str())
                .unwrap_or(&input.host)
                .to_string(),
            rtype: result
                .get("Type")
                .and_then(|v| v.as_str())
                .unwrap_or(&input.rtype)
                .to_string(),
            value: result
                .get("Value")
                .and_then(|v| v.as_str())
                .unwrap_or(&input.value)
                .to_string(),
            ttl: result
                .get("TTL")
                .and_then(|t| t.as_i64())
                .map(|t| t as u64)
                .unwrap_or(input.ttl),
            line: result
                .get("Line")
                .and_then(|l| l.as_str())
                .map(String::from)
                .or(Some(line)),
            proxied: None,
            enabled: result
                .get("Enable")
                .and_then(|e| e.as_bool())
                .unwrap_or(true),
            remark: result
                .get("Remark")
                .and_then(|r| r.as_str())
                .filter(|r| !r.is_empty())
                .map(String::from),
            weight: None,
            priority: None,
        })
    }

    async fn update_record(
        &self,
        _zone: &ProviderZone,
        record: &ProviderRecord,
    ) -> Result<ProviderRecord, ProviderError> {
        let mut body = serde_json::json!({
            "RecordID": record.provider_record_id,
            "Host": record.host,
            "Line": record.line.clone().unwrap_or_else(|| "default".into()),
            "Type": record.rtype,
            "Value": record.value,
            "TTL": record.ttl,
        });
        if let Some(r) = record.remark.as_deref().filter(|r| !r.is_empty()) {
            body["Remark"] = Value::String(r.to_string());
        }
        self.call_post("UpdateRecord", body).await?;
        Ok(record.clone())
    }

    async fn delete_record(
        &self,
        _zone: &ProviderZone,
        provider_record_id: &str,
    ) -> Result<(), ProviderError> {
        self.call_post(
            "DeleteRecord",
            serde_json::json!({ "RecordID": provider_record_id }),
        )
        .await?;
        Ok(())
    }

    async fn set_record_status(
        &self,
        _zone: &ProviderZone,
        provider_record_id: &str,
        enabled: bool,
    ) -> Result<(), ProviderError> {
        self.call_post(
            "UpdateRecordStatus",
            serde_json::json!({ "RecordID": provider_record_id, "Enable": enabled }),
        )
        .await?;
        Ok(())
    }

    /// UpdateRecord 要求 Host/Line 必填：先 QueryRecord 取当前值，再仅改 Remark。
    async fn set_record_remark(
        &self,
        _zone: &ProviderZone,
        provider_record_id: &str,
        remark: &str,
    ) -> Result<(), ProviderError> {
        let mut params = BTreeMap::new();
        params.insert("RecordID".into(), provider_record_id.to_string());
        let body = self.call_get("QueryRecord", params).await?;
        let rec = body
            .pointer("/Result")
            .cloned()
            .ok_or_else(|| ProviderError::Api {
                code: "BadResponse".into(),
                message: "火山引擎 QueryRecord 未返回 Result".into(),
            })?;
        let host = rec
            .get("Host")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let line = rec
            .get("Line")
            .and_then(|v| v.as_str())
            .filter(|l| !l.is_empty())
            .unwrap_or("default")
            .to_string();
        let mut update = serde_json::json!({
            "RecordID": provider_record_id,
            "Host": host,
            "Line": line,
        });
        if !remark.is_empty() {
            update["Remark"] = Value::String(remark.to_string());
        }
        self.call_post("UpdateRecord", update).await?;
        Ok(())
    }

    /// 先 ListZones(Key=域名, SearchMode=exact) 定位 ZID，再 UpdateZone 回写域名备注。
    async fn set_domain_remark(
        &self,
        domain_name: &str,
        remark: &str,
    ) -> Result<(), ProviderError> {
        let (zid, _) = self.find_zone(domain_name).await?;
        let mut body = serde_json::json!({ "ZID": zid });
        if !remark.is_empty() {
            body["Remark"] = Value::String(remark.to_string());
        }
        self.call_post("UpdateZone", body).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AK: &str = "AKLTtest";
    const SK: &str = "testsecret";
    const X_DATE: &str = "20260101T000000Z";
    const SCOPE: &str = "20260101/Global/dns/request";

    /// GET 基线由独立实现（Python，HMAC-SHA256）生成。
    #[test]
    fn v4_signature_baseline_get() {
        let params: BTreeMap<String, String> = [
            ("Action", "ListZones"),
            ("PageNumber", "1"),
            ("PageSize", "1"),
            ("Version", "2018-08-01"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let cq = canonical_query_string(&params);
        assert_eq!(
            cq,
            "Action=ListZones&PageNumber=1&PageSize=1&Version=2018-08-01"
        );

        let cr = canonical_request(
            "GET",
            &cq,
            "host:dns.volcengineapi.com\nx-date:20260101T000000Z\n",
            "host;x-date",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        let sts = string_to_sign(X_DATE, SCOPE, &cr);
        assert_eq!(
            sts,
            "HMAC-SHA256\n20260101T000000Z\n20260101/Global/dns/request\n\
             a89bc59ec2519e66463e364f6b685170a27b9a6e30810b0edde8503a1e6943bc"
        );

        let signed = sign_v4(AK, SK, "GET", "dns.volcengineapi.com", &cq, None, X_DATE);
        assert_eq!(signed.x_date, X_DATE);
        assert_eq!(signed.x_content_sha256, None);
        assert_eq!(
            signed.authorization,
            "HMAC-SHA256 Credential=AKLTtest/20260101/Global/dns/request, \
             SignedHeaders=host;x-date, \
             Signature=15ed7691f887b554159cf689ea6aa7f83bc14ef933a32b9c1cc7b213ff55b7f9"
        );
    }

    /// POST 基线（含 body 与 X-Content-Sha256）由独立实现（Python）生成。
    #[test]
    fn v4_signature_baseline_post() {
        let params: BTreeMap<String, String> =
            [("Action", "CreateRecord"), ("Version", "2018-08-01")]
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect();
        let cq = canonical_query_string(&params);
        assert_eq!(cq, "Action=CreateRecord&Version=2018-08-01");

        let body = r#"{"Host":"www","Type":"A","Value":"1.2.3.4","ZID":100}"#;
        let signed = sign_v4(
            AK,
            SK,
            "POST",
            "dns.volcengineapi.com",
            &cq,
            Some(body.as_bytes()),
            X_DATE,
        );
        assert_eq!(
            signed.x_content_sha256.as_deref(),
            Some("378f6dbf0be1510f654123f596822595b749d02279ae02f0575422c109377f5a")
        );
        assert_eq!(
            signed.authorization,
            "HMAC-SHA256 Credential=AKLTtest/20260101/Global/dns/request, \
             SignedHeaders=host;x-content-sha256;x-date, \
             Signature=454a06d281b31d1e7126f60a0dd2b4691e4e4ddd58afe111ce3d18118d586fa8"
        );
    }

    #[test]
    fn signing_key_derivation_matches_baseline() {
        // 派生链正确性：与官方签名示例数值规则一致（每步以上一步原始字节为密钥），
        // 并与官方 GET 请求示例向量（docs.volcengine.com 签名方法）对照验证过。
        let key = derive_signing_key(SK, "20260101", REGION, SERVICE);
        assert_eq!(
            hex::encode(&key),
            "124dc997e0be53ee22b73bef36b04d6457df33254adfa4fda93e7b2dbfdc9d66"
        );
        let mut mac = Hmac::<Sha256>::new_from_slice(&key).unwrap();
        mac.update(
            b"HMAC-SHA256\n20260101T000000Z\n20260101/Global/dns/request\n\
              a89bc59ec2519e66463e364f6b685170a27b9a6e30810b0edde8503a1e6943bc",
        );
        assert_eq!(
            hex::encode(mac.finalize().into_bytes()),
            "15ed7691f887b554159cf689ea6aa7f83bc14ef933a32b9c1cc7b213ff55b7f9"
        );
    }

    #[test]
    fn error_mapping() {
        let auth = serde_json::json!({"Code": "SignatureDoesNotMatch", "Message": "bad sig"});
        assert!(matches!(map_error(&auth), ProviderError::Auth(_)));
        let throttle = serde_json::json!({"Code": "RequestLimitExceeded", "Message": "slow"});
        assert!(matches!(
            map_error(&throttle),
            ProviderError::RateLimited { .. }
        ));
        let notfound = serde_json::json!({"Code": "ErrRecordNotFound", "Message": "gone"});
        assert!(matches!(map_error(&notfound), ProviderError::NotFound(_)));
        let other = serde_json::json!({"Code": "ErrSpecNotValid", "Message": "boom"});
        assert!(matches!(map_error(&other), ProviderError::Api { .. }));
    }

    #[test]
    fn stage_mapping() {
        assert_eq!(stage_to_status(0), "enable");
        assert_eq!(stage_to_status(1), "pending");
        assert_eq!(stage_to_status(2), "2");
        assert_eq!(stage_to_status(3), "3");
        assert_eq!(stage_to_status(5), "5");
    }

    #[test]
    fn domain_parsing() {
        // 实际响应含 CacheStage（无 Stage）
        let v = serde_json::json!({
            "ZID": 12345, "ZoneName": "example.com", "CacheStage": 0, "Remark": ""
        });
        let d = parse_domain(&v).unwrap();
        assert_eq!(d.name, "example.com");
        assert_eq!(d.provider_domain_id, "12345");
        assert_eq!(d.status, "enable");
        assert_eq!(d.remark, None, "空串备注应归一为 None");

        // 设计口径的 Stage 字段若存在则优先
        let pending = parse_domain(&serde_json::json!({
            "ZID": 1, "ZoneName": "a.com", "Stage": 1, "Remark": "备注"
        }))
        .unwrap();
        assert_eq!(pending.status, "pending");
        assert_eq!(pending.remark.as_deref(), Some("备注"));

        // Stage 缺失时回退 CacheStage 透传
        let cache = parse_domain(&serde_json::json!({
            "ZID": 2, "ZoneName": "b.com", "CacheStage": 3
        }))
        .unwrap();
        assert_eq!(cache.status, "3");

        // 两者都缺：按"已托管即正常"兜底（与 CloudFlare/阿里云一致）
        let bare = parse_domain(&serde_json::json!({"ZID": 3, "ZoneName": "c.com"})).unwrap();
        assert_eq!(bare.status, "enable");
    }

    #[test]
    fn record_parsing() {
        let v = serde_json::json!({
            "RecordID": "rec-1", "Host": "www", "Type": "A", "Value": "1.2.3.4",
            "TTL": 600, "Line": "default", "Enable": true, "Remark": "测试"
        });
        let r = parse_record(&v).unwrap();
        assert_eq!(r.provider_record_id, "rec-1");
        assert_eq!(r.host, "www");
        assert_eq!(r.line.as_deref(), Some("default"));
        assert!(r.enabled);
        assert_eq!(r.remark.as_deref(), Some("测试"));
        assert_eq!(r.weight, None);
        assert_eq!(r.priority, None);
        assert_eq!(r.proxied, None);

        let disabled = parse_record(&serde_json::json!({
            "RecordID": "rec-2", "Host": "@", "Type": "AAAA", "Value": "::1",
            "TTL": 300, "Line": "unicom", "Enable": false
        }))
        .unwrap();
        assert!(!disabled.enabled);
        assert_eq!(disabled.host, "@");
        assert_eq!(disabled.remark, None);
    }

    // ---------- 请求构造单测：endpoint_override 指向本地 mock ----------

    /// 极简 HTTP mock：按序响应预置 JSON，并记录收到的请求（method + path + body）。
    struct MockServer {
        url: String,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl MockServer {
        fn start(responses: Vec<Value>) -> Self {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let reqs = requests.clone();
            std::thread::spawn(move || {
                for resp in responses {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut buf = vec![0u8; 65536];
                    let mut raw = Vec::new();
                    // 读头 + 按 Content-Length 读 body
                    loop {
                        let n = stream.read(&mut buf).unwrap();
                        if n == 0 {
                            break;
                        }
                        raw.extend_from_slice(&buf[..n]);
                        let s = String::from_utf8_lossy(&raw).to_string();
                        if let Some(pos) = s.find("\r\n\r\n") {
                            let cl = s
                                .lines()
                                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                                .and_then(|l| l.split(':').nth(1))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if raw.len() >= pos + 4 + cl {
                                break;
                            }
                        }
                    }
                    let s = String::from_utf8_lossy(&raw).to_string();
                    let mut lines = s.split("\r\n");
                    let request_line = lines.next().unwrap_or("").to_string();
                    let body = s.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                    reqs.lock().unwrap().push(format!("{request_line}\n{body}"));
                    let resp_body = serde_json::to_vec(&resp).unwrap();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        resp_body.len()
                    );
                    stream.write_all(head.as_bytes()).unwrap();
                    stream.write_all(&resp_body).unwrap();
                }
            });
            MockServer {
                url: format!("http://{addr}"),
                requests,
            }
        }

        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.requests.lock().unwrap())
        }
    }

    fn provider_at(url: &str) -> VolcengineProvider {
        VolcengineProvider::new(VolcengineCredentials {
            access_key_id: "ak-test".into(),
            access_key_secret: "sk-test".into(),
        })
        .with_endpoint(url)
    }

    fn zone() -> ProviderZone {
        ProviderZone {
            name: "example.com".into(),
            provider_domain_id: "12345".into(),
        }
    }

    #[tokio::test]
    async fn list_zones_uses_paging_params() {
        let server = MockServer::start(vec![serde_json::json!({
            "ResponseMetadata": {"RequestId": "r1"},
            "Result": {"Total": 1, "Zones": [
                {"ZID": 12345, "ZoneName": "example.com", "Stage": 0, "Remark": ""}
            ]}
        })]);
        let p = provider_at(&server.url);
        let domains = p.list_domains().await.unwrap();
        assert_eq!(domains.len(), 1);
        assert_eq!(domains[0].provider_domain_id, "12345");
        let reqs = server.take();
        assert_eq!(reqs.len(), 1);
        assert!(
            reqs[0].contains("GET /?Action=ListZones&PageNumber=1&PageSize=500&Version=2018-08-01"),
            "ListZones 翻页参数: {}",
            reqs[0]
        );
    }

    #[tokio::test]
    async fn list_records_sends_zid_and_paging() {
        let server = MockServer::start(vec![serde_json::json!({
            "ResponseMetadata": {"RequestId": "r1"},
            "Result": {"TotalCount": 1, "Records": [
                {"RecordID": "rec-1", "Host": "www", "Type": "A", "Value": "1.2.3.4",
                 "TTL": 600, "Line": "default", "Enable": true}
            ]}
        })]);
        let p = provider_at(&server.url);
        let records = p.list_records(&zone()).await.unwrap();
        assert_eq!(records.len(), 1);
        let reqs = server.take();
        assert!(
            reqs[0].contains(
                "GET /?Action=ListRecords&PageNumber=1&PageSize=500&Version=2018-08-01&ZID=12345"
            ),
            "ListRecords 参数应含 ZID 与翻页: {}",
            reqs[0]
        );
    }

    #[tokio::test]
    async fn create_record_body_fields() {
        let server = MockServer::start(vec![serde_json::json!({
            "ResponseMetadata": {"RequestId": "r1"},
            "Result": {"RecordID": "rec-new", "Host": "www", "Type": "A",
                       "Value": "1.2.3.4", "TTL": 600, "Line": "default", "Enable": true}
        })]);
        let p = provider_at(&server.url);
        let input = RecordInput {
            host: "www".into(),
            rtype: "A".into(),
            value: "1.2.3.4".into(),
            ttl: 600,
            line: None,
            proxied: None,
            remark: Some("备注".into()),
            weight: None,
            priority: None,
        };
        let created = p.create_record(&zone(), &input).await.unwrap();
        assert_eq!(created.provider_record_id, "rec-new");
        assert_eq!(created.line.as_deref(), Some("default"));
        let reqs = server.take();
        assert!(reqs[0].starts_with("POST /?Action=CreateRecord&Version=2018-08-01"));
        let body: Value = serde_json::from_str(reqs[0].split('\n').nth(1).unwrap()).unwrap();
        assert_eq!(body["ZID"], 12345);
        assert_eq!(body["Host"], "www");
        assert_eq!(body["Type"], "A");
        assert_eq!(body["Value"], "1.2.3.4");
        assert_eq!(body["Line"], "default", "线路为空时应回填 default");
        assert_eq!(body["TTL"], 600);
        assert_eq!(body["Remark"], "备注");
    }

    #[tokio::test]
    async fn update_record_backfills_required_fields() {
        let server = MockServer::start(vec![serde_json::json!({
            "ResponseMetadata": {"RequestId": "r1"}, "Result": {}
        })]);
        let p = provider_at(&server.url);
        let record = ProviderRecord {
            provider_record_id: "rec-1".into(),
            host: "www".into(),
            rtype: "A".into(),
            value: "5.6.7.8".into(),
            ttl: 300,
            line: Some("unicom".into()),
            proxied: None,
            enabled: true,
            remark: None,
            weight: None,
            priority: None,
        };
        p.update_record(&zone(), &record).await.unwrap();
        let reqs = server.take();
        assert!(reqs[0].starts_with("POST /?Action=UpdateRecord&Version=2018-08-01"));
        let body: Value = serde_json::from_str(reqs[0].split('\n').nth(1).unwrap()).unwrap();
        assert_eq!(body["RecordID"], "rec-1");
        assert_eq!(body["Host"], "www");
        assert_eq!(body["Line"], "unicom");
        assert_eq!(body["Type"], "A");
        assert_eq!(body["Value"], "5.6.7.8");
        assert_eq!(body["TTL"], 300);
        assert!(body.get("Remark").is_none());
    }

    #[tokio::test]
    async fn delete_and_status_send_record_id() {
        let server = MockServer::start(vec![
            serde_json::json!({"ResponseMetadata": {"RequestId": "r1"}, "Result": {}}),
            serde_json::json!({"ResponseMetadata": {"RequestId": "r2"}, "Result": {}}),
        ]);
        let p = provider_at(&server.url);
        p.delete_record(&zone(), "rec-1").await.unwrap();
        p.set_record_status(&zone(), "rec-1", false).await.unwrap();
        let reqs = server.take();
        let b1: Value = serde_json::from_str(reqs[0].split('\n').nth(1).unwrap()).unwrap();
        assert!(reqs[0].starts_with("POST /?Action=DeleteRecord"));
        assert_eq!(b1["RecordID"], "rec-1");
        let b2: Value = serde_json::from_str(reqs[1].split('\n').nth(1).unwrap()).unwrap();
        assert!(reqs[1].starts_with("POST /?Action=UpdateRecordStatus"));
        assert_eq!(b2["RecordID"], "rec-1");
        assert_eq!(b2["Enable"], false);
    }

    #[tokio::test]
    async fn set_record_remark_queries_then_updates() {
        let server = MockServer::start(vec![
            serde_json::json!({
                "ResponseMetadata": {"RequestId": "r1"},
                "Result": {"RecordID": "rec-1", "Host": "www", "Line": "", "Type": "A",
                           "Value": "1.2.3.4", "TTL": 600, "Enable": true}
            }),
            serde_json::json!({"ResponseMetadata": {"RequestId": "r2"}, "Result": {}}),
        ]);
        let p = provider_at(&server.url);
        p.set_record_remark(&zone(), "rec-1", "新备注")
            .await
            .unwrap();
        let reqs = server.take();
        assert!(
            reqs[0].contains("GET /?Action=QueryRecord&RecordID=rec-1&Version=2018-08-01"),
            "先 QueryRecord 取当前 Host/Line: {}",
            reqs[0]
        );
        assert!(reqs[1].starts_with("POST /?Action=UpdateRecord"));
        let body: Value = serde_json::from_str(reqs[1].split('\n').nth(1).unwrap()).unwrap();
        assert_eq!(body["RecordID"], "rec-1");
        assert_eq!(body["Host"], "www");
        assert_eq!(
            body["Line"], "default",
            "QueryRecord 线路为空时回填 default"
        );
        assert_eq!(body["Remark"], "新备注");
    }

    #[tokio::test]
    async fn set_domain_remark_locates_zone_then_updates() {
        let server = MockServer::start(vec![
            serde_json::json!({
                "ResponseMetadata": {"RequestId": "r1"},
                "Result": {"Total": 1, "Zones": [
                    {"ZID": 777, "ZoneName": "example.com", "Stage": 0}
                ]}
            }),
            serde_json::json!({"ResponseMetadata": {"RequestId": "r2"}, "Result": {}}),
        ]);
        let p = provider_at(&server.url);
        p.set_domain_remark("example.com", "域名备注")
            .await
            .unwrap();
        let reqs = server.take();
        assert!(
            reqs[0].contains("Key=example.com") && reqs[0].contains("SearchMode=exact"),
            "先精确查找域名: {}",
            reqs[0]
        );
        assert!(reqs[1].starts_with("POST /?Action=UpdateZone"));
        let body: Value = serde_json::from_str(reqs[1].split('\n').nth(1).unwrap()).unwrap();
        assert_eq!(body["ZID"], 777);
        assert_eq!(body["Remark"], "域名备注");
    }

    #[tokio::test]
    async fn api_error_is_normalized() {
        let server = MockServer::start(vec![serde_json::json!({
            "ResponseMetadata": {
                "RequestId": "r1",
                "Error": {"Code": "ErrZoneNotFound", "Message": "zone not found"}
            }
        })]);
        let p = provider_at(&server.url);
        let err = p.list_records(&zone()).await.unwrap_err();
        assert!(
            matches!(err, ProviderError::NotFound(_)),
            "实际错误: {err:?}"
        );
    }

    #[tokio::test]
    async fn auth_error_is_normalized() {
        let server = MockServer::start(vec![serde_json::json!({
            "ResponseMetadata": {
                "RequestId": "r1",
                "Error": {"Code": "SignatureDoesNotMatch", "Message": "bad signature"}
            }
        })]);
        let p = provider_at(&server.url);
        let err = p.verify().await.unwrap_err();
        assert!(matches!(err, ProviderError::Auth(_)), "实际错误: {err:?}");
    }
}
