//! 阵列几何、pair 与 steering LUT。
//!
//! ReSpeaker Mic Array v2.0 的 4 个 mic 位于半径 32mm 的圆上（ch1..ch4，
//! 顺序对应拆分后 `*_respeaker_mic.wav` 的 1..4 通道），平面坐标单位为米：
//!
//! ```text
//!       mic0 (-0.032, 0)     mic1 (0, -0.032)
//!       mic2 (+0.032, 0)     mic3 (0, +0.032)
//! ```
//!
//! 相邻基线 = √2·0.032 ≈ 0.0452548 m；相对基线 = 0.064 m。理论空间混叠
//! 频率约为 adjacent 3790 Hz / opposite 2680 Hz，因此频带保守限制为
//! adjacent 3500 Hz / opposite 2500 Hz。

use realfft::num_complex::Complex32;

use crate::doa::{ANGLE_COUNT, FFT_BINS, FRAME_SIZE, MIC_COUNT, SAMPLE_RATE};

/// 声速（m/s）。
pub const SPEED_OF_SOUND: f32 = 343.0;

/// 4 个 mic 平面坐标（米）。
pub const RESPEAKER_V2_MICS_M: [[f32; 2]; MIC_COUNT] = [
    [-0.032, 0.000],
    [0.000, -0.032],
    [0.032, 0.000],
    [0.000, 0.032],
];

/// 6 个互谱 pair：(0,1) 相邻、(0,2) 相对、(0,3) 相邻、(1,2) 相邻、(1,3) 相对、(2,3) 相邻。
pub const MIC_PAIRS: [(usize, usize); 6] = [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)];

/// pair 是否为相对（对径）对：(0,2) 与 (1,3)。
pub fn pair_is_opposite(pair: usize) -> bool {
    pair == 1 || pair == 4
}

