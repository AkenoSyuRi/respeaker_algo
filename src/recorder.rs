//! Windows ReSpeaker 录音核心：WASAPI 独占采集 → 六通道拆分 → WAV 与可选 Pipeline。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::time::{Duration, Instant};

use crate::pipeline::{PipelineConfig, PipelineInputBlock, PipelineRuntime};
use crate::wav::WavSink;

const RESPEAKER_SAMPLE_RATE: u32 = 16_000;
const RESPEAKER_CHANNELS: u16 = 6;

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

/// 默认输出前缀：当前时间戳 `YYYYMMDD_HHMMSS`。
pub fn default_prefix() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

/// 执行一次固定 16 kHz / 16-bit / 6 通道 ReSpeaker 录音。
pub fn run_record(opts: &RecordOptions) -> Result<(), String> {
    // Pipeline 文件读取、TOML 解析和模块参数校验必须先于 WASAPI 采集。
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
    let mut pipeline = pipeline_config
        .map(|config| PipelineRuntime::new(config, out_dir, &prefix))
        .transpose()?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_handler = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        eprintln!("\n收到 Ctrl+C，正在停止录制…");
        stop_handler.store(true, Ordering::SeqCst);
    })
    .map_err(|e| format!("注册 Ctrl+C 处理器失败: {e}"))?;

    let (tx, rx) = sync_channel(128);
    let device = crate::wasapi::pick_respeaker_input_device()?;
    println!("输入设备: {}", device.name);
    let session = crate::wasapi::start_capture(
        device,
        RESPEAKER_SAMPLE_RATE,
        RESPEAKER_CHANNELS,
        tx.clone(),
        Arc::clone(&stop),
    )?;
    drop(tx);

    println!("输出目录: {out_dir}（前缀 {prefix}）");
    if opts.pipeline_config.is_some() {
        println!("内置算法 Pipeline 已启用");
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
    let frames = write_split_loop(
        &SplitOutputs {
            algo: &algo_path,
            mic: &mic_path,
            reference: &ref_path,
        },
        &rx,
        &stop,
        deadline,
        &mut pipeline,
    )?;
    drop(session);

    let size = |p: &str| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    println!(
        "录制完成: 约 {:.1}s / {} 帧\n  algo: {:.2} MB\n  mic:  {:.2} MB\n  ref:  {:.2} MB",
        started.elapsed().as_secs_f32(),
        frames,
        size(&algo_path) as f64 / 1e6,
        size(&mic_path) as f64 / 1e6,
        size(&ref_path) as f64 / 1e6,
    );
    Ok(())
}

/// 把 6 通道交织样本拆分为 algo（ch0）、mic（ch1..ch4）和 reference（ch5）。
fn split_6ch_into(
    samples: &[i16],
    algo: &mut Vec<i16>,
    mic: &mut Vec<i16>,
    reference: &mut Vec<i16>,
) -> Result<(), String> {
    if !samples.len().is_multiple_of(RESPEAKER_CHANNELS as usize) {
        return Err(format!("采样块长度 {} 不是 6 的整数倍", samples.len()));
    }
    algo.clear();
    mic.clear();
    reference.clear();
    let frames = samples.len() / RESPEAKER_CHANNELS as usize;
    algo.reserve(frames);
    mic.reserve(frames * 4);
    reference.reserve(frames);
    for frame in samples.chunks_exact(RESPEAKER_CHANNELS as usize) {
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

fn write_split_loop(
    out: &SplitOutputs<'_>,
    rx: &Receiver<Vec<i16>>,
    stop: &AtomicBool,
    deadline: Option<Instant>,
    pipeline: &mut Option<PipelineRuntime>,
) -> Result<u64, String> {
    let mut algo = WavSink::create(out.algo, 1, RESPEAKER_SAMPLE_RATE)?;
    let mut mic = WavSink::create(out.mic, 4, RESPEAKER_SAMPLE_RATE)?;
    let mut reference = WavSink::create(out.reference, 1, RESPEAKER_SAMPLE_RATE)?;
    let mut algo_buf = Vec::new();
    let mut mic_buf = Vec::new();
    let mut ref_buf = Vec::new();
    let mut frames = 0u64;

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if let Some(dl) = deadline
            && Instant::now() >= dl
        {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(samples) => {
                split_6ch_into(&samples, &mut algo_buf, &mut mic_buf, &mut ref_buf)?;
                if let Some(runtime) = pipeline.as_mut() {
                    runtime.push_block(PipelineInputBlock {
                        algo: &algo_buf,
                        mic: &mic_buf,
                        reference: &ref_buf,
                    })?;
                }
                algo.write_samples(&algo_buf)
                    .map_err(|e| format!("写入 {} 失败: {e}", out.algo))?;
                mic.write_samples(&mic_buf)
                    .map_err(|e| format!("写入 {} 失败: {e}", out.mic))?;
                reference
                    .write_samples(&ref_buf)
                    .map_err(|e| format!("写入 {} 失败: {e}", out.reference))?;
                frames += (samples.len() / RESPEAKER_CHANNELS as usize) as u64;
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("采集流已断开");
                break;
            }
        }
    }

    if let Some(runtime) = pipeline.as_mut() {
        runtime.finalize()?;
    }
    algo.finalize()
        .map_err(|e| format!("完成 {} 失败: {e}", out.algo))?;
    mic.finalize()
        .map_err(|e| format!("完成 {} 失败: {e}", out.mic))?;
    reference
        .finalize()
        .map_err(|e| format!("完成 {} 失败: {e}", out.reference))?;
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
