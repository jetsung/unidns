use std::{net::SocketAddr, sync::Arc};

use unidns::{
    config::{self, Config},
    crypto, db, providers, sync, web,
};

const HELP_TEXT: &str = r#"unidns — 统一 DNS 管理服务

用法: unidns [子命令]

不带子命令时默认启动 Web 服务。

子命令:
  init      生成示例配置文件 config.toml（遵循 UNIDNS_CONFIG，已存在时不覆盖）
  version   输出版本号
  help      打印本帮助信息

标志:
  -h, --help  等价于 help
"#;

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("UNIDNS_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// `init` 子命令：生成示例配置文件，已存在时不覆盖。
fn run_init() -> i32 {
    let path = config::config_path();
    if path.exists() {
        eprintln!("配置文件 {} 已存在，不覆盖。", path.display());
        return 2;
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("创建目录 {} 失败: {e}", parent.display());
        return 2;
    }
    match std::fs::write(&path, config::EXAMPLE_CONFIG) {
        Ok(()) => {
            println!("已生成示例配置文件: {}", path.display());
            0
        }
        Err(e) => {
            eprintln!("写入配置文件 {} 失败: {e}", path.display());
            2
        }
    }
}

/// `version` 子命令：输出单行版本号。
fn run_version() -> i32 {
    println!("{}", env!("CARGO_PKG_VERSION"));
    0
}

fn print_help(to_stderr: bool) {
    if to_stderr {
        eprint!("{HELP_TEXT}");
    } else {
        print!("{HELP_TEXT}");
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        None => match serve().await {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("错误: {e}");
                std::process::ExitCode::FAILURE
            }
        },
        Some("init") => std::process::ExitCode::from(run_init() as u8),
        Some("version") => std::process::ExitCode::from(run_version() as u8),
        Some("help" | "-h" | "--help") => {
            print_help(false);
            std::process::ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("未知子命令: {other}");
            print_help(true);
            std::process::ExitCode::from(2)
        }
    }
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::load().map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    init_tracing();
    cfg.ensure_bootstrap_files()
        .map_err(|e| -> Box<dyn std::error::Error> {
            format!("初始化数据目录失败: {e}").into()
        })?;

    let pool = db::init_from_config(&cfg).await?;
    let master_key =
        crypto::load_or_create_master_key(&cfg.data_dir, cfg.master_key_hex.as_deref())
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // 预置口令：仅首次（未设置口令）时生效
    if let Some(pw) = &cfg.admin_password
        && !crypto::has_admin_password(&pool).await
    {
        crypto::set_admin_password(&pool, None, pw)
            .await
            .map_err(|e| Box::<dyn std::error::Error>::from(format!("预置管理员口令失败: {e}")))?;
        tracing::info!("已按配置预置管理员口令");
    }

    let state: web::SharedState = Arc::new(web::AppState {
        pool: pool.clone(),
        master_key: Arc::new(master_key),
        provider_factory: providers::default_provider_factory(),
        sync_running: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    });

    // 清理服务中断遗留的僵尸同步记录
    let zombies = sqlx::query(
        "UPDATE sync_runs SET status = 'failed', finished_at = ?, \
         error = COALESCE(error, '服务重启，任务中断') WHERE status = 'running'",
    )
    .bind(unidns::models::now_utc())
    .execute(&pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    if zombies > 0 {
        tracing::warn!("已清理 {} 条服务中断遗留的同步记录", zombies);
    }

    // 同步间隔初始值（settings 中未设置时取配置值）
    if db::get_setting(&pool, sync::SYNC_INTERVAL_KEY)
        .await?
        .is_none()
    {
        db::set_setting(
            &pool,
            sync::SYNC_INTERVAL_KEY,
            &cfg.sync_interval_minutes.to_string(),
        )
        .await?;
    }
    // 定时全量同步调度器
    sync::spawn_scheduler(state.clone());

    let app = web::router(state);

    let addr: SocketAddr = cfg.listen.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("unidns 正在监听 http://{}", cfg.listen);
    axum::serve(listener, app).await?;
    Ok(())
}
