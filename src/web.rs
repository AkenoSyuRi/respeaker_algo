//! 常驻本地 Web 管理服务：静态资源、REST、SSE 和安全录音文件响应。

use std::convert::Infallible;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::StreamExt as FuturesStreamExt;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot};
use tokio_stream::wrappers::BroadcastStream;

use crate::app_config::AppConfig;
use crate::controller::ControllerHandle;
use crate::events::{EventBus, ServicePhase, ServiceSnapshot};
use crate::recordings;

const DEFAULT_BIND: &str = "127.0.0.1:8765";
const DEFAULT_MAX_CLIENTS: usize = 8;
const INDEX_HTML: &str = include_str!("../web/index.html");
const STYLES_CSS: &str = include_str!("../web/styles.css");
const APP_JS: &str = include_str!("../web/app.js");

pub struct WebServerHandle {
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<Result<(), String>>>,
    #[cfg(test)]
    pub(crate) local_addr: SocketAddr,
}

#[derive(Clone)]
struct WebState {
    controller: ControllerHandle,
    events: EventBus,
    active_clients: Arc<AtomicUsize>,
    max_clients: usize,
    shutdown: broadcast::Sender<()>,
}

#[derive(Clone, Copy)]
pub(crate) struct WebOptions {
    pub(crate) bind: SocketAddr,
    pub(crate) max_clients: usize,
    pub(crate) open_browser: bool,
}

struct ClientGuard {
    active_clients: Arc<AtomicUsize>,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.active_clients.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Serialize)]
struct ApiErrorBody {
    error: ApiError,
}

#[derive(Serialize)]
struct ApiError {
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
}

fn api_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(ApiErrorBody {
            error: ApiError {
                code: code.to_string(),
                message: message.into(),
                field: None,
            },
        }),
    )
        .into_response()
}

fn map_error(error: String) -> Response {
    let status = if error.starts_with("conflict:")
        || error.contains("目标文件已存在")
        || error.contains("目标已存在")
    {
        StatusCode::CONFLICT
    } else if error.contains("不存在")
        || error.contains("文件不存在")
        || error.contains("文件不属于")
    {
        StatusCode::NOT_FOUND
    } else if error.contains("配置") || error.contains("profile") || error.contains("prefix") {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    api_error(
        status,
        "request_failed",
        error.trim_start_matches("conflict:"),
    )
}

async fn controller_call<T, F>(controller: ControllerHandle, operation: F) -> Result<T, Response>
where
    T: Send + 'static,
    F: FnOnce(ControllerHandle) -> T + Send + 'static,
{
    tokio::task::spawn_blocking(move || operation(controller))
        .await
        .map_err(|error| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "controller_failed",
                error.to_string(),
            )
        })
}

async fn file_call<T, F>(operation: F) -> Result<T, Response>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "file_operation_failed",
                error.to_string(),
            )
        })
}

pub fn start(controller: ControllerHandle, events: EventBus) -> Result<WebServerHandle, String> {
    let bind = DEFAULT_BIND
        .parse()
        .map_err(|e| format!("解析 Web 地址 {DEFAULT_BIND} 失败: {e}"))?;
    start_with_options(
        controller,
        events,
        WebOptions {
            bind,
            max_clients: DEFAULT_MAX_CLIENTS,
            open_browser: !cfg!(test),
        },
    )
}

pub(crate) fn start_with_options(
    controller: ControllerHandle,
    events: EventBus,
    options: WebOptions,
) -> Result<WebServerHandle, String> {
    if options.max_clients == 0 {
        return Err("Web 最大客户端数必须大于 0".into());
    }
    let listener = StdTcpListener::bind(options.bind)
        .map_err(|e| format!("绑定 Web 地址 {} 失败: {e}", options.bind))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("设置 Web 监听器为非阻塞失败: {e}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| format!("读取 Web 本地地址失败: {e}"))?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let join = thread::Builder::new()
        .name("app-web".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("创建 Web 异步运行时失败: {e}"))?;
            runtime.block_on(run_server(
                listener,
                controller,
                events,
                options.max_clients,
                shutdown_rx,
            ))
        })
        .map_err(|e| format!("启动 Web 服务线程失败: {e}"))?;
    let url = format!("http://{local_addr}");
    eprintln!("[web] 管理界面: {url}");
    if options.open_browser
        && let Err(error) = open_default_browser(&url)
    {
        eprintln!("[web] 打开默认浏览器失败: {error}");
    }
    Ok(WebServerHandle {
        shutdown: Some(shutdown_tx),
        join: Some(join),
        #[cfg(test)]
        local_addr,
    })
}

