//! 阵列几何、pair 与 steering LUT。
//!
//! ReSpeaker Mic Array v2.0 的 4 个 mic 组成边长 45.7 mm 的正方形（ch1..ch4，
//! 顺序对应拆分后 `*_respeaker_mic.wav` 的 1..4 通道），阵列中心为原点，
//! 平面坐标单位为米：
//!
//! ```text
//!       mic3/ch3 (-0.02285, +0.02285)   mic2/ch2 (+0.02285, +0.02285)
//!       mic4/ch4 (-0.02285, -0.02285)   mic1/ch1 (+0.02285, -0.02285)
//! ```
//!
//! `+X` 为 0°，`+Y` 为 90°，角度逆时针增加。相邻基线 = 0.0457 m；
//! 相对基线 = √2·0.0457 ≈ 0.06463 m。理论空间混叠频率约为
//! adjacent 3753 Hz / opposite 2654 Hz，因此频带保守限制为
//! adjacent 3500 Hz / opposite 2500 Hz。

use realfft::num_complex::Complex32;

use crate::doa::{ANGLE_COUNT, FFT_BINS, FRAME_SIZE, MIC_COUNT, SAMPLE_RATE};

/// 声速（m/s）。
pub const SPEED_OF_SOUND: f32 = 343.0;

/// 相邻麦克风中心距（米）。
pub const ADJACENT_MIC_DISTANCE_M: f32 = 0.0457;

/// 正方形顶点相对坐标轴的距离，即相邻麦克风中心距的一半（米）。
const MIC_AXIS_COORD_M: f32 = ADJACENT_MIC_DISTANCE_M / 2.0;

/// 4 个物理麦克风的平面坐标（米）；数组索引 0..3 对应 mic1..mic4 / ch1..ch4。
pub const RESPEAKER_V2_MICS_M: [[f32; 2]; MIC_COUNT] = [
    [MIC_AXIS_COORD_M, -MIC_AXIS_COORD_M],
    [MIC_AXIS_COORD_M, MIC_AXIS_COORD_M],
    [-MIC_AXIS_COORD_M, MIC_AXIS_COORD_M],
    [-MIC_AXIS_COORD_M, -MIC_AXIS_COORD_M],
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
        // mic1 在第四象限、mic2 在第一象限、mic3 在第二象限、mic4 在第三象限。
        assert_eq!(RESPEAKER_V2_MICS_M[0], [0.02285, -0.02285]);
        assert_eq!(RESPEAKER_V2_MICS_M[1], [0.02285, 0.02285]);
        assert_eq!(RESPEAKER_V2_MICS_M[2], [-0.02285, 0.02285]);
        assert_eq!(RESPEAKER_V2_MICS_M[3], [-0.02285, -0.02285]);

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
        // 精确验证 4 个相邻对 = 0.0457，2 个相对对 = √2·0.0457。
        let opposite = std::f32::consts::SQRT_2 * ADJACENT_MIC_DISTANCE_M;
        for (idx, &(i, j)) in MIC_PAIRS.iter().enumerate() {
            let ri = RESPEAKER_V2_MICS_M[i];
            let rj = RESPEAKER_V2_MICS_M[j];
            let d = ((ri[0] - rj[0]).powi(2) + (ri[1] - rj[1]).powi(2)).sqrt();
            let expected = if pair_is_opposite(idx) {
                opposite
            } else {
                ADJACENT_MIC_DISTANCE_M
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
