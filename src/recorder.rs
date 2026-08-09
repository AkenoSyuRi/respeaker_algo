//! 录音核心：采集 → 拆分 → 多 WAV 写入。
//!
//! `6_channels_firmware.bin` 的输出布局（16kHz / 16-bit / 6 通道交织）：
//! - ch0: 算法处理后的音频（用于 ASR）→ `*_respeaker_algo.wav`
//! - ch1..=ch4: mic1..=mic4 原始数据（DOA / BF 输入）→ `*_respeaker_mic.wav`（4ch）
//! - ch5: 回放数据（AEC 参考信号）→ `*_respeaker_ref.wav`
//!
//! 默认录制时长 0 = 持续录制直到 Ctrl+C。16ms 帧 = 256 采样 @16kHz，
//! 后续实时 DOA / BF 可按此对齐消费 ch1–ch4。
//!
//! 采集后端：
//! - `Cpal`：跨平台共享模式（Linux ALSA 可直接拿到 6ch/16k；Windows 上
//!   系统混音器降级为 48kHz/2ch）；
//! - `Wasapi`（仅 Windows）：WASAPI 独占模式，可拿到固件完整的 6ch/16k。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{
    Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig, SupportedStreamConfig,
};

use crate::audio;
use crate::doa::DoaRunOptions;
use crate::doa::output::DoaRuntime;
use crate::wav::WavSink;

/// 录音参数（来自 CLI）。
pub struct RecordOptions {
    pub device: Option<String>,
    /// 录制时长（秒），0 = 持续录制直到 Ctrl+C。
    pub duration: u64,
    pub rate: u32,
    pub channels: u16,
    pub backend: BackendChoice,
    /// 输出目录（默认 `target/out`）。
    pub out_dir: String,
    /// 输出文件前缀（默认当前时间戳，如 `20260715_143012`）。
    pub prefix: Option<String>,
    /// 实时 DOA 选项（默认关闭）。
    pub doa: DoaRunOptions,
}

/// 后端选择（CLI `--backend`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackendChoice {
    /// 自动：Windows 默认 WASAPI 独占（目标 6ch/16k），其它平台用 cpal。
    Auto,
    /// cpal 跨平台共享模式。
    Cpal,
    /// WASAPI 独占模式（仅 Windows）。
    Wasapi,
}

enum Backend {
    Cpal,
    #[cfg(windows)]
    Wasapi,
}

/// 采集资源句柄；Drop 时停止采集（字段仅用于持有资源以管理生命周期）。
enum CaptureHandle {
    #[allow(dead_code)]
    Cpal { stream: Stream },
    #[cfg(windows)]
    #[allow(dead_code)]
    Wasapi {
        session: crate::wasapi::WasapiSession,
    },
}

fn resolve_backend(choice: BackendChoice) -> Result<Backend, String> {
    match choice {
        BackendChoice::Cpal => Ok(Backend::Cpal),
        #[cfg(windows)]
        BackendChoice::Wasapi => Ok(Backend::Wasapi),
        #[cfg(not(windows))]
        BackendChoice::Wasapi => Err("WASAPI 独占模式仅支持 Windows".into()),
        BackendChoice::Auto => {
            #[cfg(windows)]
            {
                Ok(Backend::Wasapi)
            }
            #[cfg(not(windows))]
            {
                Ok(Backend::Cpal)
            }
        }
    }
}

/// 默认输出前缀：当前时间戳 `YYYYMMDD_HHMMSS`。
pub fn default_prefix() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

