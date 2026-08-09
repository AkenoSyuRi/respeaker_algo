//! PHAT-β SRP 声源方向估计。
//!
//! 链路：4 路 RFFT → PSD/CPSD EMA → 相干度归一 PHAT-β 投票 → 360° 扫描 →
//! 主/次峰 + robust prominence + 多指标置信度。
//!
//! 热路径复用所有 FFT 与大缓冲，不在每帧分配新 Vec。

use realfft::num_complex::Complex32;
use realfft::{RealFftPlanner, RealToComplex};

use crate::doa::geometry::{MIC_PAIRS, SteeringLut, band_weight, smoothstep};
use crate::doa::{
    ANGLE_COUNT, FFT_BINS, FRAME_SIZE, HOP_SIZE, MIC_COUNT, SAMPLE_RATE, circular_distance_deg,
    wrap_360,
};

const EPS_POWER: f32 = 1e-12;
const EPS_SCORE: f32 = 1e-6;

/// 一帧 SRP 观测（内部数学角度，未做 offset/clockwise 转换）。
#[derive(Clone, Copy, Debug)]
pub struct Observation {
    /// 内部角度：0° = +X，90° = +Y，逆时针。
    pub raw_internal_deg: f32,
    pub confidence: f32,
    pub peak_score: f32,
    pub second_peak_score: f32,
    pub peak_gap_ratio: f32,
    pub prominence: f32,
    pub mean_msc: f32,
    pub rms_dbfs: f32,
}

pub struct SrpPhatBeta {
    // FFT 资源（一次创建、全程复用）
    fft: std::sync::Arc<dyn RealToComplex<f32>>,
    scratch: Vec<Complex32>,
    hann: [f32; FRAME_SIZE],
    fft_input: [[f32; FRAME_SIZE]; MIC_COUNT],
    spectrum: [[Complex32; FFT_BINS]; MIC_COUNT],

    // 统计状态
    psd: [[f32; FFT_BINS]; MIC_COUNT],
    cpsd: [[Complex32; FFT_BINS]; MIC_PAIRS.len()],
    frame_count: u64,

    // 每帧可复用缓冲
    steering: SteeringLut,
    weighted_vote: Vec<Complex32>,
    score: [f32; ANGLE_COUNT],
    median_scratch: Vec<f32>,
    rms_dbfs: f32,

    // 参数
    beta: f32,
    alpha: f32,
}

