use std::{env, fs, path::PathBuf};

use serde::Deserialize;

const DEFAULT_LISTEN: &str = "0.0.0.0:18686";
const DEFAULT_SYNC_INTERVAL_MINUTES: u64 = 0;

pub const EXAMPLE_CONFIG: &str = r#"# unidns 配置文件
# 环境变量（UNIDNS_LISTEN / UNIDNS_DATA_DIR / UNIDNS_SYNC_INTERVAL / UNIDNS_ADMIN_PASSWORD / UNIDNS_MASTER_KEY）
# 优先级高于本文件中的同名配置。

# HTTP 监听地址
#listen = "0.0.0.0:18686"

# 数据目录（SQLite 数据库与 secret.key 存放位置）
#data_dir = "./data"

# 定时全量同步间隔（分钟）；0 表示停用自动同步，启用时至少 30 分钟
#sync_interval_minutes = 0

# 预置管理员口令（仅首次创建口令时生效；也可用环境变量 UNIDNS_ADMIN_PASSWORD）
#admin_password = "change-me"

# 凭据加密主密钥（64 位 hex，表示 32 字节；缺省时自动生成并保存到 <data_dir>/secret.key）
#master_key = "0000000000000000000000000000000000000000000000000000000000000000"
"#;

/// 运行配置：默认值 <- 配置文件 <- 环境变量
#[derive(Debug, Clone)]
pub struct Config {
    pub config_path: PathBuf,
    pub listen: String,
    pub data_dir: PathBuf,
    pub sync_interval_minutes: u64,
    pub admin_password: Option<String>,
    pub master_key_hex: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    listen: Option<String>,
    data_dir: Option<String>,
    sync_interval_minutes: Option<u64>,
    admin_password: Option<String>,
    master_key: Option<String>,
}

/// 配置文件路径：取 `UNIDNS_CONFIG`，未设置时默认当前目录下 `config.toml`。
pub fn config_path() -> PathBuf {
    env::var("UNIDNS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config.toml"))
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let config_path = config_path();

        let file: FileConfig = if config_path.exists() {
            let raw = fs::read_to_string(&config_path)
                .map_err(|e| format!("读取配置文件 {} 失败: {e}", config_path.display()))?;
            toml::from_str(&raw)
                .map_err(|e| format!("解析配置文件 {} 失败: {e}", config_path.display()))?
        } else {
            FileConfig::default()
        };

        Ok(Config {
            listen: env_opt("UNIDNS_LISTEN")
                .or(file.listen)
                .unwrap_or(DEFAULT_LISTEN.into()),
            data_dir: PathBuf::from(
                env_opt("UNIDNS_DATA_DIR")
                    .or(file.data_dir)
                    .unwrap_or_else(|| "./data".into()),
            ),
            sync_interval_minutes: env_opt("UNIDNS_SYNC_INTERVAL")
                .and_then(|v| v.parse().ok())
                .or(file.sync_interval_minutes)
                .unwrap_or(DEFAULT_SYNC_INTERVAL_MINUTES),
            admin_password: env_opt("UNIDNS_ADMIN_PASSWORD").or(file.admin_password),
            master_key_hex: env_opt("UNIDNS_MASTER_KEY").or(file.master_key),
            config_path,
        })
    }

    /// 确保数据目录存在；配置文件不存在时生成示例配置。
    pub fn ensure_bootstrap_files(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.data_dir)?;
        if !self.config_path.exists() {
            fs::write(&self.config_path, EXAMPLE_CONFIG)?;
            tracing::info!(path = %self.config_path.display(), "首次启动：已生成示例配置文件");
        }
        tracing::info!(data_dir = %self.data_dir.display(), "数据目录已就绪");
        Ok(())
    }

    pub fn database_url(&self) -> String {
        format!("sqlite://{}/unidns.db?mode=rwc", self.data_dir.display())
    }
}

fn env_opt(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config {
            config_path: PathBuf::from("config.toml"),
            listen: DEFAULT_LISTEN.into(),
            data_dir: PathBuf::from("./data"),
            sync_interval_minutes: 0, // 允许为 0（停用自动同步）
            admin_password: None,
            master_key_hex: None,
        };
        assert!(cfg.database_url().starts_with("sqlite://"));
    }
}
