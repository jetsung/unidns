use std::{fs, path::Path};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use rand::RngCore;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("凭据解密失败（密文损坏或主密钥不匹配）")]
    Decrypt,
    #[error("凭据加密失败")]
    Encrypt,
    #[error("口令哈希操作失败: {0}")]
    Password(String),
}

/// AES-256-GCM 加密：输出 = 12 字节随机 nonce + 密文。
pub fn encrypt(master_key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    use aes_gcm::aead::AeadCore;
    let cipher = Aes256Gcm::new(master_key.into());
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| CryptoError::Encrypt)?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// AES-256-GCM 解密：输入 = nonce + 密文。
pub fn decrypt(master_key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if data.len() < 12 {
        return Err(CryptoError::Decrypt);
    }
    let (nonce_bytes, ciphertext) = data.split_at(12);
    let cipher = Aes256Gcm::new(master_key.into());
    cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
        .map_err(|_| CryptoError::Decrypt)
}

fn decode_hex32(s: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(s.trim()).map_err(|e| format!("主密钥不是合法 hex: {e}"))?;
    bytes
        .try_into()
        .map_err(|_| "主密钥必须是 64 位 hex（32 字节）".to_string())
}

/// 主密钥来源：环境变量 hex 优先；否则读取/生成 `<data_dir>/secret.key`（0600）。
pub fn load_or_create_master_key(
    data_dir: &Path,
    env_hex: Option<&str>,
) -> Result<[u8; 32], String> {
    if let Some(hex_str) = env_hex {
        return decode_hex32(hex_str);
    }
    let path = data_dir.join("secret.key");
    if path.exists() {
        let raw =
            fs::read_to_string(&path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
        return decode_hex32(&raw);
    }
    let mut key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    fs::write(&path, hex::encode(key)).map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置 {} 权限失败: {e}", path.display()))?;
    }
    tracing::info!(path = %path.display(), "已生成凭据加密主密钥（请随数据目录一起备份）");
    Ok(key)
}

// ---------- 管理员口令（argon2id） ----------

const PASSWORD_HASH_KEY: &str = "admin_password_hash";

/// 当前是否已设置管理员口令（用于首次启动引导）。
/// 直接读取数据库：SQLite 本地读取代价极低，且避免多库场景下的缓存串扰。
pub async fn has_admin_password(pool: &sqlx::SqlitePool) -> bool {
    crate::db::get_setting(pool, PASSWORD_HASH_KEY)
        .await
        .ok()
        .flatten()
        .is_some()
}

/// 设置/覆盖管理员口令（覆盖时要求验证旧口令）。
pub async fn set_admin_password(
    pool: &sqlx::SqlitePool,
    old: Option<&str>,
    new: &str,
) -> Result<(), String> {
    let existing = crate::db::get_setting(pool, PASSWORD_HASH_KEY)
        .await
        .map_err(|e| format!("读取口令失败: {e}"))?;
    if let Some(current_hash) = existing.as_deref() {
        let provided = old.unwrap_or_default();
        if !verify_password(provided, current_hash) {
            return Err("旧口令不正确".to_string());
        }
    }
    let new_hash = hash_password(new)?;
    crate::db::set_setting(pool, PASSWORD_HASH_KEY, &new_hash)
        .await
        .map_err(|e| format!("保存口令失败: {e}"))?;
    Ok(())
}

/// 校验管理员口令。
pub async fn verify_admin_password(pool: &sqlx::SqlitePool, password: &str) -> bool {
    match crate::db::get_setting(pool, PASSWORD_HASH_KEY).await {
        Ok(Some(hash)) => verify_password(password, &hash),
        _ => false,
    }
}

pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| CryptoError::Password(e.to_string()).to_string())
        .map(|h| h.to_string())
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_roundtrip_and_tamper() {
        let key = [7u8; 32];
        let plaintext = br#"{"api_token":"secret-token-123"}"#;
        let enc = encrypt(&key, plaintext).expect("加密失败");
        assert_ne!(&enc[..], plaintext);
        let dec = decrypt(&key, &enc).expect("解密失败");
        assert_eq!(dec, plaintext);

        // 篡改密文应报错
        let mut tampered = enc.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        assert!(decrypt(&key, &tampered).is_err());

        // 错误密钥应报错
        let wrong = [8u8; 32];
        assert!(decrypt(&wrong, &enc).is_err());

        // 过短输入应报错
        assert!(decrypt(&key, &[0u8; 5]).is_err());
    }

    #[test]
    fn master_key_env_and_file_roundtrip() {
        let key = [1u8; 32];
        let env_hex = hex::encode(key);
        let from_env = load_or_create_master_key(Path::new("/nonexistent"), Some(&env_hex));
        assert_eq!(from_env.unwrap(), key);

        let dir = std::env::temp_dir().join(format!("unidns-key-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let k1 = load_or_create_master_key(&dir, None).unwrap();
        let k2 = load_or_create_master_key(&dir, None).unwrap();
        assert_eq!(k1, k2, "第二次应读取已生成的密钥");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("secret.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // 两种来源的密钥都能完成加解密往返
        let ct = encrypt(&k1, b"abc").unwrap();
        assert_eq!(decrypt(&k1, &ct).unwrap(), b"abc");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn admin_password_lifecycle() {
        let pool = crate::db::init_pool("sqlite::memory:").await.unwrap();
        assert!(!has_admin_password(&pool).await);

        // 首次设置无需旧口令
        set_admin_password(&pool, None, "first-pass").await.unwrap();
        assert!(has_admin_password(&pool).await);
        assert!(verify_admin_password(&pool, "first-pass").await);
        assert!(!verify_admin_password(&pool, "wrong").await);

        // 覆盖需要正确的旧口令
        assert!(
            set_admin_password(&pool, Some("bad"), "new-pass")
                .await
                .is_err()
        );
        set_admin_password(&pool, Some("first-pass"), "new-pass")
            .await
            .unwrap();
        assert!(verify_admin_password(&pool, "new-pass").await);
        assert!(!verify_admin_password(&pool, "first-pass").await);
    }
}
