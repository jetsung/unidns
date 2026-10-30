# UniDNS 项目规格书（SPEC）

> 本文档是 UniDNS 的完整需求与规范说明。目标：仅凭本文档，即可用任意编程语言
> 独立重新实现功能等价的 UniDNS。文档覆盖产品需求、行为契约、数据模型、API
> 契约、安全约定与部署形态；具体实现语言与技术选型仅在「参考实现」一节说明，
> 不构成对重实现的约束。
>
> 需求来源：`openspec/specs/` 五份主规格 + `docs/` 架构、配置、提供商、API 文档
> + `migrations/0001_init.sql` 数据模型 + `docs/openapi.yaml` 机器可读契约。
> 行为以本文档为准；REST 接口的字段级细节以 `docs/openapi.yaml` 为准。

## 1. 产品概述

UniDNS 是多平台 DNS 记录统一管理系统（自托管，Web 控制台形态）：

- 把阿里云、腾讯云 DNSPod（国内/国际站）、EdgeOne（国内/国际站）、CloudFlare、
  火山引擎七类 DNS 平台聚合到一个界面；同一平台可添加多个账户。
- 从平台拉取域名（EdgeOne 为站点）列表与解析记录，维护本地统一视图。
- 在统一视图上对解析记录执行跨平台一致的创建、修改、启用/停用与删除，
  操作实时回写对应平台。
- 支持手动同步与定时全量同步，记录每次同步的结果与错误。
- 单二进制部署：前端静态资源编译期嵌入，运行只依赖一个可执行文件加
  SQLite 数据目录。品牌名面向用户统一展示为 `UniDNS`。

### 1.1 术语

| 术语 | 含义 |
|---|---|
| 账户（account） | 某一 DNS 平台的 API 凭据条目，七种平台类型之一 |
| 域名/站点（domain） | 从平台同步下来的 zone；EdgeOne 对应「站点」 |
| 解析记录（record） | 域名下的一条 DNS 记录，含上游平台记录标识 |
| 同步（sync） | 从平台拉取域名与解析记录并按平台侧数据覆盖本地 |
| 回写 | 本地的记录写操作实时调用平台 API 落到平台侧 |
| 数据目录 | 存放 SQLite 数据库、`secret.key` 主密钥的目录 |

## 2. 总体架构

### 2.1 架构形态

三层结构，全部状态经共享应用状态（等价于 `Arc<AppState>`）传递：

```
浏览器 ──静态页面──> 内嵌 Web 前端（编译期嵌入二进制）
   └──/api/v1/*──> Web 层（会话鉴权）──> 同步引擎 ──> 平台适配器 ──> 七个 DNS 平台
                        └───────────── SQLite（WAL 模式）<────────────┘
```

- **Web 层**：HTTP 路由与会话鉴权，按 auth / accounts / records / sync 分组。
- **同步引擎**：账户级并发调度、平台限流退避、同步运行记录。
- **平台适配器**：统一的提供商接口 + 七个实现 + 按平台类型构造的工厂。
- **存储**：SQLite，WAL 模式 + 外键约束 + 忙等待；启动时执行迁移。

### 2.2 功能模块划分

| 模块 | 职责 |
|---|---|
| 入口 | 加载配置、初始化主密钥与数据库连接池、启动 HTTP 服务 |
| 配置 | 默认值 ← 配置文件 ← 环境变量 三级覆盖加载 |
| 加密 | 凭据 AES-256-GCM 加解密、主密钥管理、管理员口令 Argon2id 哈希 |
| 数据库 | 连接池与迁移（五张表，见 §6） |
| 数据行结构 | Account / Domain / Record / SyncRun 等数据模型 |
| 平台适配器 | 统一提供商接口 + 七实现 + 工厂 |
| 同步引擎 | 账户并发、限流退避、运行记录、定时调度 |
| Web 层 | REST API 路由 + 会话鉴权 + 统一错误 |
| 静态前端 | Astro 构建产物在编译期嵌入（含品牌图标、Web App Manifest 与 Service Worker）；开发时代理 `/api` 到后端 |
| 错误 | 统一错误类型：到 HTTP 状态码与 `{"error":{code,message}}` 的映射 |

### 2.3 命令行入口

二进制名为 `unidns`（代码标识符，不受品牌名约束）：

- `unidns`（无参数）：加载配置并启动 Web 服务（与既有部署方式向后兼容）。
- `unidns init`：生成示例配置文件；目标路径取自 `UNIDNS_CONFIG` 环境变量，
  未设置时为当前目录 `config.toml`；父目录不存在会自动创建；目标已存在时
  **不覆盖**，输出提示并以退出码 `2` 结束；成功输出生成路径、退出码 `0`。
