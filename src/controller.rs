//! 录音服务 Controller。
//!
//! HTTP handler 只通过这里发送低频控制命令；WASAPI、WAV 和算法线程的
//! ownership 均留在 Controller 创建的 session 中。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::app_config::{self, AppConfig};
use crate::events::{
    DeviceSnapshot, ErrorSource, EventBus, EventPublisher, PipelineSnapshot, RecordingSnapshot,
    ServiceError, ServicePhase, ServiceSnapshot,
};
use crate::recorder::{RecordingControl, RecordingRequest, RecordingSummary, run_recording};
use crate::recordings;

const COMMAND_CAPACITY: usize = 32;

type SessionRunner = Arc<
    dyn Fn(RecordingRequest, RecordingControl) -> Result<RecordingSummary, String> + Send + Sync,
>;

#[derive(Clone)]
pub struct ControllerHandle {
    tx: SyncSender<ControllerCommand>,
    draft: Arc<RwLock<AppConfig>>,
    bus: EventBus,
}

struct SessionHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

enum ControllerCommand {
    UpdateConfig {
        config: AppConfig,
        reply: SyncSender<Result<(), String>>,
    },
    SaveConfig {
        reply: SyncSender<Result<(), String>>,
    },
    ResetConfig {
        reply: SyncSender<Result<(), String>>,
    },
    ImportConfig {
        text: String,
        reply: SyncSender<Result<AppConfig, String>>,
    },
    SaveProfile {
        name: String,
        config: Option<AppConfig>,
        reply: SyncSender<Result<(), String>>,
    },
    LoadProfile {
        name: String,
        reply: SyncSender<Result<AppConfig, String>>,
    },
    DeleteProfile {
        name: String,
        reply: SyncSender<Result<(), String>>,
    },
    ListProfiles {
        reply: SyncSender<Result<Vec<String>, String>>,
    },
    Start {
        reply: SyncSender<Result<String, String>>,
    },
    Stop {
        reply: SyncSender<Result<(), String>>,
    },
    RefreshDevice,
    SessionReady {
        session_id: String,
        result: Result<(), String>,
    },
    SessionStopping {
        session_id: String,
    },
    SessionFinished {
        session_id: String,
        result: Result<RecordingSummary, String>,
    },
    Shutdown {
        reply: SyncSender<Result<(), String>>,
    },
}

impl ControllerHandle {
    pub fn snapshot(&self) -> ServiceSnapshot {
        self.bus.latest_snapshot()
    }

    pub fn config(&self) -> AppConfig {
        self.draft.read().expect("配置锁 poisoned").clone()
    }

    fn call<T>(&self, command: ControllerCommand, reply: Receiver<T>) -> Result<T, String> {
        self.tx
            .send(command)
            .map_err(|_| "AudioController 已停止".to_string())?;
        reply
            .recv()
            .map_err(|_| "AudioController 未返回响应".to_string())
    }

    pub fn update_config(&self, config: AppConfig) -> Result<(), String> {
        let (tx, rx) = sync_channel(1);
        self.call(ControllerCommand::UpdateConfig { config, reply: tx }, rx)?
    }

    pub fn save_config(&self) -> Result<(), String> {
        let (tx, rx) = sync_channel(1);
        self.call(ControllerCommand::SaveConfig { reply: tx }, rx)?
    }

    pub fn reset_config(&self) -> Result<(), String> {
        let (tx, rx) = sync_channel(1);
        self.call(ControllerCommand::ResetConfig { reply: tx }, rx)?
    }

    pub fn import_config(&self, text: String) -> Result<AppConfig, String> {
        let (tx, rx) = sync_channel(1);
        self.call(ControllerCommand::ImportConfig { text, reply: tx }, rx)?
    }

    pub fn save_profile(&self, name: String, config: Option<AppConfig>) -> Result<(), String> {
        let (tx, rx) = sync_channel(1);
        self.call(
            ControllerCommand::SaveProfile {
                name,
                config,
                reply: tx,
            },
            rx,
        )?
    }

