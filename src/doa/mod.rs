//! 实时 4-Mic 单声源二维 DOA。
//!
//! 算法链：任意块长分帧 → 4 路 RFFT → PSD/CPSD EMA → 相干度加权 PHAT-β SRP
//! → 多指标置信度 → 置信度门控 → 圆周恒角速度 Kalman。
//!
//! 输入为拆分后的 `ch1..ch4`（mic1..4）交织 i16 PCM，16 kHz；输出为
//! 0～360° 水平面方位角。32 ms 分析窗、16 ms 帧移，每 16 ms 一个内部观测。

pub mod framer;
pub mod geometry;
pub mod output;
pub mod srp;
pub mod tracker;

use crate::doa::framer::FrameAssembler;
use crate::doa::srp::{Observation, SrpPhatBeta};
use crate::doa::tracker::{GateTracker, TrackStatus};

pub const SAMPLE_RATE: u32 = 16_000;
pub const MIC_COUNT: usize = 4;
pub const FRAME_SIZE: usize = 512; // 32 ms
pub const HOP_SIZE: usize = 256; // 16 ms
pub const FFT_BINS: usize = FRAME_SIZE / 2 + 1; // 257
pub const ANGLE_COUNT: usize = 360; // 0..359°

// ---------------------------------------------------------------------------
// 角度辅助函数（所有模块复用同一实现，禁止在别处各自转换）
// ---------------------------------------------------------------------------

/// wrap 到 [0, 360)。
pub fn wrap_360(deg: f32) -> f32 {
    deg.rem_euclid(360.0)
}

/// 循环差 a - b，范围 [-180, 180)。
pub fn circular_delta_deg(a: f32, b: f32) -> f32 {
    (a - b + 180.0).rem_euclid(360.0) - 180.0
}

/// 循环距离 |a - b|，范围 [0, 180]。
pub fn circular_distance_deg(a: f32, b: f32) -> f32 {
    circular_delta_deg(a, b).abs()
}

/// 内部数学角度 → 对外角度：先旋转 offset，再按顺时针镜像。
pub fn output_deg(internal_deg: f32, angle_offset_deg: f32, clockwise: bool) -> f32 {
    wrap_360(
        angle_offset_deg
            + if clockwise {
                -internal_deg
            } else {
                internal_deg
            },
    )
}

// ---------------------------------------------------------------------------
// 配置与结果
// ---------------------------------------------------------------------------

/// DOA 配置（默认值见 `Default`）。
#[derive(Clone, Debug)]
pub struct DoaConfig {
    pub sample_rate: u32,
    pub beta: f32,
    pub cpsd_tau_ms: f32,
    pub angle_offset_deg: f32,
    pub clockwise: bool,
    pub acquire_confidence: f32,
    pub update_confidence: f32,
    pub max_coast_ms: u32,
}

impl Default for DoaConfig {
    fn default() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            beta: 0.75,
            cpsd_tau_ms: 100.0,
            angle_offset_deg: 0.0,
            clockwise: false,
            acquire_confidence: 0.65,
            update_confidence: 0.40,
            max_coast_ms: 500,
        }
    }
}