- `unidns version`：标准输出打印单行版本号（编译期注入），退出码 `0`，
  不启动服务。
- `unidns help`、`unidns -h`、`unidns --help`：标准输出打印帮助，列出三个
  子命令说明并指明无参默认启动 Web 服务，退出码 `0`。
- 未知子命令：帮助信息打印到标准错误，退出码 `2`，不启动服务。

## 3. 配置规范

### 3.1 三级覆盖

配置优先级：**默认值 ← 配置文件 `config.toml` ← 环境变量 `UNIDNS_*`**。
配置文件路径由 `UNIDNS_CONFIG` 指定（仅环境变量生效），默认 `./config.toml`；
文件不存在时首次启动自动生成带注释的示例配置（内容即 §3.2 全部键的注释版）。

### 3.2 配置项

| 配置项 | 环境变量 | 默认值 | 说明 |
|---|---|---|---|
| `listen` | `UNIDNS_LISTEN` | `0.0.0.0:18686` | HTTP 监听地址 |
| `data_dir` | `UNIDNS_DATA_DIR` | `./data` | 数据目录：SQLite 数据库、`secret.key`、示例配置 |
| `sync_interval_minutes` | `UNIDNS_SYNC_INTERVAL` | `0` | 定时全量同步间隔（分钟）；`0` 停用自动同步，大于 0 至少 30 |
| `admin_password` | `UNIDNS_ADMIN_PASSWORD` | 无 | 预置管理员口令，仅首次创建口令时生效 |
| `master_key` | `UNIDNS_MASTER_KEY` | 自动生成 | 凭据加密主密钥，64 位 hex（32 字节） |
| — | `UNIDNS_CONFIG` | `./config.toml` | 配置文件路径（仅环境变量） |
| — | `UNIDNS_LOG` | — | 日志级别（RUST_LOG 语法），可选 |

间隔归一化规则（配置读取与 API 校验必须一致）：`0` 表示停用；`1`–`29` 归一化
为 `0`（视为停用）；`≥ 30` 原样生效；负数在 API 层拒绝（400）。

### 3.3 主密钥（master_key）

平台凭据以 AES-256-GCM 加密落库，加密密钥来源优先级：

1. `UNIDNS_MASTER_KEY` 环境变量（64 位 hex，表示 32 字节）；
2. `<data_dir>/secret.key` 文件——不存在时自动生成（权限 `0600`）。

`secret.key` 丢失后已保存的平台凭据不可恢复（密文无法解密），只能删除账户
重新录入；文档必须提示用户把 `secret.key` 与数据库一起备份。

## 4. 安全规范

- **管理员口令**：以 Argon2id 单向哈希存储（存于 `settings` 表，键
  `admin_password_hash`）；任何接口与存储文件不得出现口令明文。口令长度
  至少 8 个字符。首次启动未设置口令时引导设置，也可用 `UNIDNS_ADMIN_PASSWORD`
  预置（仅创建时生效）。修改口令需验证旧口令，新口令立即生效。忘记口令时提供
  `unidns reset-password` 子命令作为运维通道：连接配置指向的数据库，交互式
  输入新口令后直接覆盖哈希，**跳过旧口令校验**（要求执行者具备数据库
  文件访问权限，执行前应停止 Web 服务）；不影响已存的账户凭据。
- **会话**：登录成功颁发会话 Cookie（内存会话存储；Cookie 属性 httpOnly、
  `SameSite=Lax`，不设 Secure 以兼容反向代理终止 TLS 的 HTTP 部署）；除登录
  引导接口（`auth/status`、`auth/login`、`auth/setup`）外一切 API 与页面要求
  有效会话，未认证返回 HTTP 401 与错误码 `unauthorized`。会话 24 小时无活动
  过期；支持手动登出，登出或过期后需重新登录。
- **凭据存储**：凭据 JSON 以 AES-256-GCM 加密为密文（nonce 前置）落库
  （`credentials_enc`），密钥与密文同机保存；同时保存打码展示串
  （`credentials_mask`，如 `****9999`）。任何账户查询接口与界面 **不得**
  返回完整凭据明文。

## 5. 平台适配器规范

### 5.1 统一接口

每个平台适配器实现统一接口（语义等价于以下方法集）：

```
verify()                                          // 凭据连通性验证（最小只读调用）
list_domains() -> [域名]
list_records(zone) -> [解析记录]
create_record(zone, input) -> 平台记录标识
update_record(zone, record_id, input)
delete_record(zone, record_id)
set_record_status(zone, record_id, enabled)
set_record_remark(zone, record_id, remark)
set_domain_remark(zone, remark)
```

