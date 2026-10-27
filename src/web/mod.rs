use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{Router, middleware};
use sqlx::SqlitePool;
use tower_http::trace::TraceLayer;
use tower_sessions::{Expiry, MemoryStore, Session, SessionManagerLayer, cookie::SameSite};

use crate::{error::AppError, providers::ProviderFactory};

pub mod accounts;
pub mod auth;
pub mod records;
pub mod sync_api;

/// 会话标记：值为 true 表示已登录管理员。
pub const SESSION_ADMIN_KEY: &str = "admin";

/// 统一 JSON 提取器：反序列化失败映射为 400 + 统一错误体（而非 axum 默认 422 文本）。
pub struct ApiJson<T>(pub T);

impl<S, T> axum::extract::FromRequest<S> for ApiJson<T>
where
    axum::Json<T>:
        axum::extract::FromRequest<S, Rejection = axum::extract::rejection::JsonRejection>,
    S: Send + Sync,
{
    type Rejection = crate::error::AppError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(ApiJson(v)),
            Err(e) => Err(crate::error::AppError::BadRequest(format!(
                "请求体格式错误: {e}"
            ))),
        }
    }
}

/// 正在执行的同步任务表：账户 id -> (任务句柄, run_id)（同账户再次触发时覆盖中止）。
pub type SyncRunningMap = std::collections::HashMap<i64, (tokio::task::JoinHandle<()>, i64)>;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub master_key: Arc<[u8; 32]>,
    pub provider_factory: ProviderFactory,
    pub sync_running: Arc<tokio::sync::Mutex<SyncRunningMap>>,
}

pub type SharedState = Arc<AppState>;

/// 免认证 API（登录引导相关）。
const PUBLIC_API_PATHS: [&str; 3] = [
    "/api/v1/auth/status",
    "/api/v1/auth/login",
    "/api/v1/auth/setup",
];

/// 会话认证中间件：应用在 /api 路由外层，可见完整请求路径；
/// /api/v1/*（白名单除外）一律要求已登录会话。
async fn require_auth(
    session: Session,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, AppError> {
    let path = req.uri().path();
    if path.starts_with("/api/") && !PUBLIC_API_PATHS.contains(&path) {
        let ok = session
            .get::<bool>(SESSION_ADMIN_KEY)
            .await
            .ok()
            .flatten()
            .unwrap_or(false);
        if !ok {
            return Err(AppError::Unauthorized);
        }
    }
    Ok(next.run(req).await)
}

pub fn session_layer() -> SessionManagerLayer<MemoryStore> {
    let store = MemoryStore::default();
    SessionManagerLayer::new(store)
        .with_secure(false) // 自托管常见反向代理终止 TLS，允许 HTTP 访问
        .with_http_only(true)
        .with_same_site(SameSite::Lax)
        .with_expiry(Expiry::OnInactivity(time::Duration::seconds(24 * 3600)))
}

pub fn router(state: SharedState) -> Router {
    let api = Router::new()
        .route("/auth/status", axum::routing::get(auth::status))
        .route("/auth/login", axum::routing::post(auth::login))
        .route("/auth/setup", axum::routing::post(auth::setup))
        .route("/auth/me", axum::routing::get(auth::me))
        .route("/auth/logout", axum::routing::post(auth::logout))
        .route("/auth/password", axum::routing::post(auth::change_password))
        .route(
            "/accounts",
            axum::routing::get(accounts::list).post(accounts::create),
        )
        .route(
            "/accounts/{id}",
            axum::routing::get(accounts::get_one)
                .patch(accounts::update)
                .delete(accounts::delete),
        )
        .route(
            "/accounts/{id}/verify",
            axum::routing::post(accounts::verify),
        )
        .route(
            "/accounts/{id}/sync",
            axum::routing::post(sync_api::sync_one),
        )
        .route("/domains", axum::routing::get(records::list_domains))
        .route(
            "/domains/{id}",
            axum::routing::get(records::get_domain).patch(records::update_domain),
        )
        .route(
            "/domains/{id}/records",
            axum::routing::get(records::list_records).post(records::create_record),
        )
        .route(
            "/records/{id}",
            axum::routing::patch(records::update_record).delete(records::delete_record),
        )
        .route(
            "/records/{id}/status",
            axum::routing::post(records::set_status),
        )
        .route("/sync", axum::routing::post(sync_api::sync_now))
        .route("/sync/runs", axum::routing::get(sync_api::list_runs))
        .route("/sync/runs/{id}", axum::routing::get(sync_api::get_run))
        .route(
            "/sync/settings",
            axum::routing::get(sync_api::get_settings).put(sync_api::update_settings),
        )
        .with_state(state);

    Router::new()
        .route("/", axum::routing::get(crate::frontend::index))
        .route("/setup", axum::routing::get(crate::frontend::setup))
        .route("/login", axum::routing::get(crate::frontend::login))
        .route("/accounts", axum::routing::get(crate::frontend::accounts))
        .route("/domains", axum::routing::get(crate::frontend::domains))
        .route(
            "/domains/detail",
            axum::routing::get(crate::frontend::domain_detail),
        )
        .fallback(crate::frontend::static_fallback)
        .nest("/api/v1", api)
        .layer(middleware::from_fn(require_auth))
        .layer(session_layer())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &axum::http::Request<_>| {
                    tracing::info_span!(
                        "http_request",
                        method = %req.method(),
                        path = %req.uri().path()
                    )
                })
                .on_response(
                    |res: &axum::response::Response, latency: Duration, _span: &tracing::Span| {
                        tracing::info!(
                            status = %res.status().as_u16(),
                            latency_ms = latency.as_millis() as u64,
                            "请求完成"
                        );
                    },
                ),
        )
}