impl DoaConfig {
    /// 校验所有参数；返回错误信息。
    pub fn validate(&self) -> Result<(), String> {
        if self.sample_rate != SAMPLE_RATE {
            return Err(format!(
                "DOA 仅支持 {SAMPLE_RATE} Hz，当前 {}",
                self.sample_rate
            ));
        }
        if !(0.0..=1.0).contains(&self.beta) {
            return Err(format!("doa_beta 必须在 0..=1 之间，当前 {}", self.beta));
        }
        if !self.cpsd_tau_ms.is_finite() || self.cpsd_tau_ms <= 0.0 {
            return Err(format!(
                "doa_cpsd_tau_ms 必须为正数，当前 {}",
                self.cpsd_tau_ms
            ));
        }
        for (name, v) in [
            ("acquire_confidence", self.acquire_confidence),
            ("update_confidence", self.update_confidence),
        ] {
            if !(0.0..=1.0).contains(&v) {
                return Err(format!("{name} 必须在 0..=1 之间，当前 {v}"));
            }
        }
        if self.update_confidence > self.acquire_confidence {
            return Err(format!(
                "update_confidence ({}) 不得大于 acquire_confidence ({})",
                self.update_confidence, self.acquire_confidence
            ));
        }
        if self.max_coast_ms == 0 {
            return Err("max_coast_ms 必须大于 0".into());
        }
        for v in [
            self.beta,
            self.cpsd_tau_ms,
            self.angle_offset_deg,
            self.acquire_confidence,
            self.update_confidence,
        ] {
            if !v.is_finite() {
                return Err("DOA 浮点参数不允许 NaN/Inf".into());
            }
        }
        Ok(())
    }
}

/// CLI 层的 DOA 运行选项。
#[derive(Clone, Debug)]
pub struct DoaRunOptions {
    pub enabled: bool,
    pub csv: bool,
    pub config: DoaConfig,
}

/// 单帧 DOA 结果。
#[derive(Clone, Debug)]
pub struct DoaResult {
    /// 当前分析窗最后一个采样点对应的捕获时间（毫秒）。
    pub timestamp_ms: f64,
    /// SRP 原始角度（已做 offset/clockwise 转换）；无有效观测时为 None。
    pub raw_angle_deg: Option<f32>,
    /// Kalman 输出（已做 offset/clockwise 转换）；尚未获取目标时为 None。
    pub tracked_angle_deg: Option<f32>,
    pub confidence: f32,
    pub status: TrackStatus,
    pub observation_used: bool,
    pub peak_score: f32,
    pub second_peak_score: f32,
    pub peak_gap_ratio: f32,
    pub prominence: f32,
    pub mean_msc: f32,
    pub rms_dbfs: f32,
}

/// DOA 处理器：分帧 + SRP + 门控 + Kalman 的编排。
pub struct DoaProcessor {
    config: DoaConfig,
    framer: FrameAssembler,
    srp: SrpPhatBeta,
    tracker: GateTracker,
    frame_count: u64,
    frame_scratch: [[f32; FRAME_SIZE]; MIC_COUNT],
}

impl DoaProcessor {
    pub fn new(config: DoaConfig) -> Result<Self, String> {
        config.validate()?;
        let srp = SrpPhatBeta::new(config.beta, config.cpsd_tau_ms)?;
        let tracker = GateTracker::new(
            config.acquire_confidence,
            config.update_confidence,
            config.max_coast_ms,
        );
        Ok(Self {
            framer: FrameAssembler::new(),
            srp,
            tracker,
            config,
            frame_count: 0,
            frame_scratch: [[0.0; FRAME_SIZE]; MIC_COUNT],
        })
    }

    /// `mics_interleaved` 为 mic0,mic1,mic2,mic3 循环交织的 i16 PCM，
    /// 长度必须是 4 的整数倍。每产生一个分析帧，向 `results` 追加一个结果。
    /// 调用方负责在调用前 clear。
    pub fn push_interleaved(
        &mut self,
        mics_interleaved: &[i16],
        results: &mut Vec<DoaResult>,
    ) -> Result<(), String> {
        if !mics_interleaved.len().is_multiple_of(MIC_COUNT) {
            return Err(format!(
                "DOA 输入长度 {} 不是 4 的整数倍（mic 通道数）",
                mics_interleaved.len()
            ));
        }
        for frame in mics_interleaved.chunks_exact(MIC_COUNT) {
            let sample = [
                frame[0] as f32 / 32768.0,
                frame[1] as f32 / 32768.0,
                frame[2] as f32 / 32768.0,
                frame[3] as f32 / 32768.0,
            ];
            if self.framer.push(sample) {
                self.process_frame(results);
            }
        }
        Ok(())
    }