- 凭据以 JSON 字符串传入工厂 `build_provider(平台类型, credentials_json, proxy)`，
  由适配器自行解析；解析失败返回认证错误。
- 错误类型细分六类，Web 层据此映射 HTTP 状态码：
  `Auth`（认证失败）、`RateLimited`（含 `retry_after` 秒数）、`NotFound`、
  `Unsupported`（平台不支持该能力）、`Network`、`Api`（平台业务错误）。

### 5.2 平台清单与凭据

| 平台（provider_type） | 凭据字段 | 凭据来源 |
|---|---|---|
| 阿里云（`aliyun`） | `access_key_id`、`access_key_secret` | RAM 访问控制台 → AccessKey |
| DNSPod 国内（`dnspod`） | `secret_id`、`secret_key` | 腾讯云控制台 → API 密钥 |
| DNSPod 国际（`dnspod_intl`） | `secret_id`、`secret_key` | DNSPod 国际站 → API 密钥 |
| EdgeOne 国内（`edgeone`） | `secret_id`、`secret_key` | 腾讯云控制台 → API 密钥 |
| EdgeOne 国际（`edgeone_intl`） | `secret_id`、`secret_key` | EdgeOne 国际站 → API 密钥 |
| CloudFlare（`cloudflare`） | `api_token` | My Profile → API Tokens（需 Zone.DNS 编辑权限） |
| 火山引擎（`volcengine`） | `access_key_id`、`access_key_secret` | 火山引擎控制台 → AccessKey（需云解析 DNS 权限） |

- 平台类型为封闭枚举（七值），提交列表外的类型拒绝保存。
- 凭据必填字段缺失时拒绝保存并指出缺失字段。
- 国内站与国际站账号体系独立，密钥不通用；验证与同步按所选站点的端点执行。

### 5.3 平台 API 约定

- **签名**：阿里云 RPC 签名（HMAC-SHA1 + PercentEncode，GET 公共参数）；
  DNSPod 与 EdgeOne 共用腾讯云 TC3-HMAC-SHA256；CloudFlare 使用
  `Authorization: Bearer <token>`；火山引擎使用 V4 HMAC-SHA256 签名
  （`Action`/`Version` 公共参数，GET/POST RPC 调用，端点
  `dns.volcengineapi.com`）。
- **连通性验证调用**：阿里云 `DescribeDomains`、DNSPod/EdgeOne
  `DescribeZoneList`、CloudFlare `GET /zones`、火山引擎 `ListZones`；失败
  提示透传平台原始错误码与消息。
- **能力差异**：所有平台均支持验证、域名列表、记录增删改查、启用/停用、
  记录与域名备注；个别平台备注能力受上游 API 限制，调用不支持的平台返回
  `Unsupported`（HTTP 409，错误码 `provider_unsupported`）。
- EdgeOne 的记录启停对应其 `ModifyDnsRecordsStatus` 接口。
- 域名级同步开关存储于本地 `settings` 表：关闭的域名不参与同步，其本地备注
  不回写平台。

## 6. 数据模型

SQLite（WAL 模式、外键开启、忙等待）。五张表，语义如下（可直接据此建表）：

**accounts（平台账户）**

| 列 | 类型 | 约束/说明 |
|---|---|---|
| id | INTEGER | 主键自增 |
| name | TEXT | 非空，账户名 |
| provider_type | TEXT | 非空，CHECK 七值枚举：`aliyun`/`dnspod`/`dnspod_intl`/`cloudflare`/`edgeone`/`edgeone_intl`/`volcengine` |
| credentials_enc | BLOB | 非空，AES-256-GCM 密文（nonce 前置） |
| credentials_mask | TEXT | 非空默认 `''`，打码展示串 |
| status | TEXT | 非空默认 `enabled`，CHECK `enabled`/`disabled` |
| created_at / updated_at | TEXT | 非空 |
| last_verified_at | TEXT | 可空，最近验证时间 |
| last_verify_ok | INTEGER | 可空，最近验证结果 |
| last_verify_msg | TEXT | 可空，最近验证消息 |

**domains（域名）**

| 列 | 类型 | 约束/说明 |
|---|---|---|
| id | INTEGER | 主键自增 |
| account_id | INTEGER | 非空，外键 → accounts(id) ON DELETE CASCADE |
| name | TEXT | 非空；`UNIQUE(account_id, name)` |
| provider_domain_id | TEXT | 非空默认 `''`，上游平台域名标识 |
| status | TEXT | 非空默认 `''`，平台侧状态 |
| synced_at | TEXT | 非空，最近同步时间 |
| remark | TEXT | 非空默认 `''`，本地备注（不回写平台） |

**records（解析记录）**