    pub fn load_profile(&self, name: String) -> Result<AppConfig, String> {
        let (tx, rx) = sync_channel(1);
        self.call(ControllerCommand::LoadProfile { name, reply: tx }, rx)?
    }

    pub fn delete_profile(&self, name: String) -> Result<(), String> {
        let (tx, rx) = sync_channel(1);
        self.call(ControllerCommand::DeleteProfile { name, reply: tx }, rx)?
    }

    pub fn list_profiles(&self) -> Result<Vec<String>, String> {
        let (tx, rx) = sync_channel(1);
        self.call(ControllerCommand::ListProfiles { reply: tx }, rx)?
    }

    pub fn start(&self) -> Result<String, String> {
        let (tx, rx) = sync_channel(1);
        self.call(ControllerCommand::Start { reply: tx }, rx)?
    }

    pub fn stop(&self) -> Result<(), String> {
        let (tx, rx) = sync_channel(1);
        self.call(ControllerCommand::Stop { reply: tx }, rx)?
    }

    pub fn refresh_device(&self) {
        let _ = self.tx.try_send(ControllerCommand::RefreshDevice);
    }

    pub fn shutdown(&self) -> Result<(), String> {
        let (tx, rx) = sync_channel(1);
        self.call(ControllerCommand::Shutdown { reply: tx }, rx)?
    }
}

pub fn spawn(
    config: AppConfig,
    config_warning: Option<String>,
    bus: EventBus,
) -> Result<ControllerHandle, String> {
    spawn_with_session_runner(config, config_warning, bus, Arc::new(run_recording))
}

fn spawn_with_session_runner(
    config: AppConfig,
    config_warning: Option<String>,
    bus: EventBus,
    session_runner: SessionRunner,
) -> Result<ControllerHandle, String> {
    config.validate()?;
    let (tx, rx) = sync_channel(COMMAND_CAPACITY);
    // Session threads must never wait on the bounded HTTP/control queue while
    // the controller is joining them.
    let (internal_tx, internal_rx) = mpsc::channel();
    let draft = Arc::new(RwLock::new(config.clone()));
    let thread_draft = Arc::clone(&draft);
    let thread_internal_tx = internal_tx.clone();
    let thread_bus = bus.clone();
    thread::Builder::new()
        .name("audio-controller".into())
        .spawn(move || {
            run_controller(
                rx,
                internal_rx,
                thread_internal_tx,
                thread_draft,
                thread_bus,
                config_warning,
                session_runner,
            )
        })
        .map_err(|e| format!("创建 AudioController 线程失败: {e}"))?;
    Ok(ControllerHandle { tx, draft, bus })
}

#[cfg(test)]
pub(crate) fn spawn_with_runner<F>(
    config: AppConfig,
    config_warning: Option<String>,
    bus: EventBus,
    session_runner: F,
) -> Result<ControllerHandle, String>
where
    F: Fn(RecordingRequest, RecordingControl) -> Result<RecordingSummary, String>
        + Send
        + Sync
        + 'static,
{
    spawn_with_session_runner(config, config_warning, bus, Arc::new(session_runner))
}