impl SrpPhatBeta {
    pub fn new(beta: f32, cpsd_tau_ms: f32) -> Result<Self, String> {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FRAME_SIZE);
        let scratch = fft.make_scratch_vec();
        let hann = std::array::from_fn(|n| {
            0.5 - 0.5 * (std::f32::consts::TAU * n as f32 / (FRAME_SIZE - 1) as f32).cos()
        });
        let steering = SteeringLut::new();
        let term_count = steering.term_count();
        let tau_seconds = cpsd_tau_ms / 1000.0;
        if !tau_seconds.is_finite() || tau_seconds <= 0.0 {
            return Err(format!("无效的 CPSD EMA 时间常数: {cpsd_tau_ms}ms"));
        }
        let hop_seconds = HOP_SIZE as f32 / SAMPLE_RATE as f32;
        let alpha = (-hop_seconds / tau_seconds).exp();
        Ok(Self {
            fft,
            scratch,
            hann,
            fft_input: [[0.0; FRAME_SIZE]; MIC_COUNT],
            spectrum: [[Complex32::default(); FFT_BINS]; MIC_COUNT],
            psd: [[0.0; FFT_BINS]; MIC_COUNT],
            cpsd: [[Complex32::default(); FFT_BINS]; MIC_PAIRS.len()],
            frame_count: 0,
            steering,
            weighted_vote: vec![Complex32::default(); term_count],
            score: [0.0; ANGLE_COUNT],
            median_scratch: vec![0.0; ANGLE_COUNT],
            beta,
            alpha,
            rms_dbfs: 0.0,
        })
    }

    /// 处理一帧 4 路时域样本。
    ///
    /// - 总是更新 PSD/CPSD EMA；
    /// - `emit == true` 时（warmup 之后）计算 SRP 并返回观测，否则返回 None。
    pub fn process(
        &mut self,
        frame: &[[f32; FRAME_SIZE]; MIC_COUNT],
        emit: bool,
    ) -> Option<Observation> {
        self.frame_count += 1;
        let is_first = self.frame_count == 1;
        self.compute_ffts(frame);
        self.update_spectra(is_first);
        if !emit {
            return None;
        }
        self.estimate()
    }

    /// 最近一帧减均值、未加窗信号的总体 RMS（dBFS）。
    pub fn rms_dbfs(&self) -> f32 {
        self.rms_dbfs
    }

    fn compute_ffts(&mut self, frame: &[[f32; FRAME_SIZE]; MIC_COUNT]) {
        let mut mean_square_sum = 0.0f64;
        for (ch, input) in self.fft_input.iter_mut().enumerate() {
            input.copy_from_slice(&frame[ch]);
            let mean = input.iter().sum::<f32>() / FRAME_SIZE as f32;
            for x in input.iter_mut() {
                *x -= mean;
            }
            for &x in input.iter() {
                mean_square_sum += (x as f64) * (x as f64);
            }
            for (n, x) in input.iter_mut().enumerate() {
                *x *= self.hann[n];
            }
            // 四通道复用同一 scratch
            self.fft
                .process_with_scratch(input, &mut self.spectrum[ch], &mut self.scratch)
                .expect("固定长度 FFT 不应失败");
        }
        let rms = (mean_square_sum / (MIC_COUNT * FRAME_SIZE) as f64).sqrt() as f32;
        self.rms_dbfs = 20.0 * (rms.max(1e-12)).log10();
    }

    /// 更新 PSD/CPSD EMA。第一帧直接初始化，之后按 alpha 平滑。
    fn update_spectra(&mut self, is_first: bool) {
        for bin in 0..FFT_BINS {
            for ch in 0..MIC_COUNT {
                let x = self.spectrum[ch][bin];
                let p = x.norm_sqr();
                if is_first {
                    self.psd[ch][bin] = p;
                } else {
                    self.psd[ch][bin] = self.alpha * self.psd[ch][bin] + (1.0 - self.alpha) * p;
                }
            }
            for (pi, &(i, j)) in MIC_PAIRS.iter().enumerate() {
                let c = self.spectrum[i][bin] * self.spectrum[j][bin].conj();
                if is_first {
                    self.cpsd[pi][bin] = c;
                } else {
                    self.cpsd[pi][bin] = self.alpha * self.cpsd[pi][bin] + (1.0 - self.alpha) * c;
                }
            }
        }
    }

    /// SRP 扫描与置信度（要求 spectrum 已更新）。
    fn estimate(&mut self) -> Option<Observation> {
        // 1. 计算每项加权投票
        let mut vote_weight_sum = 0.0f32;
        let mut msc_num = 0.0f32;
        let mut msc_den = 0.0f32;
        let mut valid_terms = 0usize;

        for ti in 0..self.steering.term_count() {
            let term = self.steering.terms()[ti];
            let (i, j) = MIC_PAIRS[term.pair];
            let bin = term.bin;
            let pi = self.psd[i][bin];
            let pj = self.psd[j][bin];
            if !(pi.is_finite() && pj.is_finite()) || pi < EPS_POWER || pj < EPS_POWER {
                self.weighted_vote[ti] = Complex32::default();
                continue;
            }
            let c = self.cpsd[term.pair][bin];
            if !(c.re.is_finite() && c.im.is_finite()) {
                self.weighted_vote[ti] = Complex32::default();
                continue;
            }
            let rho = c / (pi * pj + EPS_POWER).sqrt();
            let gamma = rho.norm();
            if !gamma.is_finite() {
                self.weighted_vote[ti] = Complex32::default();
                continue;
            }
            let gamma = gamma.clamp(0.0, 1.0);
            let unit_phase = if gamma > EPS_POWER {
                rho / gamma
            } else {
                Complex32::default()
            };
            let band_w = band_weight(term.pair, bin);
            // mean_msc 必须覆盖所有频谱有效项，不能先按 coherence gate 剔除低相干项。
            msc_num += band_w * gamma * gamma;
            msc_den += band_w;

            let coherence_gate = smoothstep(0.15, 0.55, gamma);
            let beta_weight = gamma.powf(1.0 - self.beta);
            let vote_weight = band_w * coherence_gate * beta_weight;
            if vote_weight.is_finite() && vote_weight > 0.0 {
                self.weighted_vote[ti] = unit_phase * vote_weight;
                vote_weight_sum += vote_weight;
                valid_terms += 1;
            } else {
                self.weighted_vote[ti] = Complex32::default();
            }
        }
        let mean_msc = if msc_den > 0.0 {
            msc_num / msc_den
        } else {
            0.0
        };

        if valid_terms == 0 || !vote_weight_sum.is_finite() || vote_weight_sum <= EPS_SCORE {
            return None;
        }

        // 2. 360° 扫描
        for angle in 0..ANGLE_COUNT {
            let steer = self.steering.steer_slice(angle);
            let mut s = 0.0f32;
            for (v, st) in self.weighted_vote.iter().zip(steer.iter()) {
                s += (v * st).re;
            }
            self.score[angle] = s / vote_weight_sum;
        }
        if self.score.iter().any(|s| !s.is_finite()) {
            return None;
        }

        // 3. 主峰 + 抛物线插值
        let mut peak_index = 0usize;
        for a in 1..ANGLE_COUNT {
            if self.score[a] > self.score[peak_index] {
                peak_index = a;
            }
        }
        let peak_score = self.score[peak_index];
        let y_minus = self.score[(peak_index + ANGLE_COUNT - 1) % ANGLE_COUNT];
        let y_plus = self.score[(peak_index + 1) % ANGLE_COUNT];
        let den = y_minus - 2.0 * peak_score + y_plus;
        let delta = if den.abs() > EPS_SCORE {
            (0.5 * (y_minus - y_plus) / den).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let raw_internal_deg = wrap_360(peak_index as f32 + delta);

        // 4. 次峰（排除主峰 ±20°）
        let mut second_peak_score = f32::NEG_INFINITY;
        for a in 0..ANGLE_COUNT {
            if circular_distance_deg(a as f32, peak_index as f32) <= 20.0 {
                continue;
            }
            if self.score[a] > second_peak_score {
                second_peak_score = self.score[a];
            }
        }
        let second_peak_score = if second_peak_score.is_finite() {
            second_peak_score
        } else {
            peak_score
        };
        let peak_gap_ratio = (peak_score - second_peak_score).max(0.0) / peak_score.abs().max(1e-6);

        // 5. robust prominence（median / MAD）
        self.median_scratch.copy_from_slice(&self.score);
        self.median_scratch.sort_by(|a, b| a.total_cmp(b));
        let median = self.median_scratch[ANGLE_COUNT / 2];
        for (s, m) in self.median_scratch.iter_mut().zip(self.score.iter()) {
            *s = (m - median).abs();
        }
        self.median_scratch.sort_by(|a, b| a.total_cmp(b));
        let mad = self.median_scratch[ANGLE_COUNT / 2];
        let prominence = (peak_score - median) / mad.max(1e-6);

        // 6. 置信度
        let q_energy = smoothstep(-60.0, -40.0, self.rms_dbfs);
        let q_msc = smoothstep(0.15, 0.55, mean_msc);
        let q_gap = smoothstep(0.02, 0.15, peak_gap_ratio);
        let q_prom = smoothstep(2.0, 6.0, prominence);
        let spectral_quality = (q_msc * q_gap * q_prom).cbrt();
        let confidence = (q_energy * spectral_quality).clamp(0.0, 1.0);

        Some(Observation {
            raw_internal_deg,
            confidence,
            peak_score,
            second_peak_score,
            peak_gap_ratio,
            prominence,
            mean_msc,
            rms_dbfs: self.rms_dbfs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doa::framer::FrameAssembler;
    use crate::doa::geometry::RESPEAKER_V2_MICS_M;
    use std::f32::consts::PI;

    /// 合成确定性的多音平面波（多频固定相位，覆盖不同频段）。
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

    /// 内部角度转换（测试用，与 DoaProcessor 输出转换一致：默认无 offset/顺时针）。
    fn output_deg(internal: f32) -> f32 {
        wrap_360(internal)
    }

    fn circular_mean_deg(angles: &[f32]) -> f32 {
        let (mut sx, mut sy) = (0.0f32, 0.0f32);
        for &a in angles {
            sx += (a.to_radians()).sin();
            sy += (a.to_radians()).cos();
        }
        // atan2(y=sum_sin, x=sum_cos)
        sx.atan2(sy).to_degrees().rem_euclid(360.0)
    }

    /// 合成方向测试：8 个方向，稳定后 raw 误差 <= 2°。
    #[test]
    fn synthetic_plane_wave_directions() {
        for &theta in &[0.0f32, 45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0] {
            let mut srp = SrpPhatBeta::new(0.75, 100.0).unwrap();
            let signal = synth_plane_wave(theta, 1.2);
            let mut assembler = FrameAssembler::new();
            let mut frame_count = 0u64;
            let mut obs: Vec<Observation> = Vec::new();
            for f in signal.chunks_exact(MIC_COUNT) {
                let sample = [
                    f[0] as f32 / 32768.0,
                    f[1] as f32 / 32768.0,
                    f[2] as f32 / 32768.0,
                    f[3] as f32 / 32768.0,
                ];
                if assembler.push(sample) {
                    frame_count += 1;
                    let mut frame = [[0.0f32; FRAME_SIZE]; MIC_COUNT];
                    assembler.copy_frame(&mut frame);
                    if let Some(o) = srp.process(&frame, frame_count >= 3) {
                        obs.push(o);
                    }
                }
            }
            assert!(obs.len() >= 10, "theta={theta} 有效观测太少: {}", obs.len());
            // 稳定段：取最后 5 帧的圆周均值
            let tail: Vec<f32> = obs[obs.len() - 5..]
                .iter()
                .map(|o| output_deg(o.raw_internal_deg))
                .collect();
            let mean = circular_mean_deg(&tail);
            let err = circular_distance_deg(mean, theta);
            assert!(
                err <= 2.0,
                "theta={theta} raw 误差 {err}° (mean={mean}, tail={tail:?})"
            );
        }
    }

    /// 全零输入：无有效观测，不 panic。
    #[test]
    fn zero_input_no_observation() {
        let mut srp = SrpPhatBeta::new(0.75, 100.0).unwrap();
        let mut assembler = FrameAssembler::new();
        let mut frame_count = 0u64;
        let mut obs = 0usize;
        for _ in 0..2000 {
            if assembler.push([0.0; MIC_COUNT]) {
                frame_count += 1;
                let mut frame = [[0.0f32; FRAME_SIZE]; MIC_COUNT];
                assembler.copy_frame(&mut frame);
                if srp.process(&frame, frame_count >= 3).is_some() {
                    obs += 1;
                }
            }
        }
        assert_eq!(obs, 0);
    }

    #[test]
    fn mean_msc_includes_low_coherence_terms() {
        let mut srp = SrpPhatBeta::new(0.75, 100.0).unwrap();
        for ch in 0..MIC_COUNT {
            srp.psd[ch].fill(1.0);
        }
        for pair in 0..MIC_PAIRS.len() {
            for bin in 0..FFT_BINS {
                let gamma = if bin % 2 == 0 { 1.0 } else { 0.1 };
                srp.cpsd[pair][bin] = Complex32::new(gamma, 0.0);
            }
        }
        srp.rms_dbfs = -20.0;

        let mut expected_num = 0.0f32;
        let mut expected_den = 0.0f32;
        for term in srp.steering.terms() {
            let gamma = if term.bin % 2 == 0 { 1.0 } else { 0.1 };
            let weight = band_weight(term.pair, term.bin);
            expected_num += weight * gamma * gamma;
            expected_den += weight;
        }

        let observation = srp.estimate().unwrap();
        let expected = expected_num / expected_den;
        assert!((observation.mean_msc - expected).abs() < 1e-6);
        assert!(observation.mean_msc < 0.75);
    }
}