| 列 | 类型 | 约束/说明 |
|---|---|---|
| id | INTEGER | 主键自增 |
| domain_id | INTEGER | 非空，外键 → domains(id) ON DELETE CASCADE |
| account_id | INTEGER | 非空，外键 → accounts(id) ON DELETE CASCADE |
| provider_record_id | TEXT | 非空，上游记录标识；`UNIQUE(domain_id, provider_record_id)` |
| host | TEXT | 非空，主机记录 |
| rtype | TEXT | 非空，记录类型 |
| value | TEXT | 非空，记录值 |
| ttl | INTEGER | 非空 |
| line | TEXT | 非空默认 `default`，线路 |
| proxied | INTEGER | 可空，代理状态（CloudFlare 语义） |
| status | TEXT | 非空默认 `enabled`，启用/停用 |
| pending_sync | INTEGER | 非空默认 0，待同步标记 |
| synced_at | TEXT | 非空 |
| remark | TEXT | 非空默认 `''`，本地备注 |
| weight | INTEGER | 可空，权重（EdgeOne 适用，-1~100；-1 视为未设置存空） |
| priority | INTEGER | 可空，优先级（EdgeOne MX 适用，0~50；0 视为未设置存空） |

**sync_runs（同步运行历史）**

| 列 | 类型 | 约束/说明 |
|---|---|---|
| id | INTEGER | 主键自增 |
| account_id | INTEGER | 可空，外键 → accounts(id) ON DELETE CASCADE（全量同步的聚合计录可为空） |
| scope | TEXT | 非空，CHECK `account`/`all` |
| started_at | TEXT | 非空 |
| finished_at | TEXT | 可空 |
| status | TEXT | 非空，CHECK `running`/`success`/`failed` |
| error | TEXT | 可空，失败原因 |
| stats_json | TEXT | 可空，按账户聚合的统计 JSON |

**settings（KV 配置）**：`key TEXT PRIMARY KEY, value TEXT NOT NULL`。
存放管理员口令哈希、运行期同步设置（间隔）、域名级同步开关等。

索引：`domains(account_id)`、`records(domain_id)`、`records(account_id)`、
`sync_runs(account_id)`。

## 7. 平台账户规范（provider-accounts）

### 7.1 添加账户

- 录入时必选平台类型（§5.2 七值枚举）与对应凭据字段；提交不支持的类型或
  缺失必填字段时拒绝保存并给出明确错误提示。
- 保存成功后账户出现在账户列表中；保存前应对凭据做连通性验证（§7.3）。

### 7.2 凭据加密与不回显

- 凭据 MUST 加密后持久化（§4），密钥来源与凭据密文同机保存。
- 账户列表与详情只展示脱敏形式（打码串），不返回明文。
- 直接检查数据库文件时凭据字段为密文。

### 7.3 连通性验证

- 对指定账户执行验证（调用平台最小只读接口），返回成功或失败；
  失败时给出可读原因（凭据无效/过期/权限不足透传平台原因；网络不可达提示
  网络错误）。验证结果（时间/成败/消息）记录在账户上。

### 7.4 生命周期

- 支持**停用**、**启用**、**删除**。
- 停用的账户不参与同步与解析操作；重新启用后恢复。
- 删除账户仅移除账户及其本地关联数据（域名、记录级联删除），不调用任何
  平台写接口、不影响平台侧资源。

## 8. 同步引擎规范（dns-sync）

### 8.1 手动同步

- 支持对单个已启用账户触发同步：拉取该账户下域名列表及每个域名的解析记录，
  以平台侧数据为准更新本地存储（平台新增→本地新增；平台删除→本地移除，
  不残留孤儿记录；记录归属域名与账户）。
- EdgeOne 权重/优先级随同步落库：权重 `-1`、优先级 `0` 视为未设置（本地为空），
  其余值原样保存；EdgeOne 解析线路不作为本地线路字段保存（本地为空）。
- 支持全量同步：一次触发对所有已启用账户执行；单个账户失败不中断其他账户，
  失败账户在结果中标记。

### 8.2 同步覆盖语义

某账户已有同步进行中时，对再次触发的同步（手动或全量）采取覆盖：立即中止
上一次同步任务，将其未完成的同步记录标记为失败（失败原因「被新同步覆盖」），
再开始新一轮同步。全量同步触发时某账户正在单账户同步的，该账户被中止并标记
失败后纳入本轮全量重新执行。

### 8.3 定时同步

- 间隔可配置（配置项与运行期设置，见 §3.2 与 §8.5）；间隔为 `0` 表示停用
  自动同步，该状态下 MUST NOT 自动触发任何全量同步，仅手动触发可用。