impl WebServerHandle {
    pub fn shutdown(&mut self) -> Result<(), String> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| "Web 服务线程异常退出".to_string())??;
        }
        Ok(())
    }
}

impl Drop for WebServerHandle {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("[web] 关闭服务失败: {error}");
        }
    }
}

async fn run_server(
    listener: StdTcpListener,
    controller: ControllerHandle,
    events: EventBus,
    max_clients: usize,
    shutdown: oneshot::Receiver<()>,
) -> Result<(), String> {
    let listener =
        TcpListener::from_std(listener).map_err(|e| format!("创建 Web Tokio 监听器失败: {e}"))?;
    let (client_shutdown, _) = broadcast::channel(1);
    let state = WebState {
        controller,
        events,
        active_clients: Arc::new(AtomicUsize::new(0)),
        max_clients,
        shutdown: client_shutdown.clone(),
    };
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/styles.css", get(styles_handler))
        .route("/app.js", get(script_handler))
        .route("/api/status", get(status_handler))
        .route(
            "/api/config",
            get(config_handler).put(update_config_handler),
        )
        .route("/api/config/save", post(save_config_handler))
        .route("/api/config/reset", post(reset_config_handler))
        .route("/api/config/import", post(import_config_handler))
        .route("/api/config/export", get(export_config_handler))
        .route("/api/profiles", get(list_profiles_handler))
        .route(
            "/api/profiles/:name",
            get(load_profile_handler)
                .put(save_profile_handler)
                .delete(delete_profile_handler),
        )
        .route("/api/recordings/start", post(start_handler))
        .route("/api/recordings/stop", post(stop_handler))
        .route("/api/recordings", get(list_recordings_handler))
        .route("/api/recordings/:id", get(recording_handler))
        .route("/api/recordings/:id/files/:kind", get(file_handler))
        .route("/api/recordings/:id/trash", post(trash_handler))
        .route("/api/recordings/:id/restore", post(restore_handler))
        .route("/api/events", get(sse_handler))
        // 保留旧地址作为同源重定向兼容，不再由 Pipeline 拥有。
        .route("/events", get(sse_handler))
        .with_state(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.await;
            let _ = client_shutdown.send(());
        })
        .await
        .map_err(|e| format!("Web 服务失败: {e}"))
}

async fn index_handler() -> Html<String> {
    Html(INDEX_HTML.replace("{{APP_VERSION}}", env!("CARGO_PKG_VERSION")))
}

async fn styles_handler() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLES_CSS,
    )
}

async fn script_handler() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        APP_JS,
    )
}

async fn status_handler(State(state): State<WebState>) -> Json<ServiceSnapshot> {
    state.controller.refresh_device();
    Json(state.controller.snapshot())
}

async fn config_handler(State(state): State<WebState>) -> Json<Value> {
    Json(serde_json::json!({
        "draft": state.controller.config(),
        "config_warning": state.controller.snapshot().config_warning,
    }))
}

