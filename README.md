# UniDNS · 多平台域名解析集中管理

自托管的 Web 服务，把分散在**阿里云 / 腾讯云 DNSPod（国内、国际站）/ EdgeOne（国内、国际站）/ CloudFlare / 火山引擎** 的域名解析统一纳管：一处录入各平台 API 密钥，统一查看全部域名与解析记录，创建、修改、启用/停用、删除记录时实时回写对应平台。

- **单二进制交付**：Web 界面嵌入可执行文件，运行只需一个数据目录（SQLite + 密钥文件）
- **API-first**：界面与外部系统共用同一套 REST API（`/api/v1`）
- **平台为准对账**：手动/定时同步会把平台侧的最新状态拉回本地，自动清理孤儿数据
- **凭据加密存储**：平台密钥以 AES-256-GCM 加密落库，界面与 API 只显示脱敏掩码

## 快速开始

```bash
# 1. 构建前端（Astro，需要 Node.js 18+；产物嵌入二进制）
cd frontend && pnpm install && pnpm build && cd ..

# 2. 构建并运行（单二进制）
cargo build --release
./target/release/unidns init      # 可选：先生成示例 config.toml（已存在则不覆盖）
./target/release/unidns           # 不带子命令：启动 Web 服务
# 首次启动会生成 config.toml 与 data/ 目录，然后访问 http://127.0.0.1:18686
```

首次访问进入「设置管理员口令」引导；之后即可添加平台账户并同步域名。

### 命令行子命令

```
unidns              不带子命令：加载配置并启动 Web 服务（与既有部署脚本兼容）
unidns init         生成示例配置文件（遵循 UNIDNS_CONFIG，默认 ./config.toml；已存在则不覆盖）
unidns reset-password  强制重置管理员口令（忘记口令时使用，跳过旧口令校验，交互输入新口令）
unidns version      打印版本号（单行，便于脚本解析）
unidns help         打印帮助（-h / --help 等价）
```

成功时退出码 `0`；`init` 目标已存在、`reset-password` 口令不合规、
未知子命令或非法标志时退出码 `2`，未知子命令还会把帮助信息打印到标准错误且不启动服务。

`reset-password` 是忘记口令时的运维通道：连接配置（`UNIDNS_DATA_DIR` /
`UNIDNS_CONFIG`）指向的数据库，交互式输入新口令后直接覆盖
`admin_password_hash`，**不校验旧口令**。执行前应先停止 Web 服务以避免写冲突，
完成后重启服务即可用新口令登录。此操作不影响已存的账户凭据（凭据用 `master_key`
加密，与口令哈希无关），但要求 `data/secret.key` 仍在。

无真实平台凭据想先体验界面：

```bash
cargo run --example demo_server   # 内置内存 mock 平台，访问 http://127.0.0.1:18099
```

> 腾讯云国内站与国际站是两套独立的账号与密钥体系：国际站（tencentcloud.com / edgeone.ai）注册的账号请选择「腾讯云 DNSPod（国际站）」或「EdgeOne（国际站）」类型，密钥在国内站端点上无法使用，反之亦然。

## 各平台最小权限密钥创建指引

| 平台 | 密钥类型 | 创建入口 | 最小权限 |
| --- | --- | --- | --- |
| 阿里云 | AccessKey ID + Secret | RAM 访问控制 → AccessKey 管理（建议为子账号创建） | 授权策略 `AliyunDNSFullAccess`（只读场景可用自定义策略限定 `dns:Describe*`） |
| 腾讯云 DNSPod 国内站 | SecretId + SecretKey | 国内站（cloud.tencent.com）访问管理 CAM → API 密钥管理 | 挂载 `QcloudDNSPodFullAccess` 策略 |
| 腾讯云 DNSPod 国际站 | SecretId + SecretKey | 国际站（tencentcloud.com）账号的 API 密钥管理 | DNSPod 解析读写权限 |
| EdgeOne 国内站 | SecretId + SecretKey | 国内站 CAM → API 密钥管理 | EdgeOne（teo）读写权限；站点需为 NS 接入模式才有 DNS 记录管理 |
| EdgeOne 国际站 | SecretId + SecretKey | 国际站（edgeone.ai）账号的 API 密钥管理 | EdgeOne（teo）读写权限；站点需为 NS 接入模式 |
| CloudFlare | API Token | Dashboard → My Profile → API Tokens → Create Token | 模板 "Edit zone DNS"，Zone Resources 限定到需要的域名 |
| 火山引擎 | AccessKey ID + Secret Access Key | IAM 控制台 → 访问密钥（建议为 IAM 子用户创建） | 授予云解析 DNS（DNS）读写权限的策略 |

> 建议：永远为 UniDNS 使用独立的子账号/Token，不要使用主账号全局密钥。

## 配置