- 间隔大于 0 时至少为 30 分钟（小于 30 的正数被拒绝）；自上次同步起经过
  配置间隔后自动执行一次全量同步；管理员修改间隔后后续调度按新间隔执行。
- 默认值为 `0`（停用）。
- 调度器每 60 秒检查一次间隔设置（实时读取运行期设置），间隔为 0 时跳过
  本轮且不推进计时。

### 8.4 并发与限流

- 账户间并发：每账户一个并发任务，全局并发上限 3（信号量）；单账户失败
  不影响其余，结果按账户聚合。
- 限流退避：捕获平台限流错误后按其建议的 `retry_after` 等待（上限 60 秒，
  缺省 5 秒）并重试一次。

### 8.5 运行记录与运行期设置

- 每次同步写入 `sync_runs`（scope、状态 running/success/failed、起止时间、
  错误、按账户聚合的统计 JSON），支持分页查询历史（倒序）与单次运行详情。
- 同一账户同步进行中时重复触发被互斥标记拦截。
- 同步间隔可在界面「同步设置」运行期调整（存 `settings` 表）；配置文件/
  环境变量的值是初始默认，界面修改后以数据库中的值为准，下一个调度周期生效。
- 每次同步记录账户、开始时间、耗时、结果状态与失败原因（如凭据失效、网络
  错误、平台限流、被新同步覆盖），支持查询最近历史。

## 9. 解析记录操作规范（record-operations）

### 9.1 创建

在指定域名下创建解析记录（主机记录、类型、记录值、TTL、线路/代理状态、
权重、优先级等按平台适用的字段）；创建成功 MUST 同步写往对应平台，并保存
平台返回的记录标识。参数无效时拒绝创建并返回具体字段错误，**不调用平台
接口**。本地校验规则：

- 主机记录（host）非空，根记录使用 `@`；
- 记录类型限定八种：`A`、`AAAA`、`CNAME`、`MX`、`TXT`、`NS`、`SRV`、`CAA`；
- 记录值（value）非空；
- TTL 在 1–86400 秒之间；
- 权重（weight）非空时在 -1~100 之间，优先级（priority）非空时在 0~50 之间。

火山引擎域名的记录备注受平台长度限制校验，超长拒绝且不调用平台接口；
解析线路未提交时按 `default` 回写。

### 9.2 修改

修改已有记录的主机记录、类型、记录值、TTL、权重、优先级等字段；修改 MUST
回写到平台同一条记录。平台侧记录已不存在时返回失败并提示，本地记录标记
待同步。火山引擎更新接口要求主机记录与线路为必填项，未变更时仍以当前值
回填这两项。

### 9.3 启用/停用

对一条记录启用/停用，映射为对应平台操作（阿里云启停、DNSPod 启停、EdgeOne
`ModifyDnsRecordsStatus`、火山引擎 `UpdateRecordStatus`）；平台不支持时返回
明确的不支持提示。操作后本地状态同步更新。

### 9.4 删除

删除解析记录回写平台侧并移除本地记录；删除前要求确认，取消确认则两侧均
保持不变。

### 9.5 回写失败处理

平台接口失败时返回失败与原因（透传平台错误，如权限不足、限流）；保持本地
与平台实际状态的一致性可见（标记待同步或回滚本次本地变更），不得静默丢失
差异。

### 9.6 查询与过滤

支持按域名、所属平台/账户、记录类型、启用状态与关键字过滤解析记录，供
控制台域名详情页使用；本地备注仅存本地（`remark`，不回写平台）。

### 9.7 权重与优先级

解析记录支持可选的权重（`weight`）与优先级（`priority`）字段：

- EdgeOne 适配器在权重留空时按平台默认值 `-1`、优先级留空时按平台默认值
  `0` 处理；权重仅对 A/AAAA/CNAME 记录下发（-1~100），优先级仅对 MX 记录
  下发（0~50）。字段不适用于记录类型时不下发，不影响其他字段。
- EdgeOne 返回的记录在适配器层归一：权重 `-1`、优先级 `0` 视为未设置
  （本地为空）；解析线路不纳入统一视图（`line` 恒为空）。
- 修改接口以适配器返回值落库，保证归一结果持久化；其他平台两项恒为空，
  即使传入也不会下发到平台。
- 权重/优先级越界直接拒绝且不调用平台接口（见 §9.1 校验规则）。

## 10. REST API 规范（web-console）

### 10.1 通用约定

- 全部业务接口挂在 `/api/v1` 前缀下，JSON 请求/响应。
- 鉴权：除登录引导接口（`auth/status`、`auth/setup`、`auth/login`）外，
  一律要求会话 Cookie；未认证返回 401 与 `unauthorized`。
