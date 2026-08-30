//! 统一事件总线与 SSE 信封

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::beamformer::BeamformerStats;
use crate::doa::DoaResult;
use crate::doa::tracker::TrackStatus;

// ---------------------------------------------------------------------------
// 错误与快照
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorSource {
    Config,
    Device,
    Capture,
    RawWav,
    Pipeline,
    Web,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct ServiceError {
    pub source: ErrorSource,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

impl ServiceError {
    pub fn new(source: ErrorSource, code: &str, message: String) -> Self {
        Self {
            source,
            code: code.to_string(),
            message,
            field: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServicePhase {
    #[default]
    Idle,
    Starting {
        session_id: String,
    },
    Recording {
        session_id: String,
    },
    Stopping {
        session_id: String,
    },
}

#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub struct DeviceSnapshot {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub struct RecordingSnapshot {
    pub session_id: String,
    pub prefix: String,
    pub out_dir: String,
    pub started_at: String,
    pub elapsed_secs: f64,
    pub captured_frames: u64,
    pub captured_seconds: f64,
}

#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub struct PipelineSnapshot {
    pub enabled: bool,
    pub degraded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doa_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bf_stats: Option<BeamformerStatsDto>,
}

#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub struct BeamformerStatsDto {
    pub input_frames: u64,
    pub output_frames: u64,
    pub stft_frames: u64,
    pub clipped_samples: u64,
}

impl From<&BeamformerStats> for BeamformerStatsDto {
    fn from(s: &BeamformerStats) -> Self {
        Self {
            input_frames: s.input_frames,
            output_frames: s.output_frames,
            stft_frames: s.stft_frames,
            clipped_samples: s.clipped_samples,
        }
    }
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct ServiceSnapshot {
    pub revision: u64,
    pub phase: ServicePhase,
    pub device: DeviceSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingSnapshot>,
    pub pipeline: PipelineSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<ServiceError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_warning: Option<String>,
}

impl Default for ServiceSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            phase: ServicePhase::Idle,
            device: DeviceSnapshot::default(),
            recording: None,
            pipeline: PipelineSnapshot::default(),
            last_error: None,
            config_warning: None,
        }
    }
}

// ---------------------------------------------------------------------------
// 事件信封
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    seq: u64,
    timestamp_ms: f64,
    payload: Value,
}

fn now_timestamp_ms() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

#[derive(Clone)]
pub struct EventBus {
    inner: Arc<EventBusInner>,
}

struct EventBusInner {
    sender: broadcast::Sender<String>,
    seq: AtomicU64,
    latest_snapshot: Mutex<ServiceSnapshot>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            inner: Arc::new(EventBusInner {
                sender,
                seq: AtomicU64::new(0),
                latest_snapshot: Mutex::new(ServiceSnapshot::default()),
            }),
        }
    }

    pub fn publisher(&self) -> EventPublisher {
        EventPublisher { bus: self.clone() }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.inner.sender.subscribe()
    }

    pub fn latest_snapshot(&self) -> ServiceSnapshot {
        self.inner.latest_snapshot.lock().unwrap().clone()
    }

    pub fn set_snapshot(&self, snap: ServiceSnapshot) {
        let mut snapshot = snap;
        let mut latest = self.inner.latest_snapshot.lock().unwrap();
        if !matches!(snapshot.phase, ServicePhase::Starting { .. }) {
            if let (Some(recording), Some(latest_recording)) =
                (&mut snapshot.recording, latest.recording.as_ref())
                && recording.session_id == latest_recording.session_id
            {
                *recording = latest_recording.clone();
            }
            snapshot.pipeline.doa_count = snapshot.pipeline.doa_count.or(latest.pipeline.doa_count);
            snapshot.pipeline.bf_stats = snapshot
                .pipeline
                .bf_stats
                .or_else(|| latest.pipeline.bf_stats.clone());
            if !snapshot.pipeline.degraded {
                snapshot.pipeline.degraded = latest.pipeline.degraded;
            }
            snapshot.pipeline.error = snapshot
                .pipeline
                .error
                .or_else(|| latest.pipeline.error.clone());
        }
        *latest = snapshot.clone();
        self.publish(
            "state_snapshot",
            serde_json::to_value(&snapshot).unwrap_or(Value::Null),
        );
    }

    fn update_snapshot(&self, update: impl FnOnce(&mut ServiceSnapshot)) {
        update(&mut self.inner.latest_snapshot.lock().unwrap());
    }

    pub fn snapshot_envelope(&self) -> String {
        let envelope = Envelope {
            kind: "state_snapshot".into(),
            seq: self.inner.seq.fetch_add(1, Ordering::Relaxed) + 1,
            timestamp_ms: now_timestamp_ms(),
            payload: serde_json::to_value(self.latest_snapshot()).unwrap_or(Value::Null),
        };
        serde_json::to_string(&envelope).unwrap_or_else(|_| "{}".into())
    }

    pub fn publish(&self, kind: &str, payload: Value) {
        let seq = self.inner.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let env = Envelope {
            kind: kind.to_string(),
            seq,
            timestamp_ms: now_timestamp_ms(),
            payload,
        };
        if let Ok(json) = serde_json::to_string(&env) {
            let _ = self.inner.sender.send(json);
        }
    }
}