fn run_controller(
    rx: Receiver<ControllerCommand>,
    internal_rx: Receiver<ControllerCommand>,
    internal_tx: Sender<ControllerCommand>,
    draft: Arc<RwLock<AppConfig>>,
    bus: EventBus,
    config_warning: Option<String>,
    session_runner: SessionRunner,
) {
    let events = bus.publisher();
    let mut snapshot = ServiceSnapshot {
        config_warning,
        ..ServiceSnapshot::default()
    };
    publish_snapshot(&mut snapshot, &events);
    refresh_device(&mut snapshot, &events);
    let mut active: Option<SessionHandle> = None;

    loop {
        let command = match internal_rx.try_recv() {
            Ok(command) => command,
            Err(TryRecvError::Empty) => match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            Err(TryRecvError::Disconnected) => break,
        };
        match command {
            ControllerCommand::UpdateConfig { config, reply } => {
                let result = config.validate().map(|_| {
                    *draft.write().expect("配置锁 poisoned") = config;
                });
                let _ = reply.send(result);
            }
            ControllerCommand::SaveConfig { reply } => {
                let result = app_config::save_config(&draft.read().expect("配置锁 poisoned"));
                let _ = reply.send(result);
            }
            ControllerCommand::ResetConfig { reply } => {
                *draft.write().expect("配置锁 poisoned") = AppConfig::default();
                let _ = reply.send(Ok(()));
            }
            ControllerCommand::ImportConfig { text, reply } => {
                let result = AppConfig::parse_toml(&text);
                if let Ok(config) = &result {
                    *draft.write().expect("配置锁 poisoned") = config.clone();
                }
                let _ = reply.send(result);
            }
            ControllerCommand::SaveProfile {
                name,
                config,
                reply,
            } => {
                let config =
                    config.unwrap_or_else(|| draft.read().expect("配置锁 poisoned").clone());
                let _ = reply.send(app_config::save_profile(&name, &config));
            }
            ControllerCommand::LoadProfile { name, reply } => {
                let result = match name.as_str() {
                    "pure" => Ok(app_config::preset_pure()),
                    "doa" => app_config::preset_doa(),
                    "doa_bf" => app_config::preset_doa_bf(),
                    "bf_fixed" => app_config::preset_bf_fixed(),
                    _ => app_config::load_profile(&name),
                };
                if let Ok(config) = &result {
                    *draft.write().expect("配置锁 poisoned") = config.clone();
                }
                let _ = reply.send(result);
            }
            ControllerCommand::DeleteProfile { name, reply } => {
                let _ = reply.send(app_config::delete_profile(&name));
            }
            ControllerCommand::ListProfiles { reply } => {
                let _ = reply.send(app_config::list_profiles());
            }
            ControllerCommand::RefreshDevice => refresh_device(&mut snapshot, &events),
            ControllerCommand::Start { reply } => {
                let result = start_session(
                    &mut snapshot,
                    &events,
                    &draft,
                    &internal_tx,
                    &session_runner,
                    &mut active,
                );
                let _ = reply.send(result);
            }
            ControllerCommand::Stop { reply } => {
                let result = stop_session(&mut snapshot, &events, active.as_ref());
                let _ = reply.send(result);
            }
            ControllerCommand::SessionReady { session_id, result } => {
                if !matches!(
                    snapshot.phase,
                    ServicePhase::Starting { session_id: ref current } if current == &session_id
                ) {
                    continue;
                }
                if let Err(error) = result {
                    set_error(
                        &mut snapshot,
                        &events,
                        ErrorSource::Device,
                        "startup_failed",
                        error,
                    );
                    publish_snapshot(&mut snapshot, &events);
                } else {
                    snapshot.phase = ServicePhase::Recording { session_id };
                    publish_snapshot(&mut snapshot, &events);
                }
            }
            ControllerCommand::SessionStopping { session_id } => {
                if session_matches(&snapshot.phase, &session_id) {
                    snapshot.phase = ServicePhase::Stopping { session_id };
                    publish_snapshot(&mut snapshot, &events);
                }
            }
            ControllerCommand::SessionFinished { session_id, result } => {
                if !session_matches(&snapshot.phase, &session_id) {
                    continue;
                }
                if let Some(mut handle) = active.take()
                    && let Some(join) = handle.join.take()
                {
                    let _ = join.join();
                }
                let (error, summary) = match result {
                    Ok(summary) => (None, Some(summary)),
                    Err(error) => (Some(error), None),
                };
                snapshot.phase = ServicePhase::Idle;
                snapshot.recording = None;
                if let Some(error) = &error {
                    let source = recording_error_source(error);
                    set_error(
                        &mut snapshot,
                        &events,
                        source,
                        "recording_failed",
                        error.clone(),
                    );
                }
                events.publish(
                    "session_finished",
                    serde_json::json!({
                        "session_id": session_id,
                        "success": error.is_none(),
                        "summary_session_id": summary.as_ref().map(|s| &s.session_id),
                        "captured_frames": summary.as_ref().map(|s| s.captured_frames),
                        "prefix": summary.as_ref().map(|s| &s.prefix),
                        "out_dir": summary.as_ref().map(|s| &s.out_dir),
                        "pipeline_stats": summary.as_ref().and_then(|s| s.pipeline_stats.as_ref()),
                        "bf_stats": summary.as_ref().and_then(|s| s.bf_stats.as_ref()),
                    }),
                );
                publish_snapshot(&mut snapshot, &events);
            }
            ControllerCommand::Shutdown { reply } => {
                if let Some(mut handle) = active.take() {
                    handle.stop.store(true, Ordering::SeqCst);
                    if let Some(join) = handle.join.take() {
                        let _ = join.join();
                    }
                }
                snapshot.phase = ServicePhase::Idle;
                snapshot.recording = None;
                publish_snapshot(&mut snapshot, &events);
                let _ = reply.send(Ok(()));
                break;
            }
        }
    }
}

