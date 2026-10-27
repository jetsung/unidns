# unidns 本地开发任务。
#
# 工具链（rust / node / pnpm / prek 等）通过 mise 安装；提交前钩子见 prek.toml。
# 注意：cargo build 会经 rust-embed 在编译期嵌入 frontend/dist，改完前端先 `just frontend-build`。

# 默认：列出全部任务
default:
    @just --list

# ---- 后端（默认监听 0.0.0.0:18686，数据目录 ./data）----

# 启动后端开发服务（Web 界面：http://127.0.0.1:18686）
# 可附带参数，例如：just backend --listen 127.0.0.1:18686
[group('backend')]
@backend *args:
    cargo run -- {{ args }}

# 构建发布版（先构建前端产物，rust-embed 编译期需要 frontend/dist）
[group('backend')]
@build:
    pnpm -C frontend build
    cargo build --release

# ---- 前端（frontend/，Astro 5）----

# 安装前端依赖（首次使用或 pnpm-lock.yaml 变更后）
[group('frontend')]
@frontend-install:
    pnpm -C frontend install

# exec 去掉一层 shell 包装，减少信号转发丢失的环节
# 启动前端 dev server（http://localhost:4321，/api 代理到 18686）
[group('frontend')]
@frontend:
    exec pnpm -C frontend dev

# 构建前端静态产物到 frontend/dist（rust-embed 编译期依赖）
[group('frontend')]
@frontend-build:
    pnpm -C frontend build

# ---- 检查（与 prek.toml / CI 参数逐字对齐）----

# 全量检查：fmt + clippy + 单元测试（提交/推送前 prek 会自动跑）
[group('check')]
@check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --lib

# ---- 文档（zensical，源 docs/，产物 site/）----

# 构建文档站点到 site/（--clean 先清理旧产物）
# 可附带参数，例如：just docs-build --strict
[group('docs')]
@docs-build *args:
    zensical build --clean {{ args }}

# 本地调试预览文档站点（http://127.0.0.1:8000，文件变更自动重建）
# 可附带参数，例如：just docs-serve --dev-addr 127.0.0.1:8001
[group('docs')]
@docs-serve *args:
    zensical serve {{ args }}

# ---- 组合 ----

# 同时启动后端与前端本地调试（Ctrl-C 一并退出；任一退出则整体结束）
# 用 setsid 让每个子任务成为独立进程组长，退出时 kill -PID 整组击杀；
# wait -n 在任一子任务退出后触发 cleanup 整组击杀，避免 pnpm/astro 被孤儿化。
[group('dev')]
dev:
    #!/usr/bin/env bash
    set -euo pipefail
    pids=()
    cleanup() {
        for p in "${pids[@]}"; do
            kill -- -"$p" 2>/dev/null || true
        done
    }
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    setsid just backend & pids+=("$!")
    setsid just frontend & pids+=("$!")
    wait -n

# 匹配串用 [x] 写法避免 pkill 命中本进程自身。
# 停止所有本地开发服务（后端 18686 与前端 432x 的 astro/pnpm/cargo 进程）
[group('dev')]
@stop:
    pkill -TERM -f 'node .*[a]stro' 2>/dev/null || true
    pkill -TERM -f 'pnpm -C [f]rontend' 2>/dev/null || true
    pkill -TERM -f 'unidn[s]' 2>/dev/null || true
    pkill -TERM -f 'zensica[l]' 2>/dev/null || true
    pkill -TERM -f 'just (de[v]|fronten[d]|backen[d]|docs-[a-z]+)' 2>/dev/null || true
    sleep 1
    @echo "本地开发服务已停止"
