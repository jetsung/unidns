# 快速开始

UniDNS 单二进制即可运行：编译期前端已嵌入，运行只依赖可执行文件与 SQLite
数据目录。以下按「本地编译 → 首次启动 → 添加账户」三步走通。

## 前置要求

- Rust stable（1.85+）与 pnpm（可用 [mise](https://mise.jdx.dev) 一键装齐：
  `mise use -g rust node pnpm just`）
- 首次编译需要构建前端产物（rust-embed 编译期嵌入 `frontend/dist`）

## 编译与启动

=== "从源码编译"

    ```bash
    # 1. 构建前端静态产物（仅前端变更后需要重新执行）
    pnpm -C frontend install
    pnpm -C frontend build

    # 2. 编译并运行（默认监听 0.0.0.0:18686，数据目录 ./data）
    cargo run --release
    ```

=== "使用 just（推荐）"

    ```bash
    just frontend-install   # 首次安装前端依赖
    just build              # 前端 + cargo build --release
    just backend            # 启动服务
    ```

=== "Docker"

    镜像内使用默认端口 `18686` 与默认数据目录（工作目录 `/app` 下的 `./data`，
    即 `/app/data`），无需设置 `UNIDNS_LISTEN` / `UNIDNS_DATA_DIR`。中国网络
    环境构建走 `CN.Dockerfile`（国内镜像源），国际/上线构建走 `Dockerfile`
    （官方源），运行方式完全一致。

    **docker run 方式**

    ```bash
    # 构建（仓库根目录执行；中国网络把 -f 换成 docker/CN.Dockerfile）
    docker build -f docker/Dockerfile -t unidns:dev .

    # 运行：把主机目录挂到容器数据目录 /app/data 持久化数据
    mkdir -p data && chmod 0777 data   # 容器以 uid 65532 运行，目录需可写
    docker run -d --name unidns \
      -p 18686:18686 \
      -v "$(pwd)/data:/app/data" \
      --restart unless-stopped \
      unidns:dev
    ```

    **docker compose 方式**

    ```bash
    docker compose -f docker/compose.yaml up -d      # 首次会按需构建 unidns:dev
    docker compose -f docker/compose.yaml logs -f    # 跟踪日志
    docker compose -f docker/compose.yaml down       # 停止并移除容器
    ```

    compose 定义见 `docker/compose.yaml`：端口映射 `18686:18686`，数据落在
    仓库根的 `data/`。可选配置通过环境变量注入（优先级高于 config.toml）：
    `UNIDNS_MASTER_KEY`（凭据加密主密钥，64 位 hex）、`UNIDNS_ADMIN_PASSWORD`
    （预置管理员口令）、`UNIDNS_SYNC_INTERVAL`（定时同步间隔分钟数，0 停用）、
    `UNIDNS_LOG`（日志级别）。

    启动后访问 `http://127.0.0.1:18686`，首次进入 `/setup` 设置管理员口令。

启动成功后访问 `http://127.0.0.1:18686`。

## 命令行用法

```bash
unidns              # 不带子命令：加载配置并启动 Web 服务（与既有部署脚本兼容）
unidns init         # 生成示例 config.toml（遵循 UNIDNS_CONFIG，已存在则不覆盖）
unidns version      # 打印版本号（单行）
unidns help         # 打印帮助（-h / --help 等价）
```

`init` 的目标路径与启动时读取的配置路径同源：默认当前目录的 `config.toml`，
设置 `UNIDNS_CONFIG` 时指向该路径（父目录不存在会自动创建）。目标文件已存在时
不会覆盖，直接以退出码 `2` 结束，避免误删已调优的配置。

成功时退出码 `0`；`init` 目标已存在、未知子命令或非法标志时退出码 `2`，
未知子命令还会把帮助信息打印到标准错误，且不会启动服务。

## 首次启动引导

1. 服务启动时自动创建数据目录、迁移数据库；若 `config.toml` 不存在会生成
   带注释的示例配置。
2. 打开首页会进入 `/setup` 引导页：设置管理员口令（首次启动也可用环境变量
   `UNIDNS_ADMIN_PASSWORD` 预置，仅创建时生效）。
3. 凭据加密主密钥缺省自动生成，保存到 `<data_dir>/secret.key`（权限 0600）。
   **请随数据目录一起备份**——丢失后已保存的平台凭据无法解密。
   也可通过 `UNIDNS_MASTER_KEY` 显式指定（64 位 hex，32 字节）。

## 添加 DNS 平台账户

1. 登录后进入「账户管理」，点「新建账户」。
2. 选择平台类型（阿里云 / DNSPod 国内 / DNSPod 国际 / EdgeOne 国内 /
   EdgeOne 国际 / CloudFlare），粘贴对应凭据 JSON——各平台凭据格式见
   [DNS 提供商](04-providers.md)。
3. 点「验证」确认凭据可用（调用平台 API 做连通性检查），然后保存。

账户保存后点「同步」即可把该平台的域名与解析记录拉取到本地；之后在
「域名管理」里统一查看与编辑。

## 数据备份

需要备份的只有数据目录（默认 `./data/`），内含：

| 文件 | 内容 |
|---|---|
| `unidns.db` | SQLite 数据库（账户/域名/记录/运行历史） |
| `secret.key` | 凭据加密主密钥（**必须**，否则凭据不可恢复） |
| `config.toml` | 生成的示例配置（可自行修改） |

数据库为 WAL 模式，热备份建议用 `sqlite3 unidns.db ".backup backup.db"`。