fn start_session(
    snapshot: &mut ServiceSnapshot,
    events: &EventPublisher,
    draft: &Arc<RwLock<AppConfig>>,
    internal_tx: &Sender<ControllerCommand>,
    session_runner: &SessionRunner,
    active: &mut Option<SessionHandle>,
) -> Result<String, String> {
    if !matches!(snapshot.phase, ServicePhase::Idle) {
        return Err("conflict:当前已有录音 session".into());
    }
    let mut config = draft.read().expect("配置锁 poisoned").clone();
    config.validate()?;
    let prefix = if let Some(prefix) = config.recording.prefix.clone() {
        crate::app_config::validate_prefix(&prefix)?;
        recordings::check_no_clobber(&config.recording.out_dir, &prefix, &config)?;
        prefix
    } else {
        let base = crate::recorder::default_prefix();
        let mut selected = None;
        for index in 0..10_000u32 {
            let candidate = if index == 0 {
                base.clone()
            } else {
                format!("{base}_{index}")
            };
            match recordings::check_no_clobber(&config.recording.out_dir, &candidate, &config) {
                Ok(()) => {
                    selected = Some(candidate);
                    break;
                }
                Err(error) if error.starts_with("目标文件已存在") => {}
                Err(error) => return Err(error),
            }
        }
        selected.ok_or_else(|| "无法生成不冲突的录音 prefix".to_string())?
    };
    config.recording.prefix = Some(prefix.clone());
    let session_id = format!(
        "session-{}-{}",
        chrono::Local::now().format("%Y%m%d%H%M%S"),
        next_session_number()
    );
    crate::recordings::validate_session_id(&session_id)?;
    let stop = Arc::new(AtomicBool::new(false));
    let recording_request = RecordingRequest {
        recording: config.recording.clone(),
        pipeline: config.pipeline_enabled.then_some(config.pipeline.clone()),
        session_id: session_id.clone(),
    };
    let worker_tx = internal_tx.clone();
    let worker_id = session_id.clone();
    let worker_stop = Arc::clone(&stop);
    let worker_events = events.clone();
    let worker_runner = Arc::clone(session_runner);
    let join = thread::Builder::new()
        .name(format!("recording-session-{worker_id}"))
        .spawn(move || {
            let ready_tx = worker_tx.clone();
            let ready_id = worker_id.clone();
            let stopping_tx = worker_tx.clone();
            let stopping_id = worker_id.clone();
            let control = RecordingControl {
                stop: worker_stop,
                events: worker_events,
                ready: Some(Box::new(move |result| {
                    let _ = ready_tx.send(ControllerCommand::SessionReady {
                        session_id: ready_id,
                        result,
                    });
                })),
                stopping: Some(Box::new(move || {
                    let _ = stopping_tx.send(ControllerCommand::SessionStopping {
                        session_id: stopping_id,
                    });
                })),
            };
            let result = worker_runner(recording_request, control);
            let _ = worker_tx.send(ControllerCommand::SessionFinished {
                session_id: worker_id,
                result,
            });
        })
        .map_err(|e| format!("创建录音 session 线程失败: {e}"))?;

    *active = Some(SessionHandle {
        stop,
        join: Some(join),
    });
    snapshot.last_error = None;
    snapshot.pipeline = PipelineSnapshot {
        enabled: config.pipeline_enabled,
        ..PipelineSnapshot::default()
    };
    snapshot.recording = Some(RecordingSnapshot {
        session_id: session_id.clone(),
        prefix,
        out_dir: config.recording.out_dir,
        started_at: chrono::Local::now().to_rfc3339(),
        ..RecordingSnapshot::default()
    });
    snapshot.phase = ServicePhase::Starting {
        session_id: session_id.clone(),
    };
    publish_snapshot(snapshot, events);
    Ok(session_id)
}

