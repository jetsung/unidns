//! 临时诊断：用解密后的 EdgeOne 凭据直接调用指定接口，打印原始完整响应。
//! 用法：CRED_ENC_HEX=<hex> ZONE_ID=<zone-xxx> cargo run --example diag_teo
//! 诊断完成后删除本文件。

use std::env;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).unwrap();
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

#[allow(clippy::too_many_arguments)]
fn tc3_authorization(
    secret_id: &str,
    secret_key: &str,
    timestamp: i64,
    host: &str,
    service: &str,
    action: &str,
    payload: &str,
) -> String {
    let date = chrono::DateTime::from_timestamp(timestamp, 0)
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    let canonical_request = format!(
        "POST\n/\n\ncontent-type:application/json; charset=utf-8\nhost:{}\nx-tc-action:{}\n\ncontent-type;host;x-tc-action\n{}",
        host,
        action.to_lowercase(),
        sha256_hex(payload.as_bytes())
    );
    let credential_scope = format!("{date}/{service}/tc3_request");
    let string_to_sign = format!(
        "TC3-HMAC-SHA256\n{timestamp}\n{credential_scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let k = hmac_sha256(format!("TC3{secret_key}").as_bytes(), date.as_bytes());
    let k = hmac_sha256(&k, service.as_bytes());
    let k = hmac_sha256(&k, b"tc3_request");
    let sig = hex::encode(hmac_sha256(&k, string_to_sign.as_bytes()));
    format!(
        "TC3-HMAC-SHA256 Credential={secret_id}/{credential_scope}, SignedHeaders=content-type;host;x-tc-action, Signature={sig}"
    )
}

#[tokio::main]
async fn main() {
    let cred_hex = env::var("CRED_ENC_HEX").expect("CRED_ENC_HEX");
    let master_key_hex = std::fs::read_to_string("data/secret.key").expect("data/secret.key");
    let mut master_key = [0u8; 32];
    hex::decode_to_slice(master_key_hex.trim(), &mut master_key).unwrap();
    let cred_enc = hex::decode(cred_hex.trim()).unwrap();
    let cred_json = unidns::crypto::decrypt(&master_key, &cred_enc).expect("解密失败");
    let cred: serde_json::Value = serde_json::from_slice(&cred_json).unwrap();
    let secret_id = cred["secret_id"].as_str().unwrap().to_string();
    let secret_key = cred["secret_key"].as_str().unwrap().to_string();
    println!(
        "凭据解密成功：secret_id = {}****",
        &secret_id[..8.min(secret_id.len())]
    );

    let host = env::var("TEO_ENDPOINT").unwrap_or_else(|_| "teo.intl.tencentcloudapi.com".into());
    let service = env::var("TEO_SERVICE").unwrap_or_else(|_| "teo".into());
    let version = env::var("TEO_VERSION").unwrap_or_else(|_| "2022-09-01".into());
    let endpoint = format!("https://{host}/");

    let action = env::var("TEO_ACTION").unwrap_or_else(|_| "DescribeZones".into());
    let payload_env = env::var("TEO_PAYLOAD").unwrap_or_else(|_| "{}".into());
    let payload: serde_json::Value = serde_json::from_str(&payload_env).expect("TEO_PAYLOAD JSON");
    let cases: Vec<(&str, serde_json::Value)> = vec![(action.as_str(), payload)];

    for (action, payload) in cases {
        let body = serde_json::to_string(&payload).unwrap();
        let timestamp = chrono::Utc::now().timestamp();
        let auth = tc3_authorization(
            &secret_id,
            &secret_key,
            timestamp,
            &host,
            &service,
            action,
            &body,
        );
        let client = reqwest::Client::new();
        let resp = client
            .post(&endpoint)
            .header("Authorization", auth)
            .header("Content-Type", "application/json; charset=utf-8")
            .header("X-TC-Action", action)
            .header("X-TC-Timestamp", timestamp.to_string())
            .header("X-TC-Version", &version)
            .body(body)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        println!("\n===== {action} (HTTP {status}) =====");
        // 美化输出
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
            Err(_) => println!("{text}"),
        }
    }
}