- 统一错误响应体：`{"error": {"code": "...", "message": "..."}}`。
  错误码 → HTTP 状态码映射：

  | 错误码 | HTTP |
  |---|---|
  | `invalid_request` | 400 |
  | `unauthorized` | 401 |
  | `not_found` | 404 |
  | `provider_auth` | 400（凭据无效） |
  | `provider_not_found` | 404（平台侧资源不存在） |
  | `provider_unsupported` | 409 |
  | `provider_rate_limited` | 429（附带建议重试秒数） |
  | `provider_network` / `provider_api` | 502 |
  | `internal` | 500 |

  平台类错误（`provider_*`）透传平台原始消息；JSON 请求体反序列化失败同样
  映射为 400 + 统一错误体。

### 10.2 分页约定

列表接口接受 `page`（页码，从 1 起，默认 1）与 `page_size`（默认 10，
钳制到 1–200）查询参数；响应统一为 `{total, page, page_size, items}`，
其中 `page_size` 为本次实际采用的值。`page` 超出总页数返回空
`items` 而非错误；过滤参数与分页同时生效，`total` 为过滤后的总数。
同步运行历史接口（`/sync/runs`）使用 `account_id` 过滤与 `limit`（默认 50，
钳制 1–500）代替分页。

### 10.3 端点清单

**认证 `/auth`**（全部 JSON）：

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/auth/status` | 初始化状态：是否已设置管理员口令 |
| POST | `/auth/setup` | 首次设置管理员口令（仅口令未设置时可用） |
| POST | `/auth/login` | 登录，设置会话 Cookie |
| GET | `/auth/me` | 当前登录信息 |
| POST | `/auth/logout` | 退出登录 |
| POST | `/auth/password` | 修改口令（需验证旧口令） |

**账户 `/accounts`**：

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/accounts?page=&page_size=` | 账户分页列表（含最近同步结果） |
| POST | `/accounts` | 新建账户（校验凭据后加密入库） |
| GET | `/accounts/{id}` | 账户详情 |
| PATCH | `/accounts/{id}` | 更新名称/状态/凭据（换凭据后原验证结果作废） |
| DELETE | `/accounts/{id}` | 删除账户（级联删除其域名与记录） |
| POST | `/accounts/{id}/verify` | 平台连通性验证 |
| POST | `/accounts/{id}/sync` | 单账户同步 |

`provider_type` 取值见 §5.2；账户状态仅 `enabled`/`disabled`。

**域名与记录**：

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/domains?page=&page_size=&search=&account_id=&keyword=&provider=` | 分页域名列表（含账户名与平台名、记录数量） |
| GET | `/domains/{id}` | 域名详情 |
| PATCH | `/domains/{id}` | 更新本地备注（不回写平台） |
| GET | `/domains/{id}/records?page=&page_size=&...` | 记录分页列表（搜索/类型/状态过滤） |
| POST | `/domains/{id}/records` | 新建记录（回写平台） |
| PATCH | `/records/{id}` | 更新记录（host/rtype/value/ttl/line/remark/proxied/weight/priority） |
| POST | `/records/{id}/status` | 启用/停用（回写平台） |
| DELETE | `/records/{id}` | 删除记录（回写平台） |

**同步 `/sync`**：

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/sync` | 触发全量同步（异步启动，返回接受结果） |
| GET | `/sync/runs?page=` | 运行历史（分页，倒序） |
| GET | `/sync/runs/{id}` | 单次运行详情（状态/统计/错误） |
| GET | `/sync/settings` | 读取同步设置 `{interval_minutes}` |
| PUT | `/sync/settings` | 更新同步设置；间隔为 0（停用）或 ≥30，负数与 1–29 拒绝（400），返回 204 |

### 10.4 OpenAPI 契约

- 必须提供机器可读的 OpenAPI 3.0 文档（`docs/openapi.yaml`），覆盖 `/api/v1`
  全部接口：路径、方法、请求体、查询/路径参数、响应结构与状态码。
- 文档与实际行为一致、与代码解耦（独立静态文件，可单独编辑与版本管理），
  不依赖运行期自动生成。
- 文档需描述会话 Cookie 鉴权方案、统一错误响应体及错误码 → 状态码映射、
  分页参数与 `{total,page,page_size,items}` 响应结构，以及账户、域名、解析
  记录、同步运行、同步设置等可复用 schema（`$ref` 引用）。

## 11. Web 控制台规范

### 11.1 页面与导航

提供 Web 界面，至少包含：

- **登录页 / 首次设置页**：口令登录；首次启动引导设置初始口令（`/setup`）。
- **账户管理页**：可见当前页账户的平台类型、名称、状态、最近同步结果，
  可执行添加、验证、启停、删除与单账户同步，表格下方提供分页控件。
