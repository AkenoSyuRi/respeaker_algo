//! Windows ReSpeaker 录音核心：WASAPI 独占采集 → 六通道拆分 → WAV 与可选算法 worker。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::time::{Duration, Instant};

use crate::audio::{
    CAPTURE_QUEUE_CAPACITY, CaptureBlock, RESPEAKER_CHANNELS, RESPEAKER_SAMPLE_RATE,
};
use crate::pipeline::PipelineConfig;
use crate::pipeline_worker::{PipelineWorkerHandle, PipelineWorkerStats};
use crate::wav::WavSink;

/// 录音参数（来自 CLI）。采样率、通道数和 WASAPI 后端固定为 ReSpeaker 所需值。
pub struct RecordOptions {
    /// 录制时长（秒），0 = 持续录制直到 Ctrl+C。
    pub duration: u64,
    /// 输出目录（默认 `target/out`）。
    pub out_dir: String,
    /// 输出文件前缀（默认当前时间戳，如 `20260809_143012`）。
    pub prefix: Option<String>,
    /// 可选的内置算法 Pipeline TOML。
    pub pipeline_config: Option<PathBuf>,
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

/// 执行一次固定 16 kHz / 16-bit / 6 通道 ReSpeaker 录音。
pub fn run_record(opts: &RecordOptions) -> Result<(), String> {
    let pipeline_config = opts
        .pipeline_config
        .as_deref()
        .map(PipelineConfig::load)
        .transpose()?;

    let prefix = opts.prefix.clone().unwrap_or_else(default_prefix);
    let out_dir = &opts.out_dir;
    std::fs::create_dir_all(out_dir).map_err(|e| format!("创建输出目录 {out_dir} 失败: {e}"))?;
    let algo_path = format!("{out_dir}/{prefix}_respeaker_algo.wav");
    let mic_path = format!("{out_dir}/{prefix}_respeaker_mic.wav");
    let ref_path = format!("{out_dir}/{prefix}_respeaker_ref.wav");

    let mut pipeline = match pipeline_config {
        Some(config) => PipelineDispatchState::Active(PipelineWorkerHandle::spawn(
            config,
            out_dir.to_string(),
            prefix.clone(),
        )?),
        None => PipelineDispatchState::Disabled,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let stop_handler = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        eprintln!("\n收到 Ctrl+C，正在停止录制…");
        stop_handler.store(true, Ordering::SeqCst);
    })
    .map_err(|e| format!("注册 Ctrl+C 处理器失败: {e}"))?;

    let (tx, rx) = sync_channel(CAPTURE_QUEUE_CAPACITY);
    let device = crate::wasapi::pick_respeaker_input_device()?;
    println!("输入设备: {}", device.name);
    let raw_writers = create_raw_wav_writers(&SplitOutputs {
        algo: &algo_path,
        mic: &mic_path,
        reference: &ref_path,
    })?;
    let mut session = match crate::wasapi::start_capture(
        device,
        RESPEAKER_SAMPLE_RATE,
        RESPEAKER_CHANNELS as u16,
        tx.clone(),
        Arc::clone(&stop),
    ) {
        Ok(session) => session,
        Err(error) => {
            let mut first_error = Some(error);
            finalize_raw_wav_writers(
                raw_writers,
                &SplitOutputs {
                    algo: &algo_path,
                    mic: &mic_path,
                    reference: &ref_path,
                },
                &mut first_error,
            );
            finish_pipeline(&mut pipeline, &mut first_error);
            return Err(first_error.expect("WASAPI 启动错误必须保留"));
        }
    };
    drop(tx);

    println!("输出目录: {out_dir}（前缀 {prefix}）");
    if opts.pipeline_config.is_some() {
        println!("内置算法 Pipeline 已启用（独立 algorithm worker）");
    }
    println!(
        "开始录制 ->\n  {algo_path}  (ch0 算法输出)\n  {mic_path}  (ch1-4 麦克风原始)\n  {ref_path}  (ch5 回放)\nCtrl+C 或 {} 秒后停止",
        if opts.duration > 0 {
            opts.duration.to_string()
        } else {
            "不限时".to_string()
        }
    );

    let deadline = (opts.duration > 0).then(|| Instant::now() + Duration::from_secs(opts.duration));
    let started = Instant::now();
    let result = write_split_loop(
        &SplitOutputs {
            algo: &algo_path,
            mic: &mic_path,
            reference: &ref_path,
        },
        &rx,
        &stop,
        deadline,
        raw_writers,
        &mut pipeline,
        &mut session,
    );

    let size = |p: &str| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    if let Ok(frames) = &result {
        println!(
            "录制完成: 约 {:.1}s / {} 帧\n  algo: {:.2} MB\n  mic:  {:.2} MB\n  ref:  {:.2} MB",
            started.elapsed().as_secs_f32(),
            frames,
            size(&algo_path) as f64 / 1e6,
            size(&mic_path) as f64 / 1e6,
            size(&ref_path) as f64 / 1e6,
        );
    }
    result.map(|_| ())
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

fn write_split_loop(
    out: &SplitOutputs<'_>,
    rx: &Receiver<CaptureBlock>,
    stop: &AtomicBool,
    deadline: Option<Instant>,
    mut raw_writers: RawWavWriters,
    pipeline: &mut PipelineDispatchState,
    session: &mut crate::wasapi::WasapiSession,
) -> Result<u64, String> {
    let mut algo_buf = Vec::new();
    let mut mic_buf = Vec::new();
    let mut ref_buf = Vec::new();
    let mut frames = 0u64;
    let mut first_error: Option<String> = None;
    let mut draining = false;

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
            }
        }

        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(block) => {
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
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if draining && stop.load(Ordering::SeqCst) {
                    // 仍可能有 in-flight 数据；继续直到 Disconnected。
                    continue;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // 优先 finalize raw WAV。
    finalize_raw_wav_writers(raw_writers, out, &mut first_error);

    let pipeline_stats = finish_pipeline(pipeline, &mut first_error);
    append_err(&mut first_error, session.stop_and_join());

    if let Some(stats) = pipeline_stats {
        println!(
            "algorithm worker: blocks={} frames={} hops={} max_queue_depth={}",
            stats.capture_blocks, stats.input_frames, stats.processed_hops, stats.max_queue_depth
        );
    }

    match first_error {
        Some(e) => Err(e),
        None => Ok(frames),
    }
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
                    first_error.get_or_insert(e);
                    None
                }
            }
        }
        PipelineDispatchState::Failed {
            mut worker,
            first_error: pipeline_err,
        } => {
            // first_error 优先于 finish() == Ok。worker 内部错误已在 try_push
            // 失败时记录过，finish() 若返回同一错误则不重复追加。
            first_error.get_or_insert(pipeline_err);
            match worker.finish() {
                Ok(stats) => Some(stats),
                Err(e) => {
                    if first_error.as_deref() != Some(e.as_str()) {
                        append_err(first_error, Err(e));
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

        finish_pipeline(&mut pipeline, &mut first_error);
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

        finish_pipeline(&mut pipeline, &mut first_error);
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
        finish_pipeline(&mut pipeline, &mut first_error);
        let final_error = first_error.expect("worker 错误必须保留");
        assert_eq!(final_error, pipeline_err);
        assert!(!final_error.contains("; "), "错误不应重复: {final_error}");
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
        finish_pipeline(&mut pipeline, &mut first_error);
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
