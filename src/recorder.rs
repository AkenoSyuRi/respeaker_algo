//! Windows ReSpeaker 录音核心：WASAPI 独占采集 → 六通道拆分 → WAV 与可选算法 worker。

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::time::{Duration, Instant};

use crate::app_config::RecordingConfig;
use crate::audio::{
    CAPTURE_QUEUE_CAPACITY, CaptureBlock, RESPEAKER_CHANNELS, RESPEAKER_SAMPLE_RATE,
};
use crate::beamformer::BeamformerStats;
use crate::events::{ErrorSource, EventPublisher, ServiceError};
use crate::pipeline::PipelineConfig;
use crate::pipeline_worker::{PipelineWorkerHandle, PipelineWorkerStats};
use crate::recordings::{SessionManifest, SessionResultStatus, check_no_clobber};
use crate::wav::WavSink;

pub struct RecordingRequest {
    pub recording: RecordingConfig,
    pub pipeline: Option<PipelineConfig>,
    pub session_id: String,
}

pub type RecordingReadyCallback = Box<dyn FnOnce(Result<(), String>) + Send>;
pub type RecordingStoppingCallback = Box<dyn FnOnce() + Send>;

pub struct RecordingControl {
    pub stop: Arc<AtomicBool>,
    pub events: EventPublisher,
    pub ready: Option<RecordingReadyCallback>,
    pub stopping: Option<RecordingStoppingCallback>,
}

#[derive(Clone, Debug)]
pub struct RecordingSummary {
    pub session_id: String,
    pub prefix: String,
    pub out_dir: String,
    pub captured_frames: u64,
    pub pipeline_stats: Option<PipelineWorkerStats>,
    pub bf_stats: Option<BeamformerStats>,
}

enum PipelineDispatchState {
    Disabled,
    Active(PipelineWorkerHandle),
    Failed {
        worker: PipelineWorkerHandle,
        first_error: String,
    },
}

struct ChannelWriters<'a> {
    algo: &'a mut WavSink,
    mic: &'a mut WavSink,
    reference: &'a mut WavSink,
    algo_buf: &'a mut Vec<i16>,
    mic_buf: &'a mut Vec<i16>,
    ref_buf: &'a mut Vec<i16>,
}

struct RawWavWriters {
    algo: WavSink,
    mic: WavSink,
    reference: WavSink,
}

/// 默认输出前缀：当前时间戳 `YYYYMMDD_HHMMSS`。
pub fn default_prefix() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

