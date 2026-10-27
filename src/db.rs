use std::{str::FromStr, time::Duration};

use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

use crate::config::Config;

/// 创建 SQLite 连接池（WAL + 外键 + 忙等待），并执行数据库迁移。
pub async fn init_pool(database_url: &str) -> Result<SqlitePool, sqlx::Error> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

pub async fn init_from_config(cfg: &Config) -> Result<SqlitePool, sqlx::Error> {
    init_pool(&cfg.database_url()).await
}

pub async fn get_setting(pool: &SqlitePool, key: &str) -> sqlx::Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(v,)| v))
}

pub async fn set_setting(pool: &SqlitePool, key: &str, value: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let pool = init_pool("sqlite::memory:")
            .await
            .expect("初始化内存库失败");
        // 再次执行迁移应幂等成功
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("重复迁移失败");

        set_setting(&pool, "k", "v").await.unwrap();
        assert_eq!(get_setting(&pool, "k").await.unwrap().as_deref(), Some("v"));
        set_setting(&pool, "k", "v2").await.unwrap();
        assert_eq!(
            get_setting(&pool, "k").await.unwrap().as_deref(),
            Some("v2")
        );
    }
}