/// 执行一次录音。
///
/// 6 通道时拆分为 3 个文件（algo / mic / ref）；其它通道数回退为单个
/// `*_respeaker_mix.wav`。
pub fn run_record(opts: &RecordOptions) -> Result<(), String> {
    // DOA 参数校验提前到采集启动之前
    if opts.doa.enabled {
        opts.doa.config.validate()?;
    }
    let backend = resolve_backend(opts.backend)?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_handler = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        eprintln!("\n收到 Ctrl+C，正在停止录制…");
        stop_handler.store(true, Ordering::SeqCst);
    })
    .map_err(|e| format!("注册 Ctrl+C 处理器失败: {e}"))?;

    let (tx, rx): (SyncSender<Vec<i16>>, Receiver<Vec<i16>>) = sync_channel(128);

    // 启动采集后端，得到实际采样率/通道数与资源句柄。
    let (handle, actual_rate, actual_ch): (CaptureHandle, u32, u16) = match backend {
        Backend::Cpal => {
            let device = audio::pick_input_device(opts.device.as_deref())?;
            let name = device.name().unwrap_or_else(|_| "?".to_string());
            let cfg = audio::find_stream_config(&device, opts.rate, opts.channels)?;
            let rate = cfg.sample_rate().0;
            let ch = cfg.channels();
            println!("输入设备: {name}");
            println!(
                "流配置: {rate} Hz / {ch} 通道 / {:?}（cpal 共享模式）",
                cfg.sample_format()
            );
            if rate != opts.rate || ch != opts.channels {
                eprintln!(
                    "注意: 实际采样率/通道数与请求不一致，输出文件按实际值 {rate}Hz/{ch}ch 写入"
                );
            }
            let stream = build_stream(&device, &cfg, tx.clone(), Arc::clone(&stop))?;
            stream.play().map_err(|e| format!("启动音频流失败: {e}"))?;
            (CaptureHandle::Cpal { stream }, rate, ch)
        }
        #[cfg(windows)]
        Backend::Wasapi => {
            let dev = crate::wasapi::pick_input_device(opts.device.as_deref())?;
            println!("输入设备: {}", dev.name);
            let session = crate::wasapi::start_capture(
                dev,
                opts.rate,
                opts.channels,
                tx.clone(),
                Arc::clone(&stop),
            )?;
            (CaptureHandle::Wasapi { session }, opts.rate, opts.channels)
        }
    };
    drop(tx); // 仅保留采集端的发送者

    // 输出路径：{out_dir}/{prefix}_respeaker_{algo,mic,ref}.wav
    let prefix = opts.prefix.clone().unwrap_or_else(default_prefix);
    let out_dir = &opts.out_dir;
    std::fs::create_dir_all(out_dir).map_err(|e| format!("创建输出目录 {out_dir} 失败: {e}"))?;
    let algo_path = format!("{out_dir}/{prefix}_respeaker_algo.wav");
    let mic_path = format!("{out_dir}/{prefix}_respeaker_mic.wav");
    let ref_path = format!("{out_dir}/{prefix}_respeaker_ref.wav");
    println!("输出目录: {out_dir}（前缀 {prefix}）");

    // 创建 DOA 运行时（仅 6ch/16k 原始输入支持）
    let mut doa_runtime = if opts.doa.enabled {
        if actual_rate != crate::doa::SAMPLE_RATE || actual_ch != 6 {
            return Err(format!(
                "实时 DOA 仅支持 ReSpeaker 16kHz/6ch 原始输入；当前为 {actual_rate}Hz/{actual_ch}ch"
            ));
        }
        let csv_path = opts
            .doa
            .csv
            .then(|| format!("{out_dir}/{prefix}_respeaker_doa.csv"));
        let runtime = DoaRuntime::new(opts.doa.config.clone(), csv_path.as_deref())?;
        println!("实时 DOA 已启用（16kHz/4-mic，PHAT-β SRP + Kalman）");
        Some(runtime)
    } else {
        None
    };

    let deadline = (opts.duration > 0).then(|| Instant::now() + Duration::from_secs(opts.duration));
    let started = Instant::now();
    let frames = if actual_ch == 6 {
        println!(
            "开始录制 ->\n  {algo_path}  (ch0 算法输出)\n  {mic_path}  (ch1-4 麦克风原始)\n  {ref_path}  (ch5 回放)\nCtrl+C 或 {} 秒后停止",
            if opts.duration > 0 {
                opts.duration.to_string()
            } else {
                "不限时".to_string()
            }
        );
        write_split_loop(
            &SplitOutputs {
                algo: &algo_path,
                mic: &mic_path,
                reference: &ref_path,
                rate: actual_rate,
            },
            &rx,
            &stop,
            deadline,
            &mut doa_runtime,
        )?
    } else {
        let mix_path = format!("{out_dir}/{prefix}_respeaker_mix.wav");
        println!(
            "注意: 实际通道数 {actual_ch} != 6，不拆分，输出单文件\n  {mix_path}\nCtrl+C 或 {} 秒后停止",
            if opts.duration > 0 {
                opts.duration.to_string()
            } else {
                "不限时".to_string()
            }
        );
        write_wav_loop(&mix_path, actual_rate, actual_ch, &rx, &stop, deadline)?
    };
    drop(handle); // 停止采集（cpal 停流 / WASAPI 停线程）

    let size = |p: &str| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    if actual_ch == 6 {
        println!(
            "录制完成: 约 {:.1}s / {} 帧\n  algo: {:.2} MB\n  mic:  {:.2} MB\n  ref:  {:.2} MB",
            started.elapsed().as_secs_f32(),
            frames,
            size(&algo_path) as f64 / 1e6,
            size(&mic_path) as f64 / 1e6,
            size(&ref_path) as f64 / 1e6,
        );
    } else {
        let mix_path = format!("{out_dir}/{prefix}_respeaker_mix.wav");
        println!(
            "录制完成: 约 {:.1}s / {} 帧 / {:.2} MB -> {mix_path}",
            started.elapsed().as_secs_f32(),
            frames,
            size(&mix_path) as f64 / 1e6,
        );
    }
    Ok(())
}

