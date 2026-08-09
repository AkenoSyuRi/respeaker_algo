//! ReSpeaker Mic Array v2.0（XVF3000）Windows 专用录音工具。
//!
//! 程序固定使用 WASAPI 独占模式采集 16 kHz / 16-bit / 6 通道 PCM，并输出
//! ch0 算法音频、ch1..ch4 原始麦克风和 ch5 回放参考三路 WAV。
//! 传入 `--pipeline-config` 时，音频块还会依次经过配置的内置 Rust 算法模块。

#[cfg(not(windows))]
compile_error!("respeaker_algo 仅支持 Windows ReSpeaker WASAPI 独占录音");

mod doa;
mod pipeline;
mod recorder;
mod wasapi;
mod wav;
mod web;

use std::path::PathBuf;

use clap::Parser;

use recorder::RecordOptions;

#[derive(Parser)]
#[command(
    name = "respeaker-algo",
    version,
    about = "ReSpeaker Mic Array v2.0 Windows 专用录音与内置算法 Pipeline 工具"
)]
struct Cli {
    /// 录制时长（秒）；0 表示持续录制直到 Ctrl+C
    #[arg(short, long, default_value_t = 0)]
    duration: u64,

    /// 输出目录
    #[arg(long, default_value = "target/out")]
    out_dir: String,

    /// 输出文件前缀；默认使用当前时间戳，如 20260809_143012
    #[arg(long)]
    prefix: Option<String>,

    /// 内置算法 Pipeline TOML 配置文件
    #[arg(long)]
    pipeline_config: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = recorder::run_record(&RecordOptions {
        duration: cli.duration,
        out_dir: cli.out_dir,
        prefix: cli.prefix,
        pipeline_config: cli.pipeline_config,
    }) {
        eprintln!("错误: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_accepts_recording_and_pipeline_options() {
        let cli = Cli::try_parse_from([
            "respeaker-algo",
            "--duration",
            "10",
            "--out-dir",
            "out",
            "--prefix",
            "test",
            "--pipeline-config",
            "configs/doa.toml",
        ])
        .unwrap();
        assert_eq!(cli.duration, 10);
        assert_eq!(cli.out_dir, "out");
        assert_eq!(cli.prefix.as_deref(), Some("test"));
        assert_eq!(
            cli.pipeline_config.as_deref(),
            Some(std::path::Path::new("configs/doa.toml"))
        );
    }

    #[test]
    fn cli_rejects_removed_commands_and_options() {
        for args in [
            vec!["respeaker-algo", "list-devices"],
            vec!["respeaker-algo", "help"],
            vec!["respeaker-algo", "--device", "0"],
            vec!["respeaker-algo", "--rate", "16000"],
            vec!["respeaker-algo", "--channels", "6"],
            vec!["respeaker-algo", "--backend", "wasapi"],
            vec!["respeaker-algo", "--doa"],
            vec!["respeaker-algo", "--doa-csv"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }
}