async fn update_config_handler(State(state): State<WebState>, body: String) -> Response {
    let config = match serde_json::from_str::<AppConfig>(&body) {
        Ok(config) => config,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, "invalid_json", error.to_string()),
    };
    let draft = config.clone();
    let result = match controller_call(state.controller, move |controller| {
        controller.update_config(config).map(|()| draft)
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    match result {
        Ok(config) => Json(serde_json::json!({"draft": config})).into_response(),
        Err(error) => map_error(error),
    }
}

async fn save_config_handler(State(state): State<WebState>) -> Response {
    let result =
        match controller_call(state.controller, |controller| controller.save_config()).await {
            Ok(result) => result,
            Err(response) => return response,
        };
    match result {
        Ok(()) => Json(serde_json::json!({"saved": true})).into_response(),
        Err(error) => map_error(error),
    }
}

async fn reset_config_handler(State(state): State<WebState>) -> Response {
    let controller = state.controller;
    let result =
        match controller_call(controller.clone(), |controller| controller.reset_config()).await {
            Ok(result) => result,
            Err(response) => return response,
        };
    match result {
        Ok(()) => Json(serde_json::json!({"draft": controller.config()})).into_response(),
        Err(error) => map_error(error),
    }
}

async fn import_config_handler(State(state): State<WebState>, body: String) -> Response {
    let result = match controller_call(state.controller, move |controller| {
        controller.import_config(body)
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    match result {
        Ok(config) => Json(serde_json::json!({"draft": config})).into_response(),
        Err(error) => api_error(StatusCode::BAD_REQUEST, "invalid_config", error),
    }
}

async fn export_config_handler(State(state): State<WebState>) -> Response {
    match state.controller.config().to_toml_string() {
        Ok(text) => (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/toml; charset=utf-8"),
                ),
                (
                    header::CONTENT_DISPOSITION,
                    HeaderValue::from_static("attachment; filename=app-config.toml"),
                ),
            ],
            text,
        )
            .into_response(),
        Err(error) => map_error(error),
    }
}

async fn list_profiles_handler(State(state): State<WebState>) -> Response {
    let result =
        match controller_call(state.controller, |controller| controller.list_profiles()).await {
            Ok(result) => result,
            Err(response) => return response,
        };
    match result {
        Ok(names) => Json(serde_json::json!({"profiles": names})).into_response(),
        Err(error) => map_error(error),
    }
}

async fn load_profile_handler(
    State(state): State<WebState>,
    AxumPath(name): AxumPath<String>,
) -> Response {
    let result = match controller_call(state.controller, move |controller| {
        controller.load_profile(name)
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    match result {
        Ok(config) => Json(serde_json::json!({"draft": config})).into_response(),
        Err(error) => map_error(error),
    }
}

async fn save_profile_handler(
    State(state): State<WebState>,
    AxumPath(name): AxumPath<String>,
    body: String,
) -> Response {
    let config = match serde_json::from_str::<AppConfig>(&body) {
        Ok(config) => config,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, "invalid_json", error.to_string()),
    };
    let result = match controller_call(state.controller, move |controller| {
        controller.save_profile(name, Some(config))
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    match result {
        Ok(()) => Json(serde_json::json!({"saved": true})).into_response(),
        Err(error) => map_error(error),
    }
}

async fn delete_profile_handler(
    State(state): State<WebState>,
    AxumPath(name): AxumPath<String>,
) -> Response {
    let result = match controller_call(state.controller, move |controller| {
        controller.delete_profile(name)
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => map_error(error),
    }
}

async fn start_handler(State(state): State<WebState>) -> Response {
    let result = match controller_call(state.controller, |controller| controller.start()).await {
        Ok(result) => result,
        Err(response) => return response,
    };
    match result {
        Ok(session_id) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"session_id": session_id, "status": "starting"})),
        )
            .into_response(),
        Err(error) => map_error(error),
    }
}

async fn stop_handler(State(state): State<WebState>) -> Response {
    let result = match controller_call(state.controller, |controller| controller.stop()).await {
        Ok(result) => result,
        Err(response) => return response,
    };
    match result {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"status": "stopping"})),
        )
            .into_response(),
        Err(error) => map_error(error),
    }
}

