//! ReSpeaker Mic Array v2.0（XVF3000）录音 + 实时 DOA 工具。
//!
//! 默认（无子命令）直接启动录制；`--duration` 指定时长（0 = 直到 Ctrl+C），
//! 输出到 `{out_dir}/{prefix}_respeaker_{algo,mic,ref}.wav`（6 通道固件拆分
//! 为 3 个文件：ch0 算法输出 / ch1-4 麦克风原始 / ch5 回放）。
//!
//! 子命令 `list-devices` 列出音频输入设备。
//!
//! 实时 DOA（`--doa`）：基于 ch1..ch4 的 4 路原始麦克风做单声源二维方位角
//! 估计（512 点 Hann 窗 / 256 点帧移 / 360° 1° 网格 / PHAT-β SRP + 圆周
//! Kalman），每 16 ms 一个内部观测，终端限速 10 Hz；`--doa-csv` 额外保存
//! 逐帧 CSV。DOA 默认关闭，不影响现有录音输出。
//!
//! 采集后端（`--backend`）：
//! - `auto`（默认）：Windows 上走 WASAPI 独占模式（可拿到 6ch/16k），
//!   其它平台走 cpal；
//! - `cpal`：跨平台共享模式（Windows 上仅 48kHz/2ch）；
//! - `wasapi`：WASAPI 独占模式，仅 Windows。

mod audio;
mod doa;
mod recorder;
#[cfg(windows)]
mod wasapi;
mod wav;

use clap::{Parser, Subcommand, ValueEnum};

use doa::{DoaConfig, DoaRunOptions};
use recorder::{BackendChoice, RecordOptions};

#[derive(Parser)]
#[command(
    name = "respeaker-algo",
    version,
    about = "ReSpeaker Mic Array v2.0 录音工具（6_channels_firmware.bin）"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// 录制时长（秒）；0 表示持续录制直到 Ctrl+C
    #[arg(short, long, default_value_t = 0)]
    duration: u64,

    /// 输入设备：list-devices 中的索引，或名称包含的关键字；
    /// 缺省自动查找 ReSpeaker 设备
    #[arg(long)]
    device: Option<String>,

    /// 采样率（默认 16000，匹配 6_channels_firmware.bin）
    #[arg(long, default_value_t = 16000)]
    rate: u32,

    /// 通道数（默认 6）
    #[arg(long, default_value_t = 6)]
    channels: u16,

    /// 采集后端（见顶部说明）
    #[arg(long, value_enum, default_value_t = BackendArg::Auto)]
    backend: BackendArg,

    /// 输出目录（默认 target/out）
    #[arg(long, default_value = "target/out")]
    out_dir: String,

    /// 输出文件前缀（默认当前时间戳，如 20260715_143012）
    #[arg(long)]
    prefix: Option<String>,

    /// 启用实时 4-Mic DOA（仅支持 16kHz/6ch ReSpeaker 输入）
    #[arg(long)]
    doa: bool,

    /// 保存逐帧 DOA CSV；该选项同时隐式启用 DOA
    #[arg(long)]
    doa_csv: bool,

    /// PHAT 部分白化指数，范围 0..=1
    #[arg(long, default_value_t = 0.75)]
    doa_beta: f32,

    /// CPSD/PSD EMA 时间常数，单位 ms
    #[arg(long, default_value_t = 100.0)]
    doa_cpsd_tau_ms: f32,

    /// 输出角度旋转补偿，单位度
    #[arg(long, default_value_t = 0.0)]
    doa_offset_deg: f32,

    /// 输出角度改为顺时针增加
    #[arg(long)]
    doa_clockwise: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum BackendArg {
    /// 自动：Windows 默认 WASAPI 独占，其它平台 cpal
    Auto,
    /// cpal 跨平台共享模式
    Cpal,
    /// WASAPI 独占模式（仅 Windows）
    Wasapi,
}

#[derive(Subcommand)]
enum Command {
    /// 列出所有音频输入设备
    ListDevices {
        /// Windows 上打印设备 ID；其它平台打印支持的输入配置
        #[arg(long)]
        verbose: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::ListDevices { verbose }) => list_devices(verbose),
        None => recorder::run_record(&RecordOptions {
            device: cli.device,
            duration: cli.duration,
            rate: cli.rate,
            channels: cli.channels,
            backend: match cli.backend {
                BackendArg::Auto => BackendChoice::Auto,
                BackendArg::Cpal => BackendChoice::Cpal,
                BackendArg::Wasapi => BackendChoice::Wasapi,
            },
            out_dir: cli.out_dir,
            prefix: cli.prefix,
            doa: DoaRunOptions {
                enabled: cli.doa || cli.doa_csv,
                csv: cli.doa_csv,
                config: DoaConfig {
                    beta: cli.doa_beta,
                    cpsd_tau_ms: cli.doa_cpsd_tau_ms,
                    angle_offset_deg: cli.doa_offset_deg,
                    clockwise: cli.doa_clockwise,
                    ..DoaConfig::default()
                },
            },
        }),
    };

    if let Err(e) = result {
        eprintln!("错误: {e}");
        std::process::exit(1);
    }
}

/// 列出输入设备。Windows 用 WASAPI 枚举（与 record 默认后端的索引一致），
/// 其它平台用 cpal 枚举。
fn list_devices(verbose: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        let devices = wasapi::list_devices()?;
        if devices.is_empty() {
            println!("未找到任何音频输入设备");
            return Ok(());
        }
        println!("可用的音频输入设备（WASAPI 视图）:");
        for d in &devices {
            println!("  [{:2}] {}", d.index, d.name);
            if verbose {
                println!("      id={}", d.id);
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let devices = audio::list_input_devices()?;
        if devices.is_empty() {
            println!("未找到任何音频输入设备");
            return Ok(());
        }
        let host = cpal::default_host();
        let by_name = |name: &str| {
            host.input_devices()
                .ok()
                .and_then(|mut ds| ds.find(|d| d.name().map(|n| n == name).unwrap_or(false)))
        };
        println!("可用的音频输入设备:");
        for d in &devices {
            println!("  [{:2}] {}", d.index, d.name);
            if verbose {
                if let Some(dev) = by_name(&d.name) {
                    audio::print_device_configs(&dev);
                }
            }
        }
        Ok(())
    }
}
