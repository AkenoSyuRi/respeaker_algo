//! DOA 运行时输出：终端限速显示 + 可选逐帧 CSV。

use std::fs::File;
use std::io::{BufWriter, Write};

use crate::doa::tracker::TrackStatus;
use crate::doa::{DoaConfig, DoaProcessor, DoaResult};

/// DOA 运行时：拥有处理器、可复用结果缓冲、可选 CSV 与终端限速状态。
pub struct DoaRuntime {
    processor: DoaProcessor,
    results: Vec<DoaResult>,
    csv: Option<BufWriter<File>>,
    last_console_timestamp_ms: f64,
}

impl DoaRuntime {
    /// `csv_path` 为 Some 时创建 CSV 并写表头。
    pub fn new(config: DoaConfig, csv_path: Option<&str>) -> Result<Self, String> {
        let processor = DoaProcessor::new(config)?;
        let csv = match csv_path {
            Some(path) => {
                let file =
                    File::create(path).map_err(|e| format!("创建 DOA CSV {} 失败: {e}", path))?;
                let mut w = BufWriter::new(file);
                writeln!(
                    w,
                    "time_ms,raw_deg,tracked_deg,confidence,status,observation_used,peak_score,second_peak_score,peak_gap_ratio,prominence,mean_msc,rms_dbfs"
                )
                .map_err(|e| format!("写入 DOA CSV 表头失败: {e}"))?;
                Some(w)
            }
            None => None,
        };
        Ok(Self {
            processor,
            results: Vec::new(),
            csv,
            last_console_timestamp_ms: f64::NEG_INFINITY,
        })
    }

    /// 输入一块 4 路交织 i16 数据；处理并输出（终端限速 100ms，CSV 每帧）。
    pub fn push_block(&mut self, mics_interleaved: &[i16]) -> Result<&[DoaResult], String> {
        self.results.clear();
        self.processor
            .push_interleaved(mics_interleaved, &mut self.results)?;
        for r in &self.results {
            // 终端：最多每 100 ms 输出一次
            if r.timestamp_ms - self.last_console_timestamp_ms >= 100.0 {
                print_console_line(r);
                self.last_console_timestamp_ms = r.timestamp_ms;
            }
            // CSV：每个内部观测写一行
            if let Some(csv) = &mut self.csv {
                write_csv_line(csv, r).map_err(|e| format!("写入 DOA CSV 失败: {e}"))?;
            }
        }
        Ok(&self.results)
    }

    /// 刷新并关闭 CSV。
    pub fn finalize(&mut self) -> Result<(), String> {
        if let Some(csv) = &mut self.csv {
            csv.flush().map_err(|e| format!("刷新 DOA CSV 失败: {e}"))?;
        }
        Ok(())
    }
}

fn fmt_deg(v: Option<f32>) -> String {
    match v {
        Some(x) => format!("{x:6.1}°"),
        None => "    --".to_string(),
    }
}

fn status_str(s: TrackStatus) -> &'static str {
    match s {
        TrackStatus::Searching => "searching",
        TrackStatus::Tracking => "tracking",
        TrackStatus::Coasting => "coasting",
    }
}

fn print_console_line(r: &DoaResult) {
    println!(
        "DOA t={:8.3}s raw={} track={} conf={:.2} status={} used={}",
        r.timestamp_ms / 1000.0,
        fmt_deg(r.raw_angle_deg),
        fmt_deg(r.tracked_angle_deg),
        r.confidence,
        status_str(r.status),
        if r.observation_used { "yes" } else { "no" }
    );
}

fn write_csv_line(w: &mut BufWriter<File>, r: &DoaResult) -> std::io::Result<()> {
    let raw = r
        .raw_angle_deg
        .map(|x| format!("{x:.3}"))
        .unwrap_or_default();
    let tracked = r
        .tracked_angle_deg
        .map(|x| format!("{x:.3}"))
        .unwrap_or_default();
    writeln!(
        w,
        "{:.3},{},{},{:.6},{},{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6}",
        r.timestamp_ms,
        raw,
        tracked,
        r.confidence,
        status_str(r.status),
        if r.observation_used { 1 } else { 0 },
        r.peak_score,
        r.second_peak_score,
        r.peak_gap_ratio,
        r.prominence,
        r.mean_msc,
        r.rms_dbfs,
    )
}