async fn list_recordings_handler(State(state): State<WebState>) -> Response {
    let out_dir = state.controller.config().recording.out_dir;
    let active_session_id = active_session_id(&state.controller.snapshot());
    let result = match file_call(move || {
        serde_json::json!({
            "recordings": recordings::scan_catalog_with_active(&out_dir, active_session_id.as_deref()),
            "trash": recordings::scan_trash_catalog(&out_dir),
        })
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    Json(result).into_response()
}

async fn recording_handler(
    State(state): State<WebState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let out_dir = state.controller.config().recording.out_dir;
    let active = active_session_id(&state.controller.snapshot());
    let result = match file_call(move || {
        recordings::scan_catalog_with_active(&out_dir, active.as_deref())
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| "session 不存在".to_string())
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    match result {
        Ok(entry) => Json(entry).into_response(),
        Err(error) => map_error(error),
    }
}

async fn file_handler(
    State(state): State<WebState>,
    AxumPath((id, kind)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let out_dir = state.controller.config().recording.out_dir;
    let requested_kind = kind.clone();
    let range_header = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let result = match file_call(move || {
        let entry = recordings::find_entry(&out_dir, &id)?;
        if !entry
            .files
            .iter()
            .any(|file| file.kind == requested_kind && file.exists)
        {
            return Err("文件不属于该 session".to_string());
        }
        let path = recordings::resolve_file_path(&out_dir, &entry.prefix, &requested_kind)?;
        let total = std::fs::metadata(&path)
            .map_err(|error| format!("读取文件元数据失败: {error}"))?
            .len();
        Ok((path, total))
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    let (path, total) = match result {
        Ok(result) => result,
        Err(error) => return map_error(error),
    };
    let range = match parse_range(total, range_header.as_deref()) {
        Ok(range) => range,
        Err(()) => {
            return api_error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "invalid_range",
                "Range 无效",
            );
        }
    };
    stream_file_response(path, &kind, range).await
}

async fn trash_handler(State(state): State<WebState>, AxumPath(id): AxumPath<String>) -> Response {
    if active_session_id(&state.controller.snapshot()).as_deref() == Some(id.as_str()) {
        return api_error(
            StatusCode::CONFLICT,
            "session_active",
            "当前 session 正在录音或收尾，不能移入回收站",
        );
    }
    let out_dir = state.controller.config().recording.out_dir;
    let result = match file_call(move || {
        let entry = recordings::find_entry(&out_dir, &id)?;
        recordings::trash_session(&out_dir, &entry.id, &entry.prefix)
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    match result {
        Ok(()) => Json(serde_json::json!({"trashed": true})).into_response(),
        Err(error) => map_error(error),
    }
}

async fn restore_handler(
    State(state): State<WebState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let out_dir = state.controller.config().recording.out_dir;
    let result = match file_call(move || {
        let entry = recordings::find_trashed_entry(&out_dir, &id)?;
        recordings::restore_session(&out_dir, &entry.id, &entry.prefix)
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    match result {
        Ok(()) => Json(serde_json::json!({"restored": true})).into_response(),
        Err(error) => map_error(error),
    }
}

async fn sse_handler(State(state): State<WebState>) -> Response {
    if !try_acquire_client(&state.active_clients, state.max_clients) {
        return api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "client_limit",
            "SSE 客户端数量已达上限",
        );
    }
    let guard = Arc::new(ClientGuard {
        active_clients: Arc::clone(&state.active_clients),
    });
    let receiver = state.events.subscribe();
    let initial = state.events.snapshot_envelope();
    let initial_event = Event::default().event("state_snapshot").data(initial);
    let events = state.events.clone();
    let live =
        tokio_stream::StreamExt::filter_map(BroadcastStream::new(receiver), move |message| {
            let events = events.clone();
            match message {
                Ok(json) => {
                    let kind = serde_json::from_str::<Value>(&json)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("type")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "message".into());
                    Some(Ok::<Event, Infallible>(
                        Event::default().event(kind).data(json),
                    ))
                }
                Err(_) => Some(Ok::<Event, Infallible>(snapshot_event(&events))),
            }
        });
    let stream = tokio_stream::StreamExt::map(
        tokio_stream::StreamExt::chain(
            tokio_stream::iter([Ok::<Event, Infallible>(initial_event)]),
            live,
        ),
        move |event| {
            let _keep_guard = &guard;
            event
        },
    );
    let mut shutdown = state.shutdown.subscribe();
    let stream = FuturesStreamExt::take_until(stream, async move {
        let _ = shutdown.recv().await;
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn snapshot_event(events: &EventBus) -> Event {
    Event::default()
        .event("state_snapshot")
        .data(events.snapshot_envelope())
}

fn active_session_id(snapshot: &ServiceSnapshot) -> Option<String> {
    match &snapshot.phase {
        ServicePhase::Starting { session_id }
        | ServicePhase::Recording { session_id }
        | ServicePhase::Stopping { session_id } => Some(session_id.clone()),
        ServicePhase::Idle => None,
    }
}

#[derive(Debug)]
struct FileRange {
    status: StatusCode,
    start: u64,
    end: u64,
    total: u64,
}

fn parse_range(total: u64, range: Option<&str>) -> Result<FileRange, ()> {
    let Some(value) = range else {
        return Ok(FileRange {
            status: StatusCode::OK,
            start: 0,
            end: total.saturating_sub(1),
            total,
        });
    };
    let invalid = || Err(());
    if !value.starts_with("bytes=") || value[6..].contains(',') || total == 0 {
        return invalid();
    }
    let (start_text, end_text) = value[6..].split_once('-').ok_or(())?;
    let (start, end) = if start_text.is_empty() {
        let suffix = end_text.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return invalid();
        }
        (total.saturating_sub(suffix), total - 1)
    } else {
        let start = start_text.parse::<u64>().map_err(|_| ())?;
        if start >= total {
            return invalid();
        }
        let end = if end_text.is_empty() {
            total - 1
        } else {
            end_text.parse::<u64>().map_err(|_| ())?.min(total - 1)
        };
        (start, end)
    };
    if end < start {
        return invalid();
    }
    Ok(FileRange {
        status: StatusCode::PARTIAL_CONTENT,
        start,
        end,
        total,
    })
}

async fn stream_file_response(path: std::path::PathBuf, kind: &str, range: FileRange) -> Response {
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "file_read_failed",
                error.to_string(),
            );
        }
    };
    if let Err(error) = file.seek(SeekFrom::Start(range.start)).await {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "file_seek_failed",
            error.to_string(),
        );
    }
    let remaining = range.end.saturating_sub(range.start) + u64::from(range.total > 0);
    let stream =
        futures_util::stream::unfold((file, remaining), |(mut file, remaining)| async move {
            if remaining == 0 {
                return None;
            }
            let mut buffer = vec![0u8; remaining.min(64 * 1024) as usize];
            match file.read(&mut buffer).await {
                Ok(0) => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "文件提前结束",
                    )),
                    (file, 0),
                )),
                Ok(read) => {
                    buffer.truncate(read);
                    Some((
                        Ok::<Bytes, std::io::Error>(Bytes::from(buffer)),
                        (file, remaining - read as u64),
                    ))
                }
                Err(error) => Some((Err(error), (file, 0))),
            }
        });
    let content_type = match kind {
        "doa_csv" => "text/csv; charset=utf-8",
        "manifest" => "application/json; charset=utf-8",
        _ => "audio/wav",
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = range.status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let content_length = range.end.saturating_sub(range.start) + u64::from(range.total > 0);
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&content_length.to_string()).unwrap(),
    );
    if range.status == StatusCode::PARTIAL_CONTENT {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!(
                "bytes {}-{}/{}",
                range.start, range.end, range.total
            ))
            .unwrap(),
        );
    }
    response
}