/// 把 6 通道交织样本拆分为三路（容量复用版）：algo（ch0）、mic（ch1-4，4ch）、ref（ch5）。
fn split_6ch_into(
    samples: &[i16],
    algo: &mut Vec<i16>,
    mic: &mut Vec<i16>,
    reference: &mut Vec<i16>,
) -> Result<(), String> {
    if !samples.len().is_multiple_of(6) {
        return Err(format!("采样块长度 {} 不是 6 的整数倍", samples.len()));
    }
    algo.clear();
    mic.clear();
    reference.clear();
    let n = samples.len() / 6;
    algo.reserve(n);
    mic.reserve(n * 4);
    reference.reserve(n);
    for frame in samples.chunks_exact(6) {
        algo.push(frame[0]);
        mic.extend_from_slice(&frame[1..5]);
        reference.push(frame[5]);
    }
    Ok(())
}

/// 3 个拆分输出文件的路径与采样率。
struct SplitOutputs<'a> {
    algo: &'a str,
    mic: &'a str,
    reference: &'a str,
    rate: u32,
}

/// 写 3 个拆分文件的主循环（6 通道）：从通道取样本、拆分、分别写入。
/// 输出为标准 WAVEFORMATEX（无通道布局元信息，播放器以数字显示通道）。
fn write_split_loop(
    out: &SplitOutputs<'_>,
    rx: &Receiver<Vec<i16>>,
    stop: &AtomicBool,
    deadline: Option<Instant>,
    doa: &mut Option<DoaRuntime>,
) -> Result<u64, String> {
    let mut algo = WavSink::create(out.algo, 1, out.rate)?;
    let mut mic = WavSink::create(out.mic, 4, out.rate)?;
    let mut rf = WavSink::create(out.reference, 1, out.rate)?;
    let mut algo_buf = Vec::new();
    let mut mic_buf = Vec::new();
    let mut ref_buf = Vec::new();
    let mut frames: u64 = 0;

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
                // DOA 与录音在同一消费线程串行执行；DOA 不修改 mic 数据
                if let Some(doa) = doa.as_mut() {
                    doa.push_block(&mic_buf)?;
                }
                algo.write_samples(&algo_buf)
                    .map_err(|e| format!("写入 {} 失败: {e}", out.algo))?;
                mic.write_samples(&mic_buf)
                    .map_err(|e| format!("写入 {} 失败: {e}", out.mic))?;
                rf.write_samples(&ref_buf)
                    .map_err(|e| format!("写入 {} 失败: {e}", out.reference))?;
                frames += (samples.len() / 6) as u64;
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("采集流已断开");
                break;
            }
        }
    }
    // 先刷新 DOA CSV，再完成 WAV 回填
    if let Some(doa) = doa.as_mut() {
        doa.finalize()?;
    }
    algo.finalize()
        .map_err(|e| format!("完成 {} 失败: {e}", out.algo))?;
    mic.finalize()
        .map_err(|e| format!("完成 {} 失败: {e}", out.mic))?;
    rf.finalize()
        .map_err(|e| format!("完成 {} 失败: {e}", out.reference))?;
    Ok(frames)
}

