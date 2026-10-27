//! 临时诊断：用解密后的阿里云凭据调 DescribeDomains，查看域名的 DnsServers 真实字段。
//! 用法：CRED_ENC_HEX=<hex> cargo run --example diag_aliyun

use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::env;

fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[tokio::main]
async fn main() {
    let cred_hex = env::var("CRED_ENC_HEX").expect("CRED_ENC_HEX");
    let master_key_hex = std::fs::read_to_string("data/secret.key").expect("data/secret.key");
    let mut master_key = [0u8; 32];
    hex::decode_to_slice(master_key_hex.trim(), &mut master_key).unwrap();
    let cred: serde_json::Value = serde_json::from_slice(
        &unidns::crypto::decrypt(&master_key, &hex::decode(cred_hex.trim()).unwrap()).unwrap(),
    )
    .unwrap();
    let ak_id = cred["access_key_id"].as_str().unwrap();
    let ak_secret = cred["access_key_secret"].as_str().unwrap();

    let nonce = hex::encode({
        use rand::RngCore;
        let mut b = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut b);
        b
    });
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut params = std::collections::BTreeMap::new();
    params.insert("Action", "DescribeDomainRecords");
    params.insert("Format", "JSON");
    params.insert("Version", "2015-01-09");
    params.insert("AccessKeyId", ak_id);
    params.insert("SignatureMethod", "HMAC-SHA1");
    params.insert("SignatureVersion", "1.0");
    params.insert("SignatureNonce", &nonce);
    params.insert("Timestamp", &timestamp);
    let domain = env::var("DOMAIN").unwrap_or_default();
    params.insert("DomainName", &domain);
    params.insert("PageSize", "5");
    params.insert("PageNumber", "1");
    let canonical = params
        .iter()
        .map(|(k, v)| format!("{}={}", pct(k), pct(v)))
        .collect::<Vec<_>>()
        .join("&");
    let sts = format!("GET&%2F&{}", pct(&canonical));
    let mut mac = <Hmac<Sha1>>::new_from_slice(format!("{ak_secret}&").as_bytes()).unwrap();
    mac.update(sts.as_bytes());
    let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", pct(k), pct(v)))
        .collect::<Vec<_>>()
        .join("&");
    let resp = reqwest::get(format!(
        "https://alidns.aliyuncs.com/?{query}&Signature={}",
        pct(&sig)
    ))
    .await
    .unwrap();
    println!("HTTP {}", resp.status());
    let text = resp.text().await.unwrap();
    println!("{text}");
}