- **域名列表页**：可见当前页已同步域名及其所属平台/账户、记录数量，可进入
  域名详情，表格下方提供分页控件。
- **域名详情页（解析记录页）**：可见当前页解析记录，可创建、修改、启停、
  删除记录及按类型/关键字过滤，表格下方提供分页控件。记录字段列按域名所属
  平台展示：EdgeOne（国内/国际站）不展示「解析线路」列与输入项，改为展示
  「权重」「优先级」列（无值显示空占位），创建/修改表单仅对 EdgeOne 提供权重
  （A/AAAA/CNAME）与优先级（MX）输入，留空表示使用平台默认值（编辑时权重
  留空提交 `-1`、优先级留空提交 `0`）；火山引擎保持「解析线路」展示（展示
  平台返回值，默认 `default`），不展示权重与优先级；其他平台保持线路与
  CloudFlare 代理的既有展示。

通用界面行为：

- 内容区铺满主内容可用宽度（桌面视口无右侧大面积空白）。
- 侧边栏可折叠隐藏：点击折叠按钮收起，内容区扩展至全宽；再次点击恢复；
  折叠状态可记忆。
- 登出入口位于侧边栏底栏，与折叠/展开按钮同一行：桌面端两者并排，折叠态
  仅保留登出图标；移动端顶部栏不展示折叠按钮，仅将「登出」置于顶栏右侧；
  页头操作区不重复提供登出。
- 版权信息：登录页与首次设置页在认证卡片底部展示版权行（版权年份 + GitHub
  链接）；已登录管理页在正文内容区底部展示版权页脚，翻页/筛选不丢失；侧边栏
  （含移动端顶部栏）不展示版权文案。链接均在新标签页打开并带
  `rel="noopener noreferrer"`。
- 移动端（≤768px）顶部导航项仅展示文字、不展示图标；桌面端保持图标与文字。
- 移动端（≤768px）列表表格以卡片展示并隐藏表头，平台账户页与域名列表页的
  每个数据单元格带与桌面表头一致的字段名标签（操作列除外）；解析记录页沿用
  既有标签做法；桌面端不受影响。
- 同步历史弹窗仅在底部提供「关闭」按钮（无右上角 ✕）；弹窗在视口中水平、
  垂直双向居中；打开期间锁定背景滚动（含移动端触摸），关闭后恢复；内容超出
  可视高度时，弹窗标题、表格列标题（如「开始时间」「结果」）与底部操作区固定，
  仅内容区内部滚动。
- 所有面向用户的品牌展示文本统一为 `UniDNS`（浏览器标题、侧边栏品牌名、
  登录/设置页、文档注释等）；代码标识符（二进制名、环境变量前缀、本地存储
  键名、软件包名）不受此约束。
- 侧边栏 LOGO 背景图悬停时呈现比默认状态更浅（更亮）的色调，不得变深。
- 域名列表页的域名列与域名详情页的主机记录列在悬停单元格时于文本旁展示
  「复制」图标按钮，点击将完整域名 / 完整记录名（主机记录拼接域名，根记录
  为域名本身）写入系统剪贴板并给出成功提示；写入失败时给出失败提示，
  不静默失败。

### 11.2 分页界面

- 域名列表页与平台账户列表页在表格下方提供分页控件：展示总条数与页码导航
  （当前页高亮、可切换页码、上一页/下一页）。
- 域名详情页（解析记录页）的分页控件与列表页体验一致，且支持切换每页条数。
- 切换筛选条件或排序时回到第一页；翻页与切换每页条数仅刷新列表内容与分页
  控件，不重载整页。
- 记录总数不超过一页时仍展示总条数与每页条数选择，但不出现可翻页页码
  （上一页/下一页禁用）。
- 分页参数传递给对应列表 API（§10.2）。

### 11.3 品牌图标与可安装 PWA

- **图标资源**：系统 SHALL 提供源自登录页 brand 图形（"U" 圆角方块、蓝紫
  渐变）的图片资源，包括 PNG favicon、ICO 兼容格式与 192px / 512px 应用
  图标；页面内由 CSS 绘制的 brand 展示 SHALL 保持不变。
- **页面引用**：所有页面（登录页、首次设置页、管理页与入口跳转页）的 HTML
  head SHALL 引用 PNG favicon、`/favicon.ico` 兜底与 Web App Manifest，
  浏览器标签页显示品牌图标；直接请求 `/favicon.ico` SHALL 返回 ICO 格式的
  品牌图标。