/// 执行一次录音。该函数不注册 Ctrl+C，所有生命周期由 Controller 管理。
pub fn run_recording(
    request: RecordingRequest,
    mut control: RecordingControl,
) -> Result<RecordingSummary, String> {
    request.recording.validate()?;
    if let Some(config) = &request.pipeline {
        config.validate()?;
    }
    crate::recordings::validate_session_id(&request.session_id)?;
    let prefix = request
        .recording
        .prefix
        .clone()
        .unwrap_or_else(default_prefix);
    crate::app_config::validate_prefix(&prefix)?;
    let out_dir = request.recording.out_dir.clone();
    let app_config = crate::app_config::AppConfig {
        recording: request.recording.clone(),
        pipeline_enabled: request.pipeline.is_some(),
        pipeline: request.pipeline.clone().unwrap_or_default(),
        ..crate::app_config::AppConfig::default()
    };
    check_no_clobber(&out_dir, &prefix, &app_config)?;
    fs::create_dir_all(&out_dir).map_err(|e| format!("创建输出目录 {out_dir} 失败: {e}"))?;

    let started_at = chrono::Local::now().to_rfc3339();
    let mut manifest = SessionManifest::new(
        request.session_id.clone(),
        prefix.clone(),
        started_at,
        app_config,
    );
    manifest.save_atomic(&out_dir)?;

    let algo_path = format!("{out_dir}/{prefix}_respeaker_algo.wav");
    let mic_path = format!("{out_dir}/{prefix}_respeaker_mic.wav");
    let reference_path = format!("{out_dir}/{prefix}_respeaker_ref.wav");
    let outputs = SplitOutputs {
        algo: &algo_path,
        mic: &mic_path,
        reference: &reference_path,
    };
    // 先确认设备，再创建输出；失败时只更新 starting manifest，不遗留半成品 WAV。
    let device = match crate::wasapi::pick_respeaker_input_device() {
        Ok(device) => device,
        Err(error) => {
            return startup_failure(manifest, &out_dir, control, ErrorSource::Device, error);
        }
    };
    let raw_writers = match create_raw_wav_writers(&outputs) {
        Ok(writers) => writers,
        Err(error) => {
            return startup_failure(manifest, &out_dir, control, ErrorSource::RawWav, error);
        }
    };
    let mut pipeline = match request.pipeline {
        Some(config) => match PipelineWorkerHandle::spawn_with_publisher(
            config,
            out_dir.clone(),
            prefix.clone(),
            Some(control.events.clone()),
        ) {
            Ok(worker) => PipelineDispatchState::Active(worker),
            Err(error) => {
                let mut first_error = Some(error.clone());
                finalize_raw_wav_writers(raw_writers, &outputs, &mut first_error);
                return startup_failure(manifest, &out_dir, control, ErrorSource::Pipeline, error);
            }
        },
        None => PipelineDispatchState::Disabled,
    };
    let (tx, rx) = sync_channel(CAPTURE_QUEUE_CAPACITY);
    let mut session = match crate::wasapi::start_capture(
        device,
        RESPEAKER_SAMPLE_RATE,
        RESPEAKER_CHANNELS as u16,
        tx.clone(),
        control.stop.clone(),
    ) {
        Ok(session) => session,
        Err(error) => {
            let mut first_error = Some(error.clone());
            finalize_raw_wav_writers(raw_writers, &outputs, &mut first_error);
            finish_pipeline(&mut pipeline, &mut first_error, &control.events);
            return startup_failure(manifest, &out_dir, control, ErrorSource::Capture, error);
        }
    };
    drop(tx);
    manifest.status = SessionResultStatus::Recording;
    manifest.refresh_files(&out_dir);
    if let Err(error) = manifest.save_atomic(&out_dir) {
        let message = format!("保存 recording manifest 失败: {error}");
        control.stop.store(true, Ordering::SeqCst);
        let mut first_error = Some(message.clone());
        append_err(&mut first_error, session.stop_and_join());
        finalize_raw_wav_writers(raw_writers, &outputs, &mut first_error);
        finish_pipeline(&mut pipeline, &mut first_error, &control.events);
        return startup_failure(
            manifest,
            &out_dir,
            control,
            ErrorSource::RawWav,
            first_error.unwrap_or(message),
        );
    }
    if let Some(ready) = control.ready.take() {
        ready(Ok(()));
    }

    let started = Instant::now();
    let deadline = (request.recording.duration_seconds > 0)
        .then(|| started + Duration::from_secs(request.recording.duration_seconds));
    let loop_result = write_split_loop_with_events(
        &outputs,
        &rx,
        &control.stop,
        deadline,
        raw_writers,
        &mut pipeline,
        &mut session,
        &control.events,
        control.stopping.take(),
        started,
    );
    manifest.captured_frames = loop_result.captured_frames;
    manifest.pipeline_stats = loop_result.pipeline_stats;
    manifest.bf_stats = manifest
        .pipeline_stats
        .as_ref()
        .and_then(|stats| stats.bf_stats.clone());
    manifest.status = if loop_result.error.is_some() {
        SessionResultStatus::Failed
    } else {
        SessionResultStatus::Success
    };
    manifest.error = loop_result.error.as_ref().map(|error| {
        ServiceError::new(loop_result.error_source, "recording_failed", error.clone())
    });
    manifest.finished_at = Some(chrono::Local::now().to_rfc3339());
    manifest.refresh_files(&out_dir);
    let manifest_error = manifest.save_atomic(&out_dir).err();
    if let Some(error) = manifest_error
        && loop_result.error.is_none()
    {
        return Err(error);
    }
    if let Some(error) = loop_result.error {
        return Err(error);
    }
    Ok(RecordingSummary {
        session_id: request.session_id,
        prefix,
        out_dir,
        captured_frames: manifest.captured_frames,
        pipeline_stats: manifest.pipeline_stats,
        bf_stats: manifest.bf_stats,
    })
}