配置来源优先级：**环境变量 > `config.toml`（工作目录） > 默认值**。首次启动会生成带注释的示例 `config.toml`。

| 配置项 | 环境变量 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `listen` | `UNIDNS_LISTEN` | `0.0.0.0:18686` | HTTP 监听地址 |
| `data_dir` | `UNIDNS_DATA_DIR` | `./data` | SQLite 与密钥文件目录 |
| `sync_interval_minutes` | `UNIDNS_SYNC_INTERVAL` | `0` | 定时全量同步间隔（分钟）；`0` 停用，启用时至少 30（登录后可在界面调整） |
| `admin_password` | `UNIDNS_ADMIN_PASSWORD` | 无 | 预置管理员口令（仅首次设置口令时生效；忘记口令用 `unidns reset-password` 重置） |
| `master_key` | `UNIDNS_MASTER_KEY` | 自动生成 | 凭据加密主密钥（64 位 hex；缺省时生成 `data/secret.key`） |
| — | `UNIDNS_CONFIG` | `config.toml` | 配置文件路径 |
| — | `UNIDNS_LOG` | `info` | 日志级别（RUST_LOG 语法） |

### 主密钥备份（重要）

平台凭据用主密钥加密存储。若使用自动生成的 `data/secret.key`，**请随数据目录一起备份**；丢失该文件将无法解密已保存的凭据（重新录入即可恢复）。容器化部署建议通过 `UNIDNS_MASTER_KEY` 注入固定密钥，例如：

```bash
openssl rand -hex 32   # 生成主密钥
UNIDNS_MASTER_KEY=<hex> ./unidns
```

管理员口令以 argon2id 单向哈希存储，数据库泄露不会暴露口令本身。

## Docker 部署

官方镜像发布在 GitHub Container Registry：`ghcr.io/jetsung/unidns:latest`（多架构 amd64/arm64），可直接拉取运行，无需本地构建。镜像为三阶段构建：前端 Astro 产物 → Rust musl 静态编译 → distroless/static 非 root 运行时（自带 ca-certificates）。

### docker compose（推荐）

在部署目录（如 `/opt/unidns`）新建 `compose.yaml`：

```yaml
services:
  unidns:
    image: ghcr.io/jetsung/unidns:latest
    container_name: unidns
    restart: unless-stopped
    env_file:
      - path: ./.env
        required: false
    ports:
      - "18686:18686"
    volumes:
      - ./data:/app/data
```

同目录可选新建 `.env` 注入配置常量（容器内已默认监听 `0.0.0.0:18686`、数据目录 `/app/data`，无需设置 `UNIDNS_LISTEN` / `UNIDNS_DATA_DIR`）：

```bash
# 凭据加密主密钥（64 位 hex，建议固定；留空则首次启动自动生成 data/secret.key）
UNIDNS_MASTER_KEY=
# 预置管理员口令（仅首次设置口令时生效）
UNIDNS_ADMIN_PASSWORD=
# 定时全量同步间隔（分钟）；0 停用自动同步，启用时至少 30
UNIDNS_SYNC_INTERVAL=0
# 日志级别（RUST_LOG 语法）
UNIDNS_LOG=info
```

```bash
mkdir -p data && chown -R 65532:65532 ./data   # 容器以 uid 65532 运行，目录需归属该用户
docker compose up -d
docker compose logs -f
```

访问 `http://127.0.0.1:18686`，首次进入「设置管理员口令」引导。数据（SQLite 数据库与 `secret.key`）持久化在 `./data`（相对 `compose.yaml` 所在目录），备份该目录即可。

> `env_file` 的 `path` / `required` 写法需要 Docker Compose v2.24+；旧版本可改为 `env_file: ./.env` 并确保文件存在。

### docker build / docker run（源码构建）

提供两份构建文件（除镜像源外完全一致）：`docker/Dockerfile`（官方源，供 CI/上线）与 `docker/CN.Dockerfile`（国内镜像源）。仓库内 `docker/compose.yaml` 使用本地构建的 `unidns:dev` 镜像：

```bash
cd docker
docker compose up -d          # 首次自动构建 unidns:dev（默认走 CN.Dockerfile）
docker compose logs -f
```

或直接构建运行：

```bash
# 中国网络环境（国际/上线改用 docker/Dockerfile）
docker build -f docker/CN.Dockerfile -t unidns:dev .

mkdir -p data && chown -R 65532:65532 ./data
docker run -d --name unidns -p 18686:18686 \
  -v "$PWD/data:/app/data" \
  -e UNIDNS_MASTER_KEY=<hex> \
  --restart unless-stopped \
  unidns:dev
```

数据（SQLite 数据库与 `secret.key`）持久化在挂载卷 `/app/data` 中。注意：本地 `data/` 与 `frontend/dist/` 已被 `.dockerignore` 排除，不会进镜像，前端产物始终在构建阶段内重新生成。

