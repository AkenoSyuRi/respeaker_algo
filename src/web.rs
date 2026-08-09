//! DOA 本地 Web Viewer：嵌入页面、SSE 广播与服务生命周期。

use std::convert::Infallible;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::doa::DoaResult;
use crate::doa::tracker::TrackStatus;

#[cfg(not(test))]
const DEFAULT_BIND: &str = "127.0.0.1:8765";
#[cfg(test)]
const DEFAULT_BIND: &str = "127.0.0.1:0";
const DEFAULT_MAX_CLIENTS: usize = 8;
const INDEX_HTML: &str = include_str!("../web/doa_viewer.html");

pub struct WebServerHandle {
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<Result<(), String>>>,
    #[cfg(test)]
    local_addr: SocketAddr,
}

#[derive(Clone)]
pub struct WebBroadcaster {
    sender: broadcast::Sender<String>,
    latest: Arc<Mutex<Option<String>>>,
}

#[derive(Clone)]
struct WebState {
    sender: broadcast::Sender<String>,
    latest: Arc<Mutex<Option<String>>>,
    active_clients: Arc<AtomicUsize>,
    max_clients: usize,
    shutdown: broadcast::Sender<()>,
}

#[derive(Clone, Copy)]
struct WebOptions {
    bind: SocketAddr,
    max_clients: usize,
    open_browser: bool,
}

#[derive(Serialize)]
struct WebDoaMessage {
    #[serde(rename = "type")]
    message_type: &'static str,
    seq: u64,
    timestamp_ms: f64,
    raw_angle_deg: Option<f32>,
    tracked_angle_deg: Option<f32>,
    confidence: f32,
    status: &'static str,
    observation_used: bool,
    peak_score: f32,
    mean_msc: f32,
    rms_dbfs: f32,
}

struct ClientGuard {
    active_clients: Arc<AtomicUsize>,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.active_clients.fetch_sub(1, Ordering::Relaxed);
    }
}

impl WebBroadcaster {
    pub fn publish(&self, seq: u64, result: &DoaResult) -> Result<(), String> {
        let message = WebDoaMessage {
            message_type: "doa",
            seq,
            timestamp_ms: result.timestamp_ms,
            raw_angle_deg: result.raw_angle_deg,
            tracked_angle_deg: result.tracked_angle_deg,
            confidence: result.confidence,
            status: status_str(result.status),
            observation_used: result.observation_used,
            peak_score: result.peak_score,
            mean_msc: result.mean_msc,
            rms_dbfs: result.rms_dbfs,
        };
        let json =
            serde_json::to_string(&message).map_err(|e| format!("序列化 DOA Web 消息失败: {e}"))?;
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some(json.clone());
        }
        let _ = self.sender.send(json);
        Ok(())
    }
}

impl WebServerHandle {
    pub fn start() -> Result<(Self, WebBroadcaster), String> {
        let bind = DEFAULT_BIND
            .parse()
            .map_err(|e| format!("解析 DOA Web 地址 {DEFAULT_BIND} 失败: {e}"))?;
        Self::start_with_options(WebOptions {
            bind,
            max_clients: DEFAULT_MAX_CLIENTS,
            open_browser: !cfg!(test),
        })
    }