fn stop_session(
    snapshot: &mut ServiceSnapshot,
    events: &EventPublisher,
    active: Option<&SessionHandle>,
) -> Result<(), String> {
    match snapshot.phase {
        ServicePhase::Starting { ref session_id } | ServicePhase::Recording { ref session_id } => {
            if let Some(handle) = active {
                handle.stop.store(true, Ordering::SeqCst);
            }
            snapshot.phase = ServicePhase::Stopping {
                session_id: session_id.clone(),
            };
            publish_snapshot(snapshot, events);
            Ok(())
        }
        ServicePhase::Stopping { .. } => Ok(()),
        ServicePhase::Idle => Err("conflict:当前没有可停止的录音".into()),
    }
}

fn refresh_device(snapshot: &mut ServiceSnapshot, events: &EventPublisher) {
    match crate::wasapi::pick_respeaker_input_device() {
        Ok(device) => {
            snapshot.device = DeviceSnapshot {
                available: true,
                name: Some(device.name),
                error: None,
            };
        }
        Err(error) => {
            snapshot.device = DeviceSnapshot {
                available: false,
                name: None,
                error: Some(error),
            };
        }
    }
    events.publish_device_status(snapshot.device.clone());
    publish_snapshot(snapshot, events);
}

fn publish_snapshot(snapshot: &mut ServiceSnapshot, events: &EventPublisher) {
    snapshot.revision = snapshot.revision.saturating_add(1);
    events.publish_state(snapshot);
}

fn set_error(
    snapshot: &mut ServiceSnapshot,
    events: &EventPublisher,
    source: ErrorSource,
    code: &str,
    message: String,
) {
    let error = ServiceError::new(source, code, message);
    snapshot.last_error = Some(error.clone());
    events.publish_error(error);
}

fn session_matches(phase: &ServicePhase, id: &str) -> bool {
    match phase {
        ServicePhase::Starting { session_id }
        | ServicePhase::Recording { session_id }
        | ServicePhase::Stopping { session_id } => session_id == id,
        ServicePhase::Idle => false,
    }
}

fn recording_error_source(error: &str) -> ErrorSource {
    let lower = error.to_ascii_lowercase();
    if lower.contains("pipeline") || lower.contains("algorithm worker") {
        ErrorSource::Pipeline
    } else if lower.contains("wav") || error.contains("写入") || error.contains("完成") {
        ErrorSource::RawWav
    } else {
        ErrorSource::Capture
    }
}

