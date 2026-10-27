# 本地开发

工具链（rust / node / pnpm / just / prek / zensical）通过
[mise](https://mise.jdx.dev) 安装：`mise use -g rust node pnpm just prek zensical`。

## 常用命令（justfile）

| 命令 | 说明 |
|---|---|
| `just backend` | 启动后端（默认 0.0.0.0:18686，数据目录 `./data`） |
| `just frontend` | 启动 Astro dev server（4321，`/api` 代理到 18686） |
| `just dev` | 同时起前后端，Ctrl-C 一并退出 |
| `just frontend-install` | 安装前端依赖 |
| `just frontend-build` | 构建前端产物到 `frontend/dist` |
| `just build` | 发布构建（前端 + `cargo build --release`） |
| `just check` | fmt + clippy + 单元测试（与 CI 参数逐字对齐） |
| `just stop` | 停止全部本地服务 |

## 前后端联调

后端单跑（`just backend`）即可使用完整界面——前端静态资源已嵌入二进制。
要改前端时用 `just dev` 双进程模式：Astro dev server 热更新，
`/api` 请求代理到 18686 后端。**注意**：`cargo build` 经 rust-embed 在编译期
读取 `frontend/dist`，改完前端需先 `just frontend-build` 再编译后端，
否则嵌入的是旧产物。

## 提交前检查（prek）

提交钩子见 `prek.toml`：pre-commit 阶段自动格式化（`cargo fmt --all`）+
文件卫生检查；pre-push 阶段跑 clippy（`-D warnings`）与单元测试。

```bash
prek install          # 首次：写入 .git/hooks
prek run --all-files  # 手动全量跑一遍
```

## 测试

```bash
cargo test --lib      # 单元测试（45 个，秒级）
cargo test            # 含需要真实凭据的冒烟用例（默认 ignored）
```

4 个 `#[ignore]` 用例是真实平台冒烟（阿里云/DNSPod/CloudFlare 连通性验证
与端到端同步），仅用于开发调试——用真实密钥调用平台线上 API，验证适配器
签名与请求格式能通过平台校验（mock 服务器验证不了签名正确性）。普通
`cargo test` 不会执行它们；需要时设置环境变量后手动执行：

```bash
cargo test -- --ignored
```

| 环境变量 | 提供什么 |
|---|---|
| `UNIDNS_TEST_ALIYUN_AK_ID` / `UNIDNS_TEST_ALIYUN_AK_SECRET` | 阿里云 AccessKey，跑阿里云真实验证 |
| `UNIDNS_TEST_DNSPOD_SECRET_ID` / `UNIDNS_TEST_DNSPOD_SECRET_KEY` | 腾讯云密钥，跑 DNSPod 真实验证 |
| `UNIDNS_TEST_DNSPOD_SITE=intl` | 可选，走 DNSPod 国际站端点（国内外密钥体系独立） |
| `UNIDNS_TEST_CF_TOKEN` | CloudFlare API Token，跑 CF 真实验证 |

不设置对应变量时相关测试打印「未提供凭据，跳过」后通过，不报错；变量定义见
`src/providers/mod.rs` 测试模块。

## 文档站点

文档源在 `docs/`（zensical，配置见 `zensical.toml`），构建输出到 `site/`：

```bash
zensical build --clean    # 构建到 site/
zensical serve            # 本地预览（热更新）
```

推送 `docs/**` 或 `zensical.toml` 变更时，GitHub Actions（`.github/workflows/docs.yml`）
会自动构建并发布到 gh-pages / EdgeOne Pages。

## 代码结构速览

新增平台适配器：在 `src/providers/` 新建实现 `DnsProvider` trait 的模块，
在 `ProviderType` 枚举、CHECK 约束（迁移）与 `build_provider` 工厂注册；
凭据 JSON 字段加入 `web/accounts.rs` 的必填校验列表。
其余结构见[架构设计](01-architecture.md)。