#[derive(Clone)]
pub struct EventPublisher {
    bus: EventBus,
}

impl EventPublisher {
    pub fn publish(&self, kind: &str, payload: Value) {
        self.bus.publish(kind, payload);
    }

    pub fn publish_state(&self, snap: &ServiceSnapshot) {
        self.bus.set_snapshot(snap.clone());
    }

    pub fn publish_doa(&self, seq: u64, result: &DoaResult) -> Result<(), String> {
        let msg = serde_json::json!({
            "type": "doa",
            "seq": seq,
            "timestamp_ms": result.timestamp_ms,
            "raw_angle_deg": result.raw_angle_deg,
            "tracked_angle_deg": result.tracked_angle_deg,
            "raw_internal_deg": result.raw_internal_deg,
            "tracked_internal_deg": result.tracked_internal_deg,
            "confidence": result.confidence,
            "status": status_str(result.status),
            "observation_used": result.observation_used,
            "peak_score": result.peak_score,
            "mean_msc": result.mean_msc,
            "rms_dbfs": result.rms_dbfs,
        });
        self.bus.update_snapshot(|snapshot| {
            snapshot.pipeline.doa_count = Some(seq);
        });
        self.bus.publish("doa", msg);
        Ok(())
    }

    pub fn publish_bf_stats(&self, stats: &BeamformerStats) {
        let dto = BeamformerStatsDto::from(stats);
        self.bus.update_snapshot(|snapshot| {
            snapshot.pipeline.bf_stats = Some(dto.clone());
        });
        self.bus.publish(
            "bf_stats",
            serde_json::to_value(&dto).unwrap_or(Value::Null),
        );
    }

    pub fn publish_error(&self, err: ServiceError) {
        self.bus.update_snapshot(|snapshot| {
            if matches!(err.source, ErrorSource::Pipeline) {
                snapshot.pipeline.degraded = true;
                snapshot.pipeline.error = Some(err.message.clone());
            }
            snapshot.last_error = Some(err.clone());
        });
        self.bus
            .publish("error", serde_json::to_value(&err).unwrap_or(Value::Null));
    }

    pub fn publish_device_status(&self, snap: DeviceSnapshot) {
        self.bus.update_snapshot(|snapshot| {
            snapshot.device = snap.clone();
        });
        self.bus.publish(
            "device_status",
            serde_json::to_value(&snap).unwrap_or(Value::Null),
        );
    }