    fn start_with_options(options: WebOptions) -> Result<(Self, WebBroadcaster), String> {
        if options.max_clients == 0 {
            return Err("DOA Web 最大客户端数必须大于 0".into());
        }
        let listener = StdTcpListener::bind(options.bind)
            .map_err(|e| format!("绑定 DOA Web 地址 {} 失败: {e}", options.bind))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("设置 DOA Web 监听器为非阻塞失败: {e}"))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| format!("读取 DOA Web 本地地址失败: {e}"))?;

        let (sender, _) = broadcast::channel(128);
        let latest = Arc::new(Mutex::new(None));
        let broadcaster = WebBroadcaster {
            sender: sender.clone(),
            latest: Arc::clone(&latest),
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let join = thread::Builder::new()
            .name("doa-web".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("创建 DOA Web 异步运行时失败: {e}"))?;
                runtime.block_on(run_server(
                    listener,
                    sender,
                    latest,
                    options.max_clients,
                    shutdown_rx,
                ))
            })
            .map_err(|e| format!("启动 DOA Web 服务线程失败: {e}"))?;

        let url = format!("http://{local_addr}");
        eprintln!("[web] DOA Viewer: {url}");
        if options.open_browser
            && let Err(e) = open_default_browser(&url)
        {
            eprintln!("[web] 打开默认浏览器失败: {e}");
        }

        Ok((
            Self {
                shutdown: Some(shutdown_tx),
                join: Some(join),
                #[cfg(test)]
                local_addr,
            },
            broadcaster,
        ))
    }

    pub fn shutdown(&mut self) -> Result<(), String> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| "DOA Web 服务线程异常退出".to_string())??;
        }
        Ok(())
    }
}

impl Drop for WebServerHandle {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown() {
            eprintln!("[web] 关闭 DOA Viewer 失败: {e}");
        }
    }
}

async fn run_server(
    listener: StdTcpListener,
    sender: broadcast::Sender<String>,
    latest: Arc<Mutex<Option<String>>>,
    max_clients: usize,
    shutdown: oneshot::Receiver<()>,
) -> Result<(), String> {
    let listener = TcpListener::from_std(listener)
        .map_err(|e| format!("创建 DOA Web Tokio 监听器失败: {e}"))?;
    let (client_shutdown, _) = broadcast::channel(1);
    let state = WebState {
        sender,
        latest,
        active_clients: Arc::new(AtomicUsize::new(0)),
        max_clients,
        shutdown: client_shutdown.clone(),
    };
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/events", get(sse_handler))
        .with_state(state);

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.await;
            let _ = client_shutdown.send(());
        })
        .await
        .map_err(|e| format!("DOA Web 服务失败: {e}"))
}

