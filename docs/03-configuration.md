# 配置参考

配置三级覆盖：**默认值 ← 配置文件 `config.toml` ← 环境变量 `UNIDNS_*`**。
配置文件路径可用 `UNIDNS_CONFIG` 指定（默认 `./config.toml`）；文件不存在时
首次启动会自动生成带注释的示例配置。

## 配置项

| 配置项 | 环境变量 | 默认值 | 说明 |
|---|---|---|---|
| `listen` | `UNIDNS_LISTEN` | `0.0.0.0:18686` | HTTP 监听地址 |
| `data_dir` | `UNIDNS_DATA_DIR` | `./data` | 数据目录：SQLite 数据库、`secret.key`、示例配置 |
| `sync_interval_minutes` | `UNIDNS_SYNC_INTERVAL` | `0` | 定时全量同步间隔（分钟）；`0` 停用自动同步，大于 0 至少 30（1–29 视为停用） |
| `admin_password` | `UNIDNS_ADMIN_PASSWORD` | 无 | 预置管理员口令，仅首次创建口令时生效 |
| `master_key` | `UNIDNS_MASTER_KEY` | 自动生成 | 凭据加密主密钥，64 位 hex（32 字节） |
| — | `UNIDNS_CONFIG` | `./config.toml` | 配置文件路径（仅环境变量） |

## 示例

=== "config.toml"

    ```toml
    # HTTP 监听地址
    listen = "0.0.0.0:18686"

    # 数据目录（SQLite 数据库与 secret.key 存放位置）
    data_dir = "./data"

    # 定时全量同步间隔（分钟）；0 表示停用自动同步，启用时至少 30 分钟
    sync_interval_minutes = 0

    # 预置管理员口令（仅首次创建口令时生效；也可用环境变量 UNIDNS_ADMIN_PASSWORD）
    #admin_password = "change-me"

    # 凭据加密主密钥（64 位 hex，表示 32 字节；缺省时自动生成并保存到 <data_dir>/secret.key）
    #master_key = "0000000000000000000000000000000000000000000000000000000000000000"
    ```

=== "环境变量"

    ```bash
    export UNIDNS_LISTEN=127.0.0.1:18686
    export UNIDNS_DATA_DIR=/var/lib/unidns
    export UNIDNS_SYNC_INTERVAL=30
    export UNIDNS_ADMIN_PASSWORD='first-login-password'
    export UNIDNS_MASTER_KEY=$(openssl rand -hex 32)
    cargo run --release
    ```

## 主密钥（master_key）

平台凭据以 AES-256-GCM 加密落库，加密密钥来源优先级：

1. `UNIDNS_MASTER_KEY` 环境变量（64 位 hex）；
2. `<data_dir>/secret.key` 文件——不存在时自动生成（权限 0600）。

!!! warning "备份"

    `secret.key` 丢失后已保存的平台凭据**不可恢复**（密文无法解密），
    只能删除账户重新录入。请把 `secret.key` 与 `unidns.db` 一起备份。

## 同步设置（运行期）

定时同步的启停与间隔也可在界面「同步设置」里调整（存 `settings` 表）：
环境变量/配置文件的值是**初始默认**，界面修改后以数据库中的值为准。
域名级同步开关同样在界面操作，关闭的域名不参与同步，其本地备注不回写平台。
