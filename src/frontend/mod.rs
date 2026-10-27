use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// Astro 构建产物（frontend/dist），构建命令：`pnpm build`（见 frontend/）。
#[derive(RustEmbed)]
#[folder = "frontend/dist"]
struct Assets;

/// 多页静态路由 → dist 内文件。
fn resolve(path: &str) -> Option<&'static str> {
    match path {
        "/" => Some("index.html"),
        "/setup" => Some("setup/index.html"),
        "/login" => Some("login/index.html"),
        "/accounts" => Some("accounts/index.html"),
        "/domains" => Some("domains/index.html"),
        "/domains/detail" => Some("domains/detail/index.html"),
        _ => None,
    }
}

/// 按嵌入路径返回静态资源（含 MIME 类型推断）。
fn serve_embedded(path: &str) -> Response {
    match Assets::get(path) {
        Some(file) => {
            let mime = file.metadata.mimetype().to_string();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-cache".to_string()),
                ],
                file.data,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "Not Found").into_response(),
    }
}

fn html(path: &'static str) -> Response {
    serve_embedded(path)
}

pub async fn index() -> Response {
    html(resolve("/").unwrap())
}

pub async fn setup() -> Response {
    html(resolve("/setup").unwrap())
}

pub async fn login() -> Response {
    html(resolve("/login").unwrap())
}

pub async fn accounts() -> Response {
    html(resolve("/accounts").unwrap())
}

pub async fn domains() -> Response {
    html(resolve("/domains").unwrap())
}

pub async fn domain_detail() -> Response {
    html(resolve("/domains/detail").unwrap())
}

/// 静态资源兜底：未被页面路由匹配的路径按 dist 内相对路径查找
/// （如 /_astro/app.js → _astro/app.js），找不到返回 404。
/// Astro 产物文件名均为 ASCII hash，无需 percent 解码。
pub async fn static_fallback(req: axum::extract::Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    serve_embedded(path)
}