fn startup_failure<T>(
    mut manifest: SessionManifest,
    out_dir: &str,
    mut control: RecordingControl,
    source: ErrorSource,
    error: String,
) -> Result<T, String> {
    if let Some(ready) = control.ready.take() {
        ready(Err(error.clone()));
    }
    manifest.status = SessionResultStatus::Failed;
    manifest.error = Some(ServiceError::new(source, "startup_failed", error.clone()));
    manifest.finished_at = Some(chrono::Local::now().to_rfc3339());
    manifest.refresh_files(out_dir);
    let _ = manifest.save_atomic(out_dir);
    control
        .events
        .publish_error(ServiceError::new(source, "startup_failed", error.clone()));
    Err(error)
}

struct LoopResult {
    captured_frames: u64,
    pipeline_stats: Option<PipelineWorkerStats>,
    error: Option<String>,
    error_source: ErrorSource,
}

#[allow(clippy::too_many_arguments)]
fn write_split_loop_with_events(
    out: &SplitOutputs<'_>,
    rx: &Receiver<CaptureBlock>,
    stop: &AtomicBool,
    deadline: Option<Instant>,
    mut raw_writers: RawWavWriters,
    pipeline: &mut PipelineDispatchState,
    session: &mut crate::wasapi::WasapiSession,
    publisher: &EventPublisher,
    mut stopping: Option<Box<dyn FnOnce() + Send>>,
    started: Instant,
) -> LoopResult {
    let mut algo_buf = Vec::new();
    let mut mic_buf = Vec::new();
    let mut ref_buf = Vec::new();
    let mut frames = 0u64;
    let mut first_error: Option<String> = None;
    let mut draining = false;
    let mut last_progress = started;

    loop {
        if !draining {
            if stop.load(Ordering::SeqCst) {
                draining = true;
            }
            if let Some(dl) = deadline
                && Instant::now() >= dl
            {
                stop.store(true, Ordering::SeqCst);
                draining = true;
                notify_stopping(&mut stopping);
            }
        }
        if last_progress.elapsed() >= Duration::from_millis(200) {
            publisher.publish_recording_progress(started.elapsed().as_secs_f64(), frames);
            last_progress = Instant::now();
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(block) => {
                let pipeline_was_active = matches!(pipeline, PipelineDispatchState::Active(_));
                if process_received_block(
                    &block,
                    out,
                    &mut raw_writers,
                    &mut algo_buf,
                    &mut mic_buf,
                    &mut ref_buf,
                    pipeline,
                    &mut first_error,
                    stop,
                    &mut draining,
                ) {
                    frames += block.frames as u64;
                } else {
                    notify_stopping(&mut stopping);
                }
                if pipeline_was_active
                    && let PipelineDispatchState::Failed { first_error: e, .. } = pipeline
                {
                    publisher.publish_error(ServiceError::new(
                        ErrorSource::Pipeline,
                        "pipeline_degraded",
                        e.clone(),
                    ));
                    publisher.publish_pipeline_status(crate::events::PipelineSnapshot {
                        enabled: true,
                        degraded: true,
                        error: Some(e.clone()),
                        ..crate::events::PipelineSnapshot::default()
                    });
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if draining && stop.load(Ordering::SeqCst) {
                    continue;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                append_err(&mut first_error, session.stop_and_join());
                if first_error.is_some() {
                    notify_stopping(&mut stopping);
                }
                break;
            }
        }
    }
    finalize_raw_wav_writers(raw_writers, out, &mut first_error);
    let pipeline_stats = finish_pipeline(pipeline, &mut first_error, publisher);
    append_err(&mut first_error, session.stop_and_join());
    if first_error.as_deref().is_some_and(|error| {
        matches!(
            classify_error(error),
            ErrorSource::Capture | ErrorSource::RawWav
        )
    }) {
        notify_stopping(&mut stopping);
    }
    let error_source = first_error
        .as_deref()
        .map(classify_error)
        .unwrap_or(ErrorSource::Capture);
    LoopResult {
        captured_frames: frames,
        pipeline_stats,
        error: first_error,
        error_source,
    }
}

fn notify_stopping(stopping: &mut Option<Box<dyn FnOnce() + Send>>) {
    if let Some(callback) = stopping.take() {
        callback();
    }
}

fn classify_error(error: &str) -> ErrorSource {
    let lower = error.to_ascii_lowercase();
    if lower.contains("pipeline") || lower.contains("algorithm worker") {
        ErrorSource::Pipeline
    } else if lower.contains("wav") || error.contains("写入") || error.contains("完成") {
        ErrorSource::RawWav
    } else {
        ErrorSource::Capture
    }
}

/// 把 6 通道交织样本拆分为 algo（ch0）、mic（ch1..ch4）和 reference（ch5）。
fn split_6ch_into(
    samples: &[i16],
    algo: &mut Vec<i16>,
    mic: &mut Vec<i16>,
    reference: &mut Vec<i16>,
) -> Result<(), String> {
    if !samples.len().is_multiple_of(RESPEAKER_CHANNELS) {
        return Err(format!("采样块长度 {} 不是 6 的整数倍", samples.len()));
    }
    algo.clear();
    mic.clear();
    reference.clear();
    let frames = samples.len() / RESPEAKER_CHANNELS;
    algo.reserve(frames);
    mic.reserve(frames * 4);
    reference.reserve(frames);
    for frame in samples.chunks_exact(RESPEAKER_CHANNELS) {
        algo.push(frame[0]);
        mic.extend_from_slice(&frame[1..5]);
        reference.push(frame[5]);
    }
    Ok(())
}

struct SplitOutputs<'a> {
    algo: &'a str,
    mic: &'a str,
    reference: &'a str,
}

fn create_raw_wav_writers(out: &SplitOutputs<'_>) -> Result<RawWavWriters, String> {
    let mut algo = WavSink::create(out.algo, 1, RESPEAKER_SAMPLE_RATE)?;
    let mut mic = match WavSink::create(out.mic, 4, RESPEAKER_SAMPLE_RATE) {
        Ok(writer) => writer,
        Err(error) => {
            let _ = algo.finalize();
            return Err(error);
        }
    };
    let reference = match WavSink::create(out.reference, 1, RESPEAKER_SAMPLE_RATE) {
        Ok(writer) => writer,
        Err(error) => {
            let _ = algo.finalize();
            let _ = mic.finalize();
            return Err(error);
        }
    };
    Ok(RawWavWriters {
        algo,
        mic,
        reference,
    })
}

#[allow(clippy::too_many_arguments)]
fn process_received_block(
    block: &CaptureBlock,
    out: &SplitOutputs<'_>,
    raw_writers: &mut RawWavWriters,
    algo_buf: &mut Vec<i16>,
    mic_buf: &mut Vec<i16>,
    ref_buf: &mut Vec<i16>,
    pipeline: &mut PipelineDispatchState,
    first_error: &mut Option<String>,
    stop: &AtomicBool,
    draining: &mut bool,
) -> bool {
    match process_capture_block(
        block,
        out,
        &mut ChannelWriters {
            algo: &mut raw_writers.algo,
            mic: &mut raw_writers.mic,
            reference: &mut raw_writers.reference,
            algo_buf,
            mic_buf,
            ref_buf,
        },
        pipeline,
    ) {
        Ok(()) => true,
        Err(error) => {
            first_error.get_or_insert(error);
            stop.store(true, Ordering::SeqCst);
            *draining = true;
            false
        }
    }
}

fn finalize_raw_wav_writers(
    mut writers: RawWavWriters,
    out: &SplitOutputs<'_>,
    first_error: &mut Option<String>,
) {
    append_err(
        first_error,
        writers
            .algo
            .finalize()
            .map_err(|e| format!("完成 {} 失败: {e}", out.algo)),
    );
    append_err(
        first_error,
        writers
            .mic
            .finalize()
            .map_err(|e| format!("完成 {} 失败: {e}", out.mic)),
    );
    append_err(
        first_error,
        writers
            .reference
            .finalize()
            .map_err(|e| format!("完成 {} 失败: {e}", out.reference)),
    );
}

fn process_capture_block(
    block: &CaptureBlock,
    out: &SplitOutputs<'_>,
    writers: &mut ChannelWriters<'_>,
    pipeline: &mut PipelineDispatchState,
) -> Result<(), String> {
    block.validate()?;
    split_6ch_into(
        &block.samples,
        writers.algo_buf,
        writers.mic_buf,
        writers.ref_buf,
    )?;
    writers
        .algo
        .write_samples(writers.algo_buf)
        .map_err(|e| format!("写入 {} 失败: {e}", out.algo))?;
    writers
        .mic
        .write_samples(writers.mic_buf)
        .map_err(|e| format!("写入 {} 失败: {e}", out.mic))?;
    writers
        .reference
        .write_samples(writers.ref_buf)
        .map_err(|e| format!("写入 {} 失败: {e}", out.reference))?;

    if let PipelineDispatchState::Active(worker) = pipeline
        && let Err(e) = worker.try_push(block.clone())
    {
        eprintln!("Pipeline 已停用，raw 录音继续: {e}");
        // 取出 worker，关闭输入，切到 Failed；不 stop capture。
        let mut tmp = PipelineDispatchState::Disabled;
        std::mem::swap(pipeline, &mut tmp);
        if let PipelineDispatchState::Active(mut worker) = tmp {
            worker.close_input();
            *pipeline = PipelineDispatchState::Failed {
                worker,
                first_error: e,
            };
        }
    }
    Ok(())
}

fn finish_pipeline(
    pipeline: &mut PipelineDispatchState,
    first_error: &mut Option<String>,
    publisher: &EventPublisher,
) -> Option<PipelineWorkerStats> {
    let mut tmp = PipelineDispatchState::Disabled;
    std::mem::swap(pipeline, &mut tmp);
    match tmp {
        PipelineDispatchState::Disabled => None,
        PipelineDispatchState::Active(mut worker) => {
            worker.close_input();
            match worker.finish() {
                Ok(stats) => Some(stats),
                Err(e) => {
                    first_error.get_or_insert(e.clone());
                    publisher.publish_error(ServiceError::new(
                        ErrorSource::Pipeline,
                        "pipeline_failed",
                        e,
                    ));
                    None
                }
            }
        }
        PipelineDispatchState::Failed {
            mut worker,
            first_error: pipeline_err,
        } => {
            // worker 错误只停用 Pipeline，不停止当前 capture；但保留首错供
            // manifest 和 Controller 在 session finalize 后报告。
            first_error.get_or_insert(pipeline_err.clone());
            match worker.finish() {
                Ok(stats) => Some(stats),
                Err(e) => {
                    if e != pipeline_err {
                        first_error.get_or_insert(e.clone());
                        publisher.publish_error(ServiceError::new(
                            ErrorSource::Pipeline,
                            "pipeline_failed",
                            e,
                        ));
                        publisher.publish_pipeline_status(crate::events::PipelineSnapshot {
                            enabled: true,
                            degraded: true,
                            error: Some(pipeline_err.clone()),
                            ..crate::events::PipelineSnapshot::default()
                        });
                    }
                    None
                }
            }
        }
    }
}

fn append_err(first: &mut Option<String>, result: Result<(), String>) {
    if let Err(e) = result {
        match first {
            Some(existing) => {
                existing.push_str("; ");
                existing.push_str(&e);
            }
            None => *first = Some(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;
    use std::thread;

    fn read_wav_samples(path: &str) -> Vec<i16> {
        hound::WavReader::open(path)
            .unwrap()
            .samples::<i16>()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn split_6ch_into_works() {
        let samples: Vec<i16> = (0..12).collect();
        let mut algo = Vec::new();
        let mut mic = Vec::new();
        let mut reference = Vec::new();
        split_6ch_into(&samples, &mut algo, &mut mic, &mut reference).unwrap();
        assert_eq!(algo, vec![0, 6]);
        assert_eq!(mic, vec![1, 2, 3, 4, 7, 8, 9, 10]);
        assert_eq!(reference, vec![5, 11]);
    }

    #[test]
    fn split_6ch_into_rejects_non_multiple_of_6() {
        let samples: Vec<i16> = (0..14).collect();
        let mut algo = Vec::new();
        let mut mic = Vec::new();
        let mut reference = Vec::new();
        assert!(split_6ch_into(&samples, &mut algo, &mut mic, &mut reference).is_err());
    }

    #[test]
    fn default_prefix_format() {
        let prefix = default_prefix();
        assert_eq!(prefix.len(), 15);
        assert_eq!(&prefix[8..9], "_");
        assert!(
            prefix
                .chars()
                .enumerate()
                .all(|(i, c)| c.is_ascii_digit() || (i == 8 && c == '_'))
        );
    }

    #[test]
    fn pipeline_full_disables_pipeline_but_raw_recording_continues() {
        let dir = "target/out/recorder_pipeline_full";
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let algo_path = format!("{dir}/t_respeaker_algo.wav");
        let mic_path = format!("{dir}/t_respeaker_mic.wav");
        let ref_path = format!("{dir}/t_respeaker_ref.wav");

        let out = SplitOutputs {
            algo: &algo_path,
            mic: &mic_path,
            reference: &ref_path,
        };
        let mut raw = create_raw_wav_writers(&out).unwrap();
        let (worker, stalled_rx) = PipelineWorkerHandle::stalled_for_test(1);
        let mut pipeline = PipelineDispatchState::Active(worker);
        let mut algo_buf = Vec::new();
        let mut mic_buf = Vec::new();
        let mut ref_buf = Vec::new();
        let stop = AtomicBool::new(false);
        let mut draining = false;
        let mut first_error = None;

        for sequence in 0..2u64 {
            let value = sequence as i16 + 1;
            let block = CaptureBlock::from_samples(sequence, sequence, vec![value; 6]).unwrap();
            assert!(process_received_block(
                &block,
                &out,
                &mut raw,
                &mut algo_buf,
                &mut mic_buf,
                &mut ref_buf,
                &mut pipeline,
                &mut first_error,
                &stop,
                &mut draining,
            ));
            if sequence == 0 {
                assert!(matches!(pipeline, PipelineDispatchState::Active(_)));
            }
        }
        assert!(matches!(pipeline, PipelineDispatchState::Failed { .. }));
        assert_eq!(stalled_rx.try_recv().unwrap().sequence, 0);

        let block = CaptureBlock::from_samples(2, 2, vec![3; 6]).unwrap();
        assert!(process_received_block(
            &block,
            &out,
            &mut raw,
            &mut algo_buf,
            &mut mic_buf,
            &mut ref_buf,
            &mut pipeline,
            &mut first_error,
            &stop,
            &mut draining,
        ));
        assert!(matches!(
            stalled_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty | std::sync::mpsc::TryRecvError::Disconnected)
        ));
        assert!(!stop.load(Ordering::SeqCst));
        assert!(!draining);
        assert!(first_error.is_none());

        finalize_raw_wav_writers(raw, &out, &mut first_error);
        assert_eq!(read_wav_samples(&algo_path), vec![1, 2, 3]);
        assert_eq!(
            read_wav_samples(&mic_path),
            vec![1; 4]
                .into_iter()
                .chain(vec![2; 4])
                .chain(vec![3; 4])
                .collect::<Vec<_>>()
        );
        assert_eq!(read_wav_samples(&ref_path), vec![1, 2, 3]);

        finish_pipeline(
            &mut pipeline,
            &mut first_error,
            &crate::events::EventBus::new(1).publisher(),
        );
        assert_eq!(first_error.as_deref(), Some("Pipeline queue overrun"));
    }

    #[test]
    fn pipeline_failure_is_returned_after_raw_wavs_are_finalized() {
        let dir = "target/out/recorder_finalize_before_pipeline_error";
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let algo_path = format!("{dir}/f_respeaker_algo.wav");
        let mic_path = format!("{dir}/f_respeaker_mic.wav");
        let ref_path = format!("{dir}/f_respeaker_ref.wav");
        let out = SplitOutputs {
            algo: &algo_path,
            mic: &mic_path,
            reference: &ref_path,
        };
        let mut raw = create_raw_wav_writers(&out).unwrap();
        raw.algo.write_samples(&[1, 2]).unwrap();
        raw.mic.write_samples(&[1; 8]).unwrap();
        raw.reference.write_samples(&[3, 4]).unwrap();
        let (worker, _rx) = PipelineWorkerHandle::stalled_for_test(1);
        let mut pipeline = PipelineDispatchState::Failed {
            worker,
            first_error: "synthetic pipeline error".into(),
        };
        let mut first_error = None;

        finalize_raw_wav_writers(raw, &out, &mut first_error);
        assert_eq!(read_wav_samples(&algo_path), vec![1, 2]);
        assert!(first_error.is_none());

        finish_pipeline(
            &mut pipeline,
            &mut first_error,
            &crate::events::EventBus::new(1).publisher(),
        );
        assert_eq!(first_error.as_deref(), Some("synthetic pipeline error"));
    }

    #[test]
    fn pipeline_worker_error_is_not_duplicated_at_finish() {
        let config = PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "doa"
enable_viewer = false
"#,
        )
        .unwrap();
        let worker =
            PipelineWorkerHandle::spawn(config, "target/out".into(), "dedup_err".into()).unwrap();
        worker
            .try_push(CaptureBlock::from_samples(0, 0, vec![0i16; 6 * 16]).unwrap())
            .unwrap();
        worker
            .try_push(CaptureBlock::from_samples(2, 16, vec![0i16; 6 * 16]).unwrap())
            .unwrap();

        // worker 检测到不连续块后发布内部错误，此后 try_push 返回同一错误。
        let mut pipeline_err = None;
        for _ in 0..100_000 {
            match worker.try_push(CaptureBlock::from_samples(3, 32, vec![0i16; 6 * 16]).unwrap()) {
                Err(e) if e.contains("不连续") => {
                    pipeline_err = Some(e);
                    break;
                }
                _ => thread::yield_now(),
            }
        }
        let pipeline_err = pipeline_err.expect("worker 应在 finish 前发布内部错误");

        let mut pipeline = PipelineDispatchState::Failed {
            worker,
            first_error: pipeline_err.clone(),
        };
        let mut first_error = None;
        finish_pipeline(
            &mut pipeline,
            &mut first_error,
            &crate::events::EventBus::new(1).publisher(),
        );
        assert_eq!(first_error, Some(pipeline_err));
    }

    #[test]
    fn pipeline_disconnect_disables_pipeline_but_raw_recording_continues() {
        let dir = "target/out/recorder_pipeline_disconnect";
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let algo_path = format!("{dir}/nr_respeaker_algo.wav");
        let mic_path = format!("{dir}/nr_respeaker_mic.wav");
        let ref_path = format!("{dir}/nr_respeaker_ref.wav");
        let out = SplitOutputs {
            algo: &algo_path,
            mic: &mic_path,
            reference: &ref_path,
        };
        let mut raw = create_raw_wav_writers(&out).unwrap();
        let (worker, stalled_rx) = PipelineWorkerHandle::stalled_for_test(1);
        drop(stalled_rx);
        let mut pipeline = PipelineDispatchState::Active(worker);
        let mut a = Vec::new();
        let mut m = Vec::new();
        let mut r = Vec::new();
        let stop = AtomicBool::new(false);
        let mut draining = false;
        let mut first_error = None;
        for i in 0..3u64 {
            let block = CaptureBlock::from_samples(i, i, vec![i as i16; 6]).unwrap();
            assert!(process_received_block(
                &block,
                &out,
                &mut raw,
                &mut a,
                &mut m,
                &mut r,
                &mut pipeline,
                &mut first_error,
                &stop,
                &mut draining,
            ));
        }
        assert!(matches!(pipeline, PipelineDispatchState::Failed { .. }));
        assert!(!stop.load(Ordering::SeqCst));
        finalize_raw_wav_writers(raw, &out, &mut first_error);
        assert_eq!(read_wav_samples(&algo_path), vec![0, 1, 2]);
        finish_pipeline(
            &mut pipeline,
            &mut first_error,
            &crate::events::EventBus::new(1).publisher(),
        );
        assert!(first_error.unwrap().contains("disconnected"));
    }

    #[test]
    fn raw_write_error_stops_capture_and_finalizes_headers_best_effort() {
        let dir = "target/out/recorder_raw_error";
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let algo_path = format!("{dir}/e_respeaker_algo.wav");
        let mic_path = format!("{dir}/e_respeaker_mic.wav");
        let ref_path = format!("{dir}/e_respeaker_ref.wav");
        let out = SplitOutputs {
            algo: &algo_path,
            mic: &mic_path,
            reference: &ref_path,
        };
        let mut raw = create_raw_wav_writers(&out).unwrap();
        raw.mic.fail_after_samples_for_test(0);
        let mut pipeline = PipelineDispatchState::Disabled;
        let mut a = Vec::new();
        let mut m = Vec::new();
        let mut r = Vec::new();
        let stop = AtomicBool::new(false);
        let mut draining = false;
        let mut first_error = None;
        let block = CaptureBlock::from_samples(0, 0, vec![7i16; 6]).unwrap();

        assert!(!process_received_block(
            &block,
            &out,
            &mut raw,
            &mut a,
            &mut m,
            &mut r,
            &mut pipeline,
            &mut first_error,
            &stop,
            &mut draining,
        ));
        assert!(stop.load(Ordering::SeqCst));
        assert!(draining);
        assert!(first_error.as_ref().unwrap().contains(&mic_path));

        finalize_raw_wav_writers(raw, &out, &mut first_error);
        assert_eq!(read_wav_samples(&algo_path), vec![7]);
        assert!(read_wav_samples(&mic_path).is_empty());
        assert!(read_wav_samples(&ref_path).is_empty());
    }

    #[test]
    fn no_pipeline_config_creates_no_algorithm_outputs() {
        let dir = "target/out/recorder_no_pipeline";
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let algo_path = format!("{dir}/n_respeaker_algo.wav");
        let mic_path = format!("{dir}/n_respeaker_mic.wav");
        let ref_path = format!("{dir}/n_respeaker_ref.wav");
        let out = SplitOutputs {
            algo: &algo_path,
            mic: &mic_path,
            reference: &ref_path,
        };
        let mut raw = create_raw_wav_writers(&out).unwrap();
        let mut pipeline = PipelineDispatchState::Disabled;
        let mut a = Vec::new();
        let mut m = Vec::new();
        let mut r = Vec::new();
        process_capture_block(
            &CaptureBlock::from_samples(0, 0, vec![0i16; 6]).unwrap(),
            &out,
            &mut ChannelWriters {
                algo: &mut raw.algo,
                mic: &mut raw.mic,
                reference: &mut raw.reference,
                algo_buf: &mut a,
                mic_buf: &mut m,
                ref_buf: &mut r,
            },
            &mut pipeline,
        )
        .unwrap();
        let mut first_error = None;
        finalize_raw_wav_writers(raw, &out, &mut first_error);
        assert!(first_error.is_none());
        assert!(!std::path::Path::new(&format!("{dir}/n_respeaker_bf.wav")).exists());
        assert!(!std::path::Path::new(&format!("{dir}/n_respeaker_doa.csv")).exists());
    }

    #[test]
    fn capture_queue_capacity_constant() {
        assert_eq!(CAPTURE_QUEUE_CAPACITY, 128);
        let (_tx, _rx) = sync_channel::<CaptureBlock>(CAPTURE_QUEUE_CAPACITY);
        let _ = thread::spawn(|| {});
    }
}