/// smoothstep（用于频带淡入淡出与相干度门控）。
pub fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge0 == edge1 {
        return if x < edge0 { 0.0 } else { 1.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn bin_hz(bin: usize) -> f32 {
    bin as f32 * SAMPLE_RATE as f32 / FRAME_SIZE as f32
}

/// 频带权重：低频 250→400 Hz 平滑淡入；高频 adjacent 3250→3500 Hz、
/// opposite 2250→2500 Hz 平滑淡出。`band_weight > 0` 的项才进入投票。
pub fn band_weight(pair: usize, bin: usize) -> f32 {
    let f = bin_hz(bin);
    let low = if f <= 250.0 {
        0.0
    } else if f < 400.0 {
        smoothstep(250.0, 400.0, f)
    } else {
        1.0
    };
    let high = if pair_is_opposite(pair) {
        if f <= 2250.0 {
            1.0
        } else if f < 2500.0 {
            1.0 - smoothstep(2250.0, 2500.0, f)
        } else {
            0.0
        }
    } else if f <= 3250.0 {
        1.0
    } else if f < 3500.0 {
        1.0 - smoothstep(3250.0, 3500.0, f)
    } else {
        0.0
    };
    low * high
}

/// 一个投票项：pair 索引 + FFT bin。
#[derive(Clone, Copy, Debug)]
pub struct SteeringTerm {
    pub pair: usize,
    pub bin: usize,
}

/// steering LUT：`steer[angle * term_count + term_index]`。
///
/// `term_index` 与本帧 `weighted_vote[term_index]` 完全一致，便于顺序连续读取。
pub struct SteeringLut {
    terms: Vec<SteeringTerm>,
    steer: Vec<Complex32>,
    term_count: usize,
}

impl SteeringLut {
    /// 预计算所有有效（band_weight > 0）pair/bin 项，以及 360 个角度的 steering。
    pub fn new() -> Self {
        let mut terms: Vec<SteeringTerm> = Vec::new();
        for pair in 0..MIC_PAIRS.len() {
            for bin in 0..FFT_BINS {
                if band_weight(pair, bin) > 0.0 {
                    terms.push(SteeringTerm { pair, bin });
                }
            }
        }
        let term_count = terms.len();
        let mut steer = vec![Complex32::default(); ANGLE_COUNT * term_count];
        for angle in 0..ANGLE_COUNT {
            let theta = (angle as f32).to_radians();
            let u = [theta.cos(), theta.sin()];
            for (ti, term) in terms.iter().enumerate() {
                let (i, j) = MIC_PAIRS[term.pair];
                let ri = RESPEAKER_V2_MICS_M[i];
                let rj = RESPEAKER_V2_MICS_M[j];
                // tau_ij(theta) = dot(r_i - r_j, u) / c
                let tau = ((ri[0] - rj[0]) * u[0] + (ri[1] - rj[1]) * u[1]) / SPEED_OF_SOUND;
                let phase = -std::f32::consts::TAU * bin_hz(term.bin) * tau;
                steer[angle * term_count + ti] = Complex32::from_polar(1.0, phase);
            }
        }
        Self {
            terms,
            steer,
            term_count,
        }
    }

    pub fn terms(&self) -> &[SteeringTerm] {
        &self.terms
    }

    pub fn term_count(&self) -> usize {
        self.term_count
    }

    /// 返回第 `angle` 个角度的 steering 切片（长度 term_count）。
    pub fn steer_slice(&self, angle: usize) -> &[Complex32] {
        let start = angle * self.term_count;
        &self.steer[start..start + self.term_count]
    }
}

impl Default for SteeringLut {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_coordinates_and_pairs() {
        // 4 个坐标
        assert_eq!(RESPEAKER_V2_MICS_M[0], [-0.032, 0.0]);
        assert_eq!(RESPEAKER_V2_MICS_M[1], [0.0, -0.032]);
        assert_eq!(RESPEAKER_V2_MICS_M[2], [0.032, 0.0]);
        assert_eq!(RESPEAKER_V2_MICS_M[3], [0.0, 0.032]);

        // 6 对：i < j，无重复，覆盖全部组合
        let mut seen = std::collections::HashSet::new();
        for &(i, j) in &MIC_PAIRS {
            assert!(i < j);
            assert!(j < MIC_COUNT);
            assert!(seen.insert((i, j)), "duplicate pair ({i},{j})");
        }
        assert_eq!(seen.len(), 6);
        // 覆盖全部 4 选 2 = 6 组合
        for i in 0..MIC_COUNT {
            for j in (i + 1)..MIC_COUNT {
                assert!(seen.contains(&(i, j)), "missing pair ({i},{j})");
            }
        }
    }

    #[test]
    fn pair_lengths() {
        // 精确验证 4 个相邻对 ≈ 0.0452548，2 个相对对 = 0.064
        let adjacent: f32 = std::f32::consts::SQRT_2 * 0.032;
        for (idx, &(i, j)) in MIC_PAIRS.iter().enumerate() {
            let ri = RESPEAKER_V2_MICS_M[i];
            let rj = RESPEAKER_V2_MICS_M[j];
            let d = ((ri[0] - rj[0]).powi(2) + (ri[1] - rj[1]).powi(2)).sqrt();
            let expected = if pair_is_opposite(idx) {
                0.064
            } else {
                adjacent
            };
            assert!(
                (d - expected).abs() < 1e-6,
                "pair {idx} len {d} != {expected}"
            );
        }
    }

    #[test]
    fn band_weight_shape() {
        // 低频 250 Hz 以下为 0，400 Hz 以上为 1
        let bin8 = (250.0f32 / (SAMPLE_RATE as f32 / FRAME_SIZE as f32)).round() as usize; // ~8
        assert_eq!(band_weight(0, 0), 0.0);
        assert!((band_weight(0, bin8) - 0.0).abs() < 0.05);
        let bin400 = (400.0f32 / 31.25).round() as usize; // 13
        assert_eq!(band_weight(0, bin400), 1.0);
        // adjacent 3500 Hz 以上为 0，opposite 2500 Hz 以上为 0
        let bin3500 = (3500.0f32 / 31.25).round() as usize; // 112
        assert_eq!(band_weight(0, bin3500), 0.0);
        let bin2500 = (2500.0f32 / 31.25).round() as usize; // 80
        assert_eq!(band_weight(1, bin2500), 0.0);
    }

    #[test]
    fn steering_lut_builds() {
        let lut = SteeringLut::new();
        assert!(lut.term_count() > 0);
        assert_eq!(lut.steer.len(), ANGLE_COUNT * lut.term_count());
        // 所有 steering 模长 ≈ 1
        for a in 0..ANGLE_COUNT {
            for &s in lut.steer_slice(a) {
                let m = s.norm();
                assert!((m - 1.0).abs() < 1e-5, "steer norm {m}");
            }
        }
    }
}