pub async fn serve(state: SharedState, listen: &str) -> Result<(), Box<dyn std::error::Error>> {
    let app = router(state);
    let addr: SocketAddr = listen.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("unidns 正在监听 http://{listen}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// 供集成测试复用：空 SQLite 内存库构造的 AppState（默认真实工厂，认证测试不触发平台调用）。
#[cfg(test)]
pub(crate) async fn test_state() -> SharedState {
    let pool = crate::db::init_pool("sqlite::memory:")
        .await
        .expect("测试库初始化失败");
    Arc::new(AppState {
        pool,
        master_key: Arc::new([9u8; 32]),
        provider_factory: crate::providers::default_provider_factory(),
        sync_running: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    })
}

/// 带 MockProvider 的测试环境：返回 AppState 与 mock 句柄。
#[cfg(test)]
pub(crate) async fn test_app_with_mock() -> (SharedState, crate::providers::mock::MockProvider) {
    let (state, mut mocks) = test_app_with_multi_mock().await;
    let mock = mocks.remove("a").expect("默认 mock 槽位");
    (state, mock)
}

/// 多 MockProvider 测试环境：凭据 JSON 中的 "mock_id" 字段决定工厂返回哪个 mock。
#[cfg(test)]
pub(crate) async fn test_app_with_multi_mock() -> (
    SharedState,
    std::collections::HashMap<String, crate::providers::mock::MockProvider>,
) {
    use std::collections::HashMap;
    let pool = crate::db::init_pool("sqlite::memory:")
        .await
        .expect("测试库初始化失败");
    let mocks: HashMap<String, crate::providers::mock::MockProvider> = ["a", "b"]
        .iter()
        .map(|k| (k.to_string(), Default::default()))
        .collect();
    let mocks_for_factory = mocks.clone();
    let state = Arc::new(AppState {
        pool,
        master_key: Arc::new([9u8; 32]),
        sync_running: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        provider_factory: Arc::new(move |_t, cred| {
            let id = serde_json::from_str::<serde_json::Value>(cred)
                .ok()
                .and_then(|v| v.get("mock_id").and_then(|m| m.as_str()).map(String::from))
                .unwrap_or_else(|| "a".into());
            let mock = mocks_for_factory
                .get(&id)
                .cloned()
                .unwrap_or_else(|| mocks_for_factory["a"].clone());
            Ok(Box::new(mock) as Box<dyn crate::providers::DnsProvider>)
        }),
    });
    (state, mocks)
}

/// 测试辅助：在传入的 Router 上完成口令初始化并登录，返回会话 cookie。
/// 必须与后续请求使用同一个 Router（Router Clone 共享会话存储）。
#[cfg(test)]
pub(crate) async fn login_for_test(app: &axum::Router) -> String {
    use tower::ServiceExt;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/auth/setup")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"password":"password123"}"#))
        .unwrap();
    let res = app.clone().oneshot(req).await.expect("setup 请求失败");
    assert_eq!(res.status(), 204, "测试登录初始化失败");
    res.headers()
        .get_all("set-cookie")
        .iter()
        .find_map(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next().map(String::from))
        .expect("setup 应返回会话 cookie")
}