fn next_session_number() -> u64 {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    fn summary(request: &RecordingRequest) -> RecordingSummary {
        RecordingSummary {
            session_id: request.session_id.clone(),
            prefix: request
                .recording
                .prefix
                .clone()
                .unwrap_or_else(|| "test".into()),
            out_dir: request.recording.out_dir.clone(),
            captured_frames: 0,
            pipeline_stats: None,
            bf_stats: None,
        }
    }

    fn wait_snapshot(
        controller: &ControllerHandle,
        predicate: impl Fn(&ServiceSnapshot) -> bool,
    ) -> ServiceSnapshot {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = controller.snapshot();
            if predicate(&snapshot) {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "等待 Controller 状态超时: {snapshot:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn stop_is_idempotent_in_stopping_phase() {
        let mut snapshot = ServiceSnapshot {
            phase: ServicePhase::Stopping {
                session_id: "s".into(),
            },
            ..ServiceSnapshot::default()
        };
        let bus = EventBus::new(8);
        assert!(stop_session(&mut snapshot, &bus.publisher(), None).is_ok());
    }

    #[test]
    fn session_matching_only_accepts_active_phases() {
        assert!(!session_matches(&ServicePhase::Idle, "s"));
        assert!(session_matches(
            &ServicePhase::Recording {
                session_id: "s".into()
            },
            "s"
        ));
    }

    #[test]
    fn start_conflict_stop_idempotent_and_second_session() {
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (finish_tx, finish_rx) = mpsc::channel::<()>();
        let ready_rx = Arc::new(Mutex::new(ready_rx));
        let finish_rx = Arc::new(Mutex::new(finish_rx));
        let runs = Arc::new(AtomicUsize::new(0));
        let runner_ready = Arc::clone(&ready_rx);
        let runner_finish = Arc::clone(&finish_rx);
        let runner_runs = Arc::clone(&runs);
        let bus = EventBus::new(32);
        let controller = spawn_with_runner(
            AppConfig::default(),
            None,
            bus,
            move |request, mut control| {
                runner_runs.fetch_add(1, Ordering::SeqCst);
                runner_ready.lock().unwrap().recv().unwrap();
                control.ready.take().unwrap()(Ok(()));
                while !control.stop.load(Ordering::SeqCst) {
                    thread::yield_now();
                }
                if let Some(stopping) = control.stopping.take() {
                    stopping();
                }
                runner_finish.lock().unwrap().recv().unwrap();
                Ok(summary(&request))
            },
        )
        .unwrap();

        controller.start().unwrap();
        wait_snapshot(&controller, |snapshot| {
            matches!(snapshot.phase, ServicePhase::Starting { .. })
        });
        assert!(controller.start().unwrap_err().starts_with("conflict:"));
        ready_tx.send(()).unwrap();
        wait_snapshot(&controller, |snapshot| {
            matches!(snapshot.phase, ServicePhase::Recording { .. })
        });
        controller.stop().unwrap();
        wait_snapshot(&controller, |snapshot| {
            matches!(snapshot.phase, ServicePhase::Stopping { .. })
        });
        controller.stop().unwrap();
        finish_tx.send(()).unwrap();
        wait_snapshot(&controller, |snapshot| {
            matches!(snapshot.phase, ServicePhase::Idle)
        });

        controller.start().unwrap();
        ready_tx.send(()).unwrap();
        wait_snapshot(&controller, |snapshot| {
            matches!(snapshot.phase, ServicePhase::Recording { .. })
        });
        controller.stop().unwrap();
        finish_tx.send(()).unwrap();
        wait_snapshot(&controller, |snapshot| {
            matches!(snapshot.phase, ServicePhase::Idle)
        });
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        controller.shutdown().unwrap();
    }

    #[test]
    fn startup_failure_returns_idle_and_keeps_error() {
        let bus = EventBus::new(32);
        let controller =
            spawn_with_runner(AppConfig::default(), None, bus, |_request, mut control| {
                control.ready.take().unwrap()(Err("synthetic startup failure".into()));
                Err("synthetic startup failure".into())
            })
            .unwrap();
        controller.start().unwrap();
        let snapshot = wait_snapshot(&controller, |snapshot| {
            matches!(snapshot.phase, ServicePhase::Idle) && snapshot.last_error.is_some()
        });
        assert!(
            snapshot
                .last_error
                .unwrap()
                .message
                .contains("synthetic startup failure")
        );
        controller.shutdown().unwrap();
    }

    #[test]
    fn pipeline_failure_keeps_recording_until_stop() {
        let (finish_tx, finish_rx) = mpsc::channel::<()>();
        let finish_rx = Arc::new(Mutex::new(finish_rx));
        let runner_finish = Arc::clone(&finish_rx);
        let bus = EventBus::new(32);
        let controller = spawn_with_runner(
            AppConfig::default(),
            None,
            bus,
            move |request, mut control| {
                control.ready.take().unwrap()(Ok(()));
                control.events.publish_error(ServiceError::new(
                    ErrorSource::Pipeline,
                    "pipeline_degraded",
                    "synthetic pipeline failure".into(),
                ));
                while !control.stop.load(Ordering::SeqCst) {
                    thread::yield_now();
                }
                if let Some(stopping) = control.stopping.take() {
                    stopping();
                }
                runner_finish.lock().unwrap().recv().unwrap();
                Ok(summary(&request))
            },
        )
        .unwrap();
        controller.start().unwrap();
        let snapshot = wait_snapshot(&controller, |snapshot| {
            matches!(snapshot.phase, ServicePhase::Recording { .. }) && snapshot.pipeline.degraded
        });
        assert_eq!(
            snapshot.pipeline.error.as_deref(),
            Some("synthetic pipeline failure")
        );
        controller.stop().unwrap();
        finish_tx.send(()).unwrap();
        wait_snapshot(&controller, |snapshot| {
            matches!(snapshot.phase, ServicePhase::Idle)
        });
        controller.shutdown().unwrap();
    }

    #[test]
    fn capture_fatal_publishes_stopping_before_finished() {
        let (finish_tx, finish_rx) = mpsc::channel::<()>();
        let finish_rx = Arc::new(Mutex::new(finish_rx));
        let runner_finish = Arc::clone(&finish_rx);
        let bus = EventBus::new(32);
        let controller = spawn_with_runner(
            AppConfig::default(),
            None,
            bus,
            move |_request, mut control| {
                control.ready.take().unwrap()(Ok(()));
                control.stopping.take().unwrap()();
                runner_finish.lock().unwrap().recv().unwrap();
                Err("synthetic capture fatal".into())
            },
        )
        .unwrap();
        controller.start().unwrap();
        wait_snapshot(&controller, |snapshot| {
            matches!(snapshot.phase, ServicePhase::Stopping { .. })
        });
        finish_tx.send(()).unwrap();
        wait_snapshot(&controller, |snapshot| {
            matches!(snapshot.phase, ServicePhase::Idle)
        });
        controller.shutdown().unwrap();
    }

    #[test]
    fn shutdown_waits_for_session_finalize() {
        let finalized = Arc::new(AtomicBool::new(false));
        let runner_finalized = Arc::clone(&finalized);
        let bus = EventBus::new(32);
        let controller = spawn_with_runner(
            AppConfig::default(),
            None,
            bus,
            move |request, mut control| {
                control.ready.take().unwrap()(Ok(()));
                while !control.stop.load(Ordering::SeqCst) {
                    thread::yield_now();
                }
                thread::sleep(Duration::from_millis(20));
                runner_finalized.store(true, Ordering::SeqCst);
                Ok(summary(&request))
            },
        )
        .unwrap();
        controller.start().unwrap();
        wait_snapshot(&controller, |snapshot| {
            matches!(snapshot.phase, ServicePhase::Recording { .. })
        });
        controller.shutdown().unwrap();
        assert!(finalized.load(Ordering::SeqCst));
    }
}