async fn index_handler() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn sse_handler(State(state): State<WebState>) -> Response {
    if !try_acquire_client(&state.active_clients, state.max_clients) {
        return (StatusCode::TOO_MANY_REQUESTS, "DOA Web 客户端数量已达上限").into_response();
    }
    let guard = Arc::new(ClientGuard {
        active_clients: Arc::clone(&state.active_clients),
    });
    let receiver = state.sender.subscribe();
    let latest = state.latest.lock().ok().and_then(|value| value.clone());
    let latest_stream = tokio_stream::iter(
        latest
            .into_iter()
            .map(|json| Ok::<Event, Infallible>(Event::default().event("doa").data(json))),
    );
    let live_stream = BroadcastStream::new(receiver).filter_map(|message| match message {
        Ok(json) => Some(Ok::<Event, Infallible>(
            Event::default().event("doa").data(json),
        )),
        Err(_) => None,
    });
    let mut shutdown = state.shutdown.subscribe();
    let stream = latest_stream.chain(live_stream).map(move |event| {
        let _guard = &guard;
        event
    });
    let stream = futures_util::StreamExt::take_until(stream, async move {
        let _ = shutdown.recv().await;
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
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

fn status_str(status: TrackStatus) -> &'static str {
    match status {
        TrackStatus::Searching => "searching",
        TrackStatus::Tracking => "tracking",
        TrackStatus::Coasting => "coasting",
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
    use std::time::{Duration, Instant};

    #[test]
    fn serves_index_and_latest_doa_event() {
        let (mut handle, broadcaster) = start_test_server(4);
        broadcaster.publish(7, &test_result()).unwrap();

        let index = read_http_until(handle.local_addr, "/", "ReSpeaker DOA Viewer");
        assert!(index.contains("200 OK"));
        let events = read_http_until(handle.local_addr, "/events", r#""seq":7"#);
        assert!(events.contains("event: doa"));
        assert!(events.contains(r#""tracked_angle_deg":35.0"#));
        handle.shutdown().unwrap();
    }

    #[test]
    fn rejects_clients_over_limit() {
        let (mut handle, broadcaster) = start_test_server(1);
        broadcaster.publish(1, &test_result()).unwrap();
        let first = open_sse_connection(handle.local_addr);
        let second = read_http_until(handle.local_addr, "/events", "429 Too Many Requests");
        assert!(second.contains("429 Too Many Requests"));
        drop(first);
        handle.shutdown().unwrap();
    }

    #[test]
    fn shutdown_returns_with_active_sse_client() {
        let (handle, broadcaster) = start_test_server(4);
        broadcaster.publish(1, &test_result()).unwrap();
        let _client = open_sse_connection(handle.local_addr);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut handle = handle;
            let result = handle.shutdown();
            let _ = tx.send(result);
        });
        rx.recv_timeout(Duration::from_secs(2))
            .expect("关闭服务不应等待活动 SSE 客户端")
            .unwrap();
    }

    #[test]
    fn viewer_uses_downward_zero_degree_axis() {
        assert!(INDEX_HTML.contains(r#"data-angle="0" x="169" y="354""#));
        assert!(INDEX_HTML.contains(r#"data-angle="90" x="342" y="184""#));
        assert!(INDEX_HTML.contains("180 + radius * Math.sin(rad)"));
        assert!(INDEX_HTML.contains("180 + radius * Math.cos(rad)"));
        assert!(INDEX_HTML.contains(r#"data-mic="1" transform="translate(144 216)""#));
        assert!(INDEX_HTML.contains(r#"data-mic="2" transform="translate(216 216)""#));
        assert!(INDEX_HTML.contains(r#"data-mic="3" transform="translate(216 144)""#));
        assert!(INDEX_HTML.contains(r#"data-mic="4" transform="translate(144 144)""#));
    }

    fn start_test_server(max_clients: usize) -> (WebServerHandle, WebBroadcaster) {
        WebServerHandle::start_with_options(WebOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            max_clients,
            open_browser: false,
        })
        .unwrap()
    }

    fn test_result() -> DoaResult {
        DoaResult {
            timestamp_ms: 1234.0,
            raw_angle_deg: Some(40.0),
            tracked_angle_deg: Some(35.0),
            confidence: 0.62,
            status: TrackStatus::Tracking,
            observation_used: true,
            peak_score: 12.5,
            second_peak_score: 8.0,
            peak_gap_ratio: 0.3,
            prominence: 0.4,
            mean_msc: 0.7,
            rms_dbfs: -24.0,
        }
    }

    fn read_http_until(bind: SocketAddr, path: &str, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = String::new();
        while Instant::now() < deadline {
            if let Ok(mut stream) = TcpStream::connect(bind) {
                stream
                    .set_read_timeout(Some(Duration::from_millis(250)))
                    .unwrap();
                write!(
                    stream,
                    "GET {path} HTTP/1.1\r\nHost: {bind}\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                let mut buffer = [0u8; 2048];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            last.push_str(&String::from_utf8_lossy(&buffer[..n]));
                            if last.contains(needle) {
                                return last;
                            }
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut =>
                        {
                            break;
                        }
                        Err(e) => panic!("读取 HTTP 响应失败: {e}"),
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("等待 {path} 响应超时，最后响应: {last}");
    }

    fn open_sse_connection(bind: SocketAddr) -> TcpStream {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(mut stream) = TcpStream::connect(bind) {
                stream
                    .set_read_timeout(Some(Duration::from_millis(250)))
                    .unwrap();
                write!(
                    stream,
                    "GET /events HTTP/1.1\r\nHost: {bind}\r\nConnection: keep-alive\r\n\r\n"
                )
                .unwrap();
                let mut buffer = [0u8; 2048];
                let n = stream.read(&mut buffer).unwrap_or(0);
                let response = String::from_utf8_lossy(&buffer[..n]);
                if response.contains("200 OK") && response.contains("event: doa") {
                    return stream;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("打开 SSE 连接超时");
    }
}