## REST API 概览

除 `POST /api/v1/auth/login|setup` 与 `GET /api/v1/auth/status` 外均需登录会话（Cookie）。错误响应统一为 `{"error": {"code": "...", "message": "..."}}`。

```
POST   /api/v1/auth/login|setup|logout|password     会话与口令管理
GET    /api/v1/auth/status|me                       登录态
GET/POST        /api/v1/accounts                    账户列表 / 添加
GET/PATCH/DELETE /api/v1/accounts/{id}              详情 / 修改（名称、启停、换凭据）/ 删除
POST   /api/v1/accounts/{id}/verify                 连通性验证
POST   /api/v1/accounts/{id}/sync                   手动同步单账户
POST   /api/v1/sync                                 全量同步
GET    /api/v1/sync/runs?account_id=&limit=         同步历史
GET/PUT /api/v1/sync/settings                       同步间隔
GET    /api/v1/domains?keyword=&provider=           域名统一视图
GET    /api/v1/domains/{id}                         域名详情
GET/POST /api/v1/domains/{id}/records               记录查询（type/keyword/status + 分页）/ 创建
PATCH/DELETE /api/v1/records/{id}                   修改 / 删除
POST   /api/v1/records/{id}/status                  启用/停用
```

创建记录示例：

```bash
curl -X POST http://127.0.0.1:18686/api/v1/domains/1/records \
  -H 'Content-Type: application/json' -b cookie.txt \
  -d '{"host":"www","rtype":"A","value":"203.0.113.10","ttl":600}'
```

## 平台能力差异

| 能力 | 阿里云 | DNSPod（国内/国际） | EdgeOne（国内/国际） | CloudFlare | 火山引擎 |
| --- | --- | --- | --- | --- | --- |
| 域名/记录同步 | ✅ | ✅ | ✅ 站点（Zone） | ✅ | ✅ Zone |
| 记录增删改 | ✅ | ✅ | ✅ | ✅ | ✅ |
| 记录启用/停用 | ✅ | ✅ | ✅ ModifyDnsRecordsStatus | ❌（平台 API 不支持，操作会返回明确提示） | ✅ UpdateRecordStatus |
| 解析线路 | ✅ Line | ✅ RecordLine | ✅ Location | — | ✅ Line（默认 `default`） |
| 记录备注 | ✅ | ✅ | ❌ | ❌ | ✅（≤16 字符） |
| 域名备注 | ✅ | ✅ | ❌ | ❌ | ✅ |
| 代理状态 | — | — | —（代理在站点接入层） | ✅ proxied | — |

## 开发

```bash
cargo test                                        # 全部单元与集成测试
cargo test -- --ignored                           # 真实凭据连通性测试（见下方环境变量）
cargo clippy --all-targets -- -D warnings         # 静态检查
cargo run --example demo_server                   # 带 mock 平台的演示服务
```

### 前端开发（frontend/，Astro 5）

```bash
cd frontend
pnpm install
pnpm dev        # 开发服务器（/api 代理到 127.0.0.1:18686）
pnpm build      # 产物输出到 frontend/dist，由 rust-embed 嵌入二进制
```

界面为手写 CSS 设计系统（深色侧边栏 + 浅色内容区），多页静态路由：
`/setup`（首启引导）、`/login`、`/accounts`（平台账户）、`/domains`（域名列表）、`/domains/detail?id=`（解析记录管理）。
不依赖任何 CDN 资源；改前端后需 `pnpm build && cargo build` 重新嵌入。

真实凭据集成测试（默认跳过，凭据经环境变量注入、绝不入库）：

```bash
UNIDNS_TEST_ALIYUN_AK_ID=... UNIDNS_TEST_ALIYUN_AK_SECRET=... cargo test aliyun_real -- --ignored
UNIDNS_TEST_DNSPOD_SECRET_ID=... UNIDNS_TEST_DNSPOD_SECRET_KEY=... cargo test dnspod_real -- --ignored
UNIDNS_TEST_CF_TOKEN=... cargo test cloudflare_real -- --ignored
UNIDNS_TEST_VOLC_AK_ID=... UNIDNS_TEST_VOLC_AK_SECRET=... cargo test volcengine_real -- --ignored
```

## 安全说明

- 自托管单管理员设计：请勿暴露到公网无防护使用；建议置于内网或反代 + TLS 之后
- 会话 Cookie 为 HttpOnly（24 小时不活动过期），凭据/平台密钥永不明文回显
- 删除账户只清理本地数据，不会调用任何平台写接口

## 仓库镜像

[MyCode](https://git.jetsung.com/jetsung/unidns) ● [AtomGit](https://atomgit.com/jetsung/unidns) ● [GitHub](https://github.com/jetsung/unidns)