    pub fn publish_recording_progress(&self, secs: f64, frames: u64) {
        self.bus.update_snapshot(|snapshot| {
            if let Some(recording) = &mut snapshot.recording {
                recording.elapsed_secs = secs;
                recording.captured_frames = frames;
                recording.captured_seconds = frames as f64 / 16_000.0;
            }
        });
        self.bus.publish(
            "recording_progress",
            serde_json::json!({"elapsed_secs": secs, "captured_frames": frames}),
        );
    }

    pub fn publish_pipeline_status(&self, snap: PipelineSnapshot) {
        self.bus.update_snapshot(|snapshot| {
            snapshot.pipeline.enabled = snap.enabled;
            snapshot.pipeline.degraded = snap.degraded;
            snapshot.pipeline.error = snap.error.clone();
            if snap.doa_count.is_some() {
                snapshot.pipeline.doa_count = snap.doa_count;
            }
            if snap.bf_stats.is_some() {
                snapshot.pipeline.bf_stats = snap.bf_stats.clone();
            }
        });
        self.bus.publish(
            "pipeline_status",
            serde_json::to_value(&snap).unwrap_or(Value::Null),
        );
    }
}

fn status_str(s: TrackStatus) -> &'static str {
    match s {
        TrackStatus::Searching => "searching",
        TrackStatus::Tracking => "tracking",
        TrackStatus::Coasting => "coasting",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_serde() {
        let snap = ServiceSnapshot::default();
        let v = serde_json::to_value(&snap).unwrap();
        assert_eq!(v["revision"], 0);
        assert_eq!(v["phase"], "idle");
    }

    #[test]
    fn publish_and_subscribe() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        let pubr = bus.publisher();
        pubr.publish("test", serde_json::json!({"hello":1}));
        let msg = rx.try_recv().unwrap();
        assert!(msg.contains("test"));
    }

    #[test]
    fn lag_handling() {
        let bus = EventBus::new(2);
        bus.set_snapshot(ServiceSnapshot {
            revision: 77,
            ..ServiceSnapshot::default()
        });
        let mut receiver = bus.subscribe();
        let pubr = bus.publisher();
        for i in 0..10 {
            pubr.publish("ev", serde_json::json!({"i": i}));
        }
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        assert!(bus.snapshot_envelope().contains("\"revision\":77"));
    }

    #[test]
    fn runtime_events_update_snapshot_cache() {
        let bus = EventBus::new(16);
        bus.set_snapshot(ServiceSnapshot {
            recording: Some(RecordingSnapshot {
                session_id: "session-1".into(),
                ..RecordingSnapshot::default()
            }),
            ..ServiceSnapshot::default()
        });
        let publisher = bus.publisher();
        publisher.publish_recording_progress(1.25, 20_000);
        assert_eq!(
            bus.latest_snapshot().recording.unwrap().captured_frames,
            20_000
        );
        publisher.publish_pipeline_status(PipelineSnapshot {
            enabled: true,
            degraded: true,
            error: Some("worker failed".into()),
            ..PipelineSnapshot::default()
        });
        let snapshot = bus.latest_snapshot();
        assert!(snapshot.pipeline.degraded);
        assert_eq!(snapshot.pipeline.error.as_deref(), Some("worker failed"));
    }

    #[test]
    fn state_snapshot_preserves_progress_for_same_session() {
        let bus = EventBus::new(16);
        let publisher = bus.publisher();
        publisher.publish_state(&ServiceSnapshot {
            recording: Some(RecordingSnapshot {
                session_id: "session-1".into(),
                ..RecordingSnapshot::default()
            }),
            ..ServiceSnapshot::default()
        });
        publisher.publish_recording_progress(1.25, 20_000);

        publisher.publish_state(&ServiceSnapshot {
            phase: ServicePhase::Recording {
                session_id: "session-1".into(),
            },
            recording: Some(RecordingSnapshot {
                session_id: "session-1".into(),
                ..RecordingSnapshot::default()
            }),
            ..ServiceSnapshot::default()
        });

        let recording = bus.latest_snapshot().recording.unwrap();
        assert_eq!(recording.captured_frames, 20_000);
        assert_eq!(recording.elapsed_secs, 1.25);
    }
}