/// 写单个多通道 WAV 的主循环（非 6 通道回退路径）。
fn write_wav_loop(
    output: &str,
    rate: u32,
    channels: u16,
    rx: &Receiver<Vec<i16>>,
    stop: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<u64, String> {
    let mut writer = WavSink::create(output, channels, rate)?;
    let mut frames: u64 = 0;

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
                writer
                    .write_samples(&samples)
                    .map_err(|e| format!("写入 {} 失败: {e}", output))?;
                frames += (samples.len() / channels as usize) as u64;
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("采集流已断开");
                break;
            }
        }
    }
    writer
        .finalize()
        .map_err(|e| format!("完成 WAV 文件失败: {e}"))?;
    Ok(frames)
}

/// 按采样格式分派，构建输入流并把样本统一转换为 i16 发送给接收端。
fn build_stream(
    device: &Device,
    cfg: &SupportedStreamConfig,
    tx: SyncSender<Vec<i16>>,
    stop: Arc<AtomicBool>,
) -> Result<Stream, String> {
    let stream_config: StreamConfig = cfg.clone().into();
    match cfg.sample_format() {
        SampleFormat::I16 => build::<i16>(device, &stream_config, tx, stop),
        SampleFormat::U16 => build::<u16>(device, &stream_config, tx, stop),
        SampleFormat::I32 => build::<i32>(device, &stream_config, tx, stop),
        SampleFormat::F32 => build::<f32>(device, &stream_config, tx, stop),
        SampleFormat::F64 => build::<f64>(device, &stream_config, tx, stop),
        other => Err(format!("暂不支持的采样格式: {other:?}")),
    }
}

fn build<T>(
    device: &Device,
    config: &StreamConfig,
    tx: SyncSender<Vec<i16>>,
    stop: Arc<AtomicBool>,
) -> Result<Stream, String>
where
    T: SizedSample,
    i16: FromSample<T>,
{
    let err_fn = move |e| {
        // 设备拔出等错误：置位停止标志，让主循环退出。
        eprintln!("音频流错误: {e}");
        stop.store(true, Ordering::SeqCst);
    };
    device
        .build_input_stream(
            config,
            move |data: &[T], _info| {
                // 统一转成 i16（UAC 固件为 16-bit PCM）。
                let samples: Vec<i16> = data.iter().map(|&s| s.to_sample::<i16>()).collect();
                // 接收端已退出时（主线程结束）静默丢弃。
                let _ = tx.send(samples);
            },
            err_fn,
            None,
        )
        .map_err(|e| format!("构建输入流失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_6ch_into_works() {
        // 2 帧 × 6 通道：[0..5] 与 [6..11]
        let samples: Vec<i16> = (0..12).collect();
        let mut algo = Vec::new();
        let mut mic = Vec::new();
        let mut rf = Vec::new();
        split_6ch_into(&samples, &mut algo, &mut mic, &mut rf).unwrap();
        assert_eq!(algo, vec![0, 6]);
        assert_eq!(mic, vec![1, 2, 3, 4, 7, 8, 9, 10]);
        assert_eq!(rf, vec![5, 11]);
    }

    #[test]
    fn split_6ch_into_rejects_non_multiple_of_6() {
        let samples: Vec<i16> = (0..14).collect(); // 14 % 6 != 0
        let mut algo = Vec::new();
        let mut mic = Vec::new();
        let mut rf = Vec::new();
        assert!(split_6ch_into(&samples, &mut algo, &mut mic, &mut rf).is_err());
    }

    #[test]
    fn default_prefix_format() {
        let p = default_prefix();
        // YYYYMMDD_HHMMSS，长度 15
        assert_eq!(p.len(), 15);
        assert_eq!(&p[8..9], "_");
        assert!(
            p.chars()
                .enumerate()
                .all(|(i, c)| { c.is_ascii_digit() || (i == 8 && c == '_') })
        );
    }
}