    fn process_frame(&mut self, results: &mut Vec<DoaResult>) {
        let timestamp_ms = self.framer.total_samples() as f64 * 1000.0 / SAMPLE_RATE as f64;
        self.framer.copy_frame(&mut self.frame_scratch);
        self.frame_count += 1;
        // 前 2 个分析帧只更新 PSD/CPSD EMA，不生成结果
        let emit = self.frame_count >= 3;
        let obs = self.srp.process(&self.frame_scratch, emit);
        if !emit {
            return;
        }
        let rms_dbfs = self.srp.rms_dbfs();
        let (observation_used, tracked_internal_deg) = self.tracker.update(obs);

        let conf = obs.map(|o| o.confidence).unwrap_or(0.0);
        let raw_angle_deg = obs.map(|o| {
            output_deg(
                o.raw_internal_deg,
                self.config.angle_offset_deg,
                self.config.clockwise,
            )
        });
        let tracked_angle_deg = tracked_internal_deg
            .map(|deg| output_deg(deg, self.config.angle_offset_deg, self.config.clockwise));
        let metrics = obs.unwrap_or(Observation {
            raw_internal_deg: 0.0,
            confidence: 0.0,
            peak_score: 0.0,
            second_peak_score: 0.0,
            peak_gap_ratio: 0.0,
            prominence: 0.0,
            mean_msc: 0.0,
            rms_dbfs,
        });

        results.push(DoaResult {
            timestamp_ms,
            raw_angle_deg,
            tracked_angle_deg,
            confidence: conf,
            status: self.tracker.status(),
            observation_used,
            peak_score: metrics.peak_score,
            second_peak_score: metrics.second_peak_score,
            peak_gap_ratio: metrics.peak_gap_ratio,
            prominence: metrics.prominence,
            mean_msc: metrics.mean_msc,
            rms_dbfs: metrics.rms_dbfs,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doa::geometry::RESPEAKER_V2_MICS_M;
    use std::f32::consts::PI;

    /// 合成确定性的多音平面波（与 srp 测试同一生成器逻辑）。
    fn synth_plane_wave(theta_deg: f32, seconds: f32) -> Vec<i16> {
        let theta = theta_deg.to_radians();
        let u = [theta.cos(), theta.sin()];
        let n = (seconds * SAMPLE_RATE as f32) as usize;
        let mut out = Vec::with_capacity(n * MIC_COUNT);
        for n in 0..n {
            let t = n as f32 / SAMPLE_RATE as f32;
            for r in RESPEAKER_V2_MICS_M.iter() {
                let tau_m = (r[0] * u[0] + r[1] * u[1]) / 343.0;
                let mut s = 0.0f32;
                // 使用 FFT bin 上的宽带确定性多音；不同固定相位和幅度避免退化，
                // 所有分量的理论峰值和小于 1，避免 i16 削波制造额外谐波。
                for (fi, bin) in (16usize..=72).step_by(2).enumerate() {
                    let f = bin as f32 * SAMPLE_RATE as f32 / FRAME_SIZE as f32;
                    let phase = (fi * fi + 3 * fi) as f32 * 0.173;
                    let amp = 0.018 + 0.008 * (fi * 7 % 13) as f32 / 12.0;
                    s += amp * (2.0 * PI * f * t + 2.0 * PI * f * tau_m + phase).sin();
                }
                out.push((s * 32767.0) as i16);
            }
        }
        out
    }

    /// 合成平面波在当前阵列孔径和限频下最高置信度约为 0.4；这里降低门控阈值，
    /// 只验证方向估计与跟踪链路。默认 0.65/0.40 的门控边界由 tracker 单测验证。
    fn test_config() -> DoaConfig {
        DoaConfig {
            acquire_confidence: 0.30,
            update_confidence: 0.20,
            ..DoaConfig::default()
        }
    }

    fn circular_mean_deg(angles: &[f32]) -> f32 {
        let (mut sx, mut sy) = (0.0f32, 0.0f32);
        for &a in angles {
            sx += (a.to_radians()).sin();
            sy += (a.to_radians()).cos();
        }
        sx.atan2(sy).to_degrees().rem_euclid(360.0)
    }

    /// 合成方向全链路测试：raw <= 2°，tracked <= 3°，最终 Tracking。
    #[test]
    fn processor_synthetic_directions() {
        for &theta in &[0.0f32, 45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0] {
            let mut p = DoaProcessor::new(test_config()).unwrap();
            let signal = synth_plane_wave(theta, 1.2);
            let mut results = Vec::new();
            // 不规则块输入（块长均为 4 的倍数）
            let mut idx = 0usize;
            let mut total = 0usize;
            for chunk in [508usize, 32, 1024, 8, 256, 100, 32, 128, 400]
                .into_iter()
                .cycle()
            {
                if idx >= signal.len() {
                    break;
                }
                let end = (idx + chunk).min(signal.len());
                results.clear();
                p.push_interleaved(&signal[idx..end], &mut results).unwrap();
                total += results.len();
                idx = end;
            }
            assert!(total >= 50, "theta={theta} 结果太少: {total}");

            // 重新整体输入获取全部结果
            let mut p2 = DoaProcessor::new(test_config()).unwrap();
            let mut results2 = Vec::new();
            p2.push_interleaved(&signal, &mut results2).unwrap();

            // 前 2 帧 warmup 不产生结果；第 3 分析帧（1024 采样 = 64ms）起产生
            assert!(
                results2.len() >= 70,
                "theta={theta} 结果太少: {}",
                results2.len()
            );
            let first = &results2[0];
            assert!(
                (first.timestamp_ms - 64.0).abs() < 1e-6,
                "首帧时间戳 {}",
                first.timestamp_ms
            );
            // 时间戳间隔恒为 16ms
            for w in results2.windows(2) {
                assert!((w[1].timestamp_ms - w[0].timestamp_ms - 16.0).abs() < 1e-6);
            }

            // raw 稳定段误差
            let tail: Vec<f32> = results2[results2.len() - 5..]
                .iter()
                .filter_map(|r| r.raw_angle_deg)
                .collect();
            let mean = circular_mean_deg(&tail);
            let err = circular_distance_deg(mean, theta);
            assert!(err <= 2.0, "theta={theta} raw 误差 {err}° (mean={mean})");

            // tracked 误差与最终状态
            let last = results2.last().unwrap();
            let max_confidence = results2.iter().map(|r| r.confidence).fold(0.0f32, f32::max);
            assert_eq!(
                last.status,
                TrackStatus::Tracking,
                "theta={theta} 未进入 Tracking，last_conf={} max_conf={max_confidence} gap={} prom={} msc={} rms={}",
                last.confidence,
                last.peak_gap_ratio,
                last.prominence,
                last.mean_msc,
                last.rms_dbfs
            );
            let tracked = last.tracked_angle_deg.unwrap();
            let terr = circular_distance_deg(tracked, theta);
            assert!(
                terr <= 3.0,
                "theta={theta} tracked 误差 {terr}° ({tracked})"
            );
        }
    }

    /// 任意块长一致性：同一数据整体输入 vs 不规则块输入，结果一致。
    #[test]
    fn processor_block_size_consistency() {
        let signal = synth_plane_wave(137.0, 0.6);
        // 整体输入
        let mut p_all = DoaProcessor::new(test_config()).unwrap();
        let mut r_all = Vec::new();
        p_all.push_interleaved(&signal, &mut r_all).unwrap();
        // 不规则块
        let mut p_block = DoaProcessor::new(test_config()).unwrap();
        let mut r_block = Vec::new();
        let mut idx = 0usize;
        for chunk in [8usize, 300, 44, 1024, 12, 200, 512, 40, 96]
            .into_iter()
            .cycle()
        {
            if idx >= signal.len() {
                break;
            }
            let end = (idx + chunk).min(signal.len());
            p_block
                .push_interleaved(&signal[idx..end], &mut r_block)
                .unwrap();
            idx = end;
        }
        assert_eq!(r_all.len(), r_block.len());
        for (a, b) in r_all.iter().zip(r_block.iter()) {
            assert!((a.timestamp_ms - b.timestamp_ms).abs() < 1e-9);
            let (ra, rb) = (a.raw_angle_deg, b.raw_angle_deg);
            match (ra, rb) {
                (Some(x), Some(y)) => assert!((x - y).abs() < 1e-3, "raw 不一致 {x} vs {y}"),
                (None, None) => {}
                _ => panic!("raw 可选性不一致"),
            }
            assert!((a.confidence - b.confidence).abs() < 1e-5);
        }
    }

    /// 无效输入：长度非 4 整数倍返回错误。
    #[test]
    fn rejects_non_multiple_of_4() {
        let mut p = DoaProcessor::new(DoaConfig::default()).unwrap();
        let mut results = Vec::new();
        assert!(p.push_interleaved(&[0i16, 1, 2], &mut results).is_err());
    }

    /// 非法配置：DoaConfig::validate 拒绝。
    #[test]
    fn config_validation() {
        // beta 越界
        assert!(
            DoaConfig {
                beta: 1.5,
                ..DoaConfig::default()
            }
            .validate()
            .is_err()
        );
        // tau 非正
        assert!(
            DoaConfig {
                cpsd_tau_ms: 0.0,
                ..DoaConfig::default()
            }
            .validate()
            .is_err()
        );
        // update > acquire
        assert!(
            DoaConfig {
                acquire_confidence: 0.4,
                update_confidence: 0.6,
                ..DoaConfig::default()
            }
            .validate()
            .is_err()
        );
        // NaN
        assert!(
            DoaConfig {
                beta: f32::NAN,
                ..DoaConfig::default()
            }
            .validate()
            .is_err()
        );
        // 合法配置
        assert!(DoaConfig::default().validate().is_ok());
        // 错误配置无法构建处理器
        assert!(
            DoaProcessor::new(DoaConfig {
                beta: -1.0,
                ..DoaConfig::default()
            })
            .is_err()
        );
    }

    /// 低电平独立噪声：不 panic、不获取目标。
    #[test]
    fn low_level_noise_never_acquires() {
        // 确定性 LCG 生成独立噪声（不引入 rand 依赖）
        let mut state: u32 = 0x1234_5678;
        let mut next = move || {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            (state >> 16) as i16 / 1024
        };
        let mut signal = Vec::with_capacity(16000 * 4);
        for _ in 0..16000 {
            for _ in 0..4 {
                signal.push(next());
            }
        }
        let mut p = DoaProcessor::new(DoaConfig::default()).unwrap();
        let mut results = Vec::new();
        p.push_interleaved(&signal, &mut results).unwrap();
        assert!(!results.is_empty());
        for r in &results {
            assert_eq!(r.status, TrackStatus::Searching, "噪声不应获取目标");
            assert!(r.tracked_angle_deg.is_none());
        }
    }

    /// 全零输入：不 panic、不产生有效观测。
    #[test]
    fn zero_input_safe() {
        let mut p = DoaProcessor::new(DoaConfig::default()).unwrap();
        let mut results = Vec::new();
        let zeros = vec![0i16; 16000 * 4];
        p.push_interleaved(&zeros, &mut results).unwrap();
        assert!(!results.is_empty());
        for r in &results {
            assert_eq!(r.confidence, 0.0);
            assert!(r.raw_angle_deg.is_none());
            assert_eq!(r.status, TrackStatus::Searching);
            assert!((r.rms_dbfs + 240.0).abs() < 1e-3);
        }
    }
}