fn try_acquire_client(active_clients: &AtomicUsize, max_clients: usize) -> bool {
    let mut current = active_clients.load(Ordering::Relaxed);
    loop {
        if current >= max_clients {
            return false;
        }
        match active_clients.compare_exchange_weak(
            current,
            current + 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(next) => current = next,
        }
    }
}

fn open_default_browser(url: &str) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new("cmd")
        .args(["/C", "start", "", url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Mutex;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    static CONFIG_DIR_LOCK: Mutex<()> = Mutex::new(());

    fn test_server() -> (WebServerHandle, ControllerHandle) {
        let bus = EventBus::new(32);
        let controller = crate::controller::spawn(AppConfig::default(), None, bus.clone()).unwrap();
        let server = start_with_options(
            controller.clone(),
            bus,
            WebOptions {
                bind: "127.0.0.1:0".parse().unwrap(),
                max_clients: 1,
                open_browser: false,
            },
        )
        .unwrap();
        (server, controller)
    }

    #[test]
    fn serves_home_static_status_and_snapshot_sse() {
        let (mut server, controller) = test_server();
        let home = read_http(server.local_addr, "/", "ReSpeaker Audio Manager");
        assert!(home.contains("200 OK"));
        let css = read_http(server.local_addr, "/styles.css", "--ink");
        assert!(css.contains("200 OK"));
        let script = read_http(server.local_addr, "/app.js", "phaseName !== \"idle\"");
        assert!(script.contains("phaseName === \"idle\""));
        let status = read_http(server.local_addr, "/api/status", "\"phase\"");
        assert!(status.contains("200 OK"));
        let sse = read_http(server.local_addr, "/api/events", "state_snapshot");
        assert!(sse.contains("200 OK"));
        assert!(sse.contains("state_snapshot"));
        controller.shutdown().unwrap();
        server.shutdown().unwrap();
    }

    #[test]
    fn rejects_clients_over_limit() {
        let (mut server, controller) = test_server();
        let first = open_sse(server.local_addr);
        let second = open_sse_status(server.local_addr);
        assert!(second.contains("429 Too Many Requests"));
        drop(first);
        controller.shutdown().unwrap();
        server.shutdown().unwrap();
    }

    #[test]
    fn range_parser_handles_partial_suffix_and_reverse_ranges() {
        let range = parse_range(6, Some("bytes=1-3")).unwrap();
        assert_eq!(range.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!((range.start, range.end), (1, 3));
        let suffix = parse_range(6, Some("bytes=-2")).unwrap();
        assert_eq!((suffix.start, suffix.end), (4, 5));
        assert!(parse_range(6, Some("bytes=5-3")).is_err());
        for malformed in ["bytes=abc-2", "bytes=1-abc", "bytes=-", "bytes=1-2-3"] {
            assert!(parse_range(6, Some(malformed)).is_err(), "{malformed}");
        }
    }

    #[test]
    fn config_profile_crud_and_path_traversal_use_http_contracts() {
        let _guard = CONFIG_DIR_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "respeaker_web_profiles_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after epoch")
                .as_nanos()
        ));
        crate::app_config::set_test_config_dir(Some(dir.clone()));
        let (mut server, controller) = test_server();
        let config = serde_json::to_string(&AppConfig::default()).unwrap();

        let update = request_http(
            server.local_addr,
            "PUT",
            "/api/config",
            "application/json",
            &config,
            "200 OK",
        );
        assert!(update.contains("\"pipeline_enabled\":false"));
        let save = request_http(
            server.local_addr,
            "PUT",
            "/api/profiles/review_profile",
            "application/json",
            &config,
            "200 OK",
        );
        assert!(save.contains("\"saved\":true"));
        let load = read_http(
            server.local_addr,
            "/api/profiles/review_profile",
            "\"draft\"",
        );
        assert!(load.contains("200 OK"));
        let list = read_http(server.local_addr, "/api/profiles", "review_profile");
        assert!(list.contains("200 OK"));
        let delete = request_http(
            server.local_addr,
            "DELETE",
            "/api/profiles/review_profile",
            "application/json",
            "",
            "204 No Content",
        );
        assert!(delete.contains("204 No Content"));

        for path in [
            "/api/profiles/%2E%2E%2Fevil",
            "/api/recordings/%2E%2E%2Fevil",
            "/api/recordings/fake/files/%2E%2E%2Fmanifest",
        ] {
            let response = request_http(
                server.local_addr,
                "GET",
                path,
                "application/json",
                "",
                "HTTP/1.1",
            );
            assert!(!response.contains("200 OK"), "{path}: {response}");
        }

        controller.shutdown().unwrap();
        server.shutdown().unwrap();
        crate::app_config::set_test_config_dir(None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn start_and_stop_conflicts_are_reported_over_http() {
        let (finish_tx, finish_rx) = std::sync::mpsc::channel::<()>();
        let finish_rx = Arc::new(Mutex::new(finish_rx));
        let runner_finish = Arc::clone(&finish_rx);
        let bus = EventBus::new(32);
        let controller = crate::controller::spawn_with_runner(
            AppConfig::default(),
            None,
            bus.clone(),
            move |request, mut control| {
                control.ready.take().unwrap()(Ok(()));
                while !control.stop.load(Ordering::SeqCst) {
                    thread::yield_now();
                }
                if let Some(stopping) = control.stopping.take() {
                    stopping();
                }
                runner_finish.lock().unwrap().recv().unwrap();
                Ok(crate::recorder::RecordingSummary {
                    session_id: request.session_id,
                    prefix: request.recording.prefix.unwrap(),
                    out_dir: request.recording.out_dir,
                    captured_frames: 0,
                    pipeline_stats: None,
                    bf_stats: None,
                })
            },
        )
        .unwrap();
        let mut server = start_with_options(
            controller.clone(),
            bus,
            WebOptions {
                bind: "127.0.0.1:0".parse().unwrap(),
                max_clients: 1,
                open_browser: false,
            },
        )
        .unwrap();

        let start = request_http(
            server.local_addr,
            "POST",
            "/api/recordings/start",
            "application/json",
            "",
            "202 Accepted",
        );
        assert!(start.contains("session_id"));
        let conflict = request_http(
            server.local_addr,
            "POST",
            "/api/recordings/start",
            "application/json",
            "",
            "409 Conflict",
        );
        assert!(conflict.contains("当前已有录音 session"));
        request_http(
            server.local_addr,
            "POST",
            "/api/recordings/stop",
            "application/json",
            "",
            "202 Accepted",
        );
        request_http(
            server.local_addr,
            "POST",
            "/api/recordings/stop",
            "application/json",
            "",
            "202 Accepted",
        );
        finish_tx.send(()).unwrap();
        let idle = read_http(server.local_addr, "/api/status", "\"phase\":\"idle\"");
        assert!(idle.contains("200 OK"));
        let no_active = request_http(
            server.local_addr,
            "POST",
            "/api/recordings/stop",
            "application/json",
            "",
            "409 Conflict",
        );
        assert!(no_active.contains("当前没有可停止的录音"));
        controller.shutdown().unwrap();
        server.shutdown().unwrap();
    }

    fn read_http(bind: SocketAddr, path: &str, needle: &str) -> String {
        request_http(bind, "GET", path, "application/json", "", needle)
    }

    fn request_http(
        bind: SocketAddr,
        method: &str,
        path: &str,
        content_type: &str,
        body: &str,
        needle: &str,
    ) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last_output = String::new();
        while Instant::now() < deadline {
            if let Ok(mut stream) = TcpStream::connect(bind) {
                stream
                    .set_read_timeout(Some(Duration::from_millis(250)))
                    .unwrap();
                write!(
                    stream,
                    "{method} {path} HTTP/1.1\r\nHost: {bind}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                let mut output = String::new();
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => output.push_str(&String::from_utf8_lossy(&buf[..n])),
                        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => break,
                        Err(error) => panic!("HTTP 读取失败: {error}"),
                    }
                }
                if output.contains(needle) {
                    return output;
                }
                last_output = output;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("等待 {path} 超时，最后响应: {last_output}");
    }

    fn open_sse(bind: SocketAddr) -> TcpStream {
        let mut stream = TcpStream::connect(bind).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        write!(
            stream,
            "GET /api/events HTTP/1.1\r\nHost: {bind}\r\nConnection: keep-alive\r\n\r\n"
        )
        .unwrap();
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).contains("200 OK"));
        stream
    }

    fn open_sse_status(bind: SocketAddr) -> String {
        let mut stream = TcpStream::connect(bind).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        write!(
            stream,
            "GET /api/events HTTP/1.1\r\nHost: {bind}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut buf = [0u8; 4096];
        let bytes = stream.read(&mut buf).unwrap();
        String::from_utf8_lossy(&buf[..bytes]).into_owned()
    }
}