- **Web App Manifest**：SHALL 提供 manifest，声明应用名称 `UniDNS`、192px
  与 512px 图标、主题色与背景色（取界面设计系统的固定值）、
  `display: standalone` 与 `start_url`，使控制台可被浏览器识别为可安装
  应用；安装后以声明的名称与品牌图标在独立窗口打开。
- **Service Worker 缓存**：所有页面 SHALL 注册 Service Worker；同源 GET 且
  非 `/api/` 的静态资源（页面、脚本、样式、图标、manifest）采用缓存优先
  （未命中回源并写入缓存）；`/api/` 请求与跨域请求 MUST 直连网络，既不
  返回缓存副本也不写入缓存；缓存 SHALL 按版本命名，新版本激活时 SHALL
  删除旧版本缓存。
- **降级**：Service Worker 仅在安全上下文（HTTPS 或 localhost）可用；HTTP
  部署下注册失败 MUST 静默忽略、不影响既有功能，favicon 与 manifest 仍然
  生效，PWA 降级为普通网页。

## 12. 部署形态

### 12.1 裸机 / 单二进制

编译产出单个可执行文件；默认监听 `0.0.0.0:18686`，数据目录默认工作目录下
`./data`。需备份的数据只有数据目录：`unidns.db`（SQLite，WAL 模式，热备份
建议 `sqlite3 unidns.db ".backup backup.db"`）与 `secret.key`（必须）。

控制台可直接以 HTTP 访问；若需安装为桌面/主屏应用（PWA），应经反向代理启用
HTTPS——Service Worker 仅在 HTTPS 或 localhost 下可用，HTTP 访问时自动降级
为普通网页（品牌图标与 manifest 不受影响，见 §11.3）。

### 12.2 Docker

- 发行镜像：`ghcr.io/jetsung/unidns:latest`（多架构 amd64/arm64，随版本
  标签发布），可直接拉取运行，无需本地构建。
- 源码构建：多阶段构建，前端构建（Node/pnpm，产物为静态站点）→ 静态编译
  （musl 静态链接，无 glibc 依赖）→ 运行阶段使用 distroless static、非 root
  （uid 65532），出站 HTTPS 根证书由基础镜像自带。全程 rustls、无 OpenSSL。
  提供两份等价 Dockerfile：官方源版与国内镜像源版，仅镜像源配置不同。
- 运行阶段 **不注入 `UNIDNS_LISTEN` / `UNIDNS_DATA_DIR`**：监听用默认
  `0.0.0.0:18686`，数据目录用默认工作目录下的 `./data`。工作目录为 `/app`
  且属主为运行用户，数据落在 `/app/data`；`EXPOSE 18686`，
  `VOLUME /app/data`。
- docker run 方式：`-p 18686:18686 -v <主机目录>:/app/data` 运行；
  主机目录需归属 uid 65532（`chown -R 65532:65532 ./data`）。
- docker compose 方式（推荐）：使用 `image: ghcr.io/jetsung/unidns:latest`，
  端口映射 `18686:18686`，挂载 `./data:/app/data`，
  `restart: unless-stopped`；可选 `env_file: ./.env`（`required: false`，
  需 Compose v2.24+）注入 `UNIDNS_MASTER_KEY`、`UNIDNS_ADMIN_PASSWORD`、
  `UNIDNS_SYNC_INTERVAL`、`UNIDNS_LOG`。使用本地构建镜像的 compose 见
  `docker/compose.yaml`（`unidns:dev`）。

### 12.3 前端开发模式

前端为静态站点构建产物；开发时开发服务器将 `/api` 代理到本地 `18686`
后端，无需改动后端。品牌图标、manifest 与 `sw.js` 等 PWA 资源位于前端
`public/` 目录，构建时原样复制到产物根路径；开发服务器下同样以根路径访问。
页面会注册 Service Worker，开发时若前端改动后仍加载旧静态资源，需在浏览器
开发者工具中注销 Service Worker 并清空 Cache Storage（或强制刷新）。

## 13. 参考实现（非规范性说明）

参考实现技术栈，仅供对照，不构成重实现约束：

- 后端 Rust（axum + tokio + rusqlite 连接池 + reqwest + rustls）；
  前端 Astro 5 静态站点，经 rust-embed 编译期嵌入。
- 口令哈希 Argon2id；凭据加密 AES-256-GCM；主密钥 32 字节。
- 同步引擎基于 tokio 任务 + Semaphore(3)；限流退避重试一次。
- 版本号来自构建期包管理器元数据（等价于 `CARGO_PKG_VERSION`）。
- 集成测试覆盖：认证全流程、账户 CRUD 与凭据打码、同步设置校验（0/30/负数）、
  分页（截断/超范围/过滤组合）、同步覆盖中止等。
