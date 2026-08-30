//! Beamformer 权重 LUT：Delay-and-Sum 与鲁棒超指向 MVDR。

use realfft::num_complex::Complex32;

use crate::beamformer::matrix::{chol_solve4_complex, cholesky4};
use crate::beamformer::{BeamformerAlgorithm, BeamformerConfig};
use crate::doa::geometry::{RESPEAKER_V2_MICS_M, SPEED_OF_SOUND, smoothstep};
use crate::doa::{ANGLE_COUNT, FFT_BINS, FRAME_SIZE, MIC_COUNT, SAMPLE_RATE};

const INITIAL_LOADING: f64 = 1e-10;
const MAXIMUM_LOADING: f64 = 1e4;
const BINARY_ITERS: usize = 32;
const DISTORTIONLESS_TOL: f64 = 1e-4;

/// 预计算权重表：`360 × 257 × 4` Complex32。
pub struct WeightLut {
    /// 扁平存储：`((angle * FFT_BINS) + bin) * MIC_COUNT + mic`
    weights: Vec<Complex32>,
    pub das_fallback_bins: u64,
    pub min_generated_wng_db: f32,
}

#[inline]
fn bin_hz(bin: usize) -> f32 {
    bin as f32 * SAMPLE_RATE as f32 / FRAME_SIZE as f32
}

#[inline]
fn lut_index(angle: usize, bin: usize, mic: usize) -> usize {
    ((angle * FFT_BINS) + bin) * MIC_COUNT + mic
}

/// `sinc(x) = sin(x)/x`，`sinc(0)=1`。
pub fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 { 1.0 } else { x.sin() / x }
}

/// 超指向与 DAS 的频带混合系数。
pub fn frequency_mix(
    freq_hz: f32,
    low_start: f32,
    low_full: f32,
    high_full: f32,
    high_end: f32,
) -> f32 {
    if freq_hz <= low_start || freq_hz >= high_end {
        0.0
    } else if freq_hz < low_full {
        smoothstep(low_start, low_full, freq_hz)
    } else if freq_hz <= high_full {
        1.0
    } else {
        1.0 - smoothstep(high_full, high_end, freq_hz)
    }
}

/// Steering：`d_m = exp(j 2π f τ_m)`，`τ_m = dot(r_m, u)/c`。
pub fn steering_vector(angle_deg: f32, bin: usize) -> [Complex32; MIC_COUNT] {
    let theta = angle_deg.to_radians();
    let u = [theta.cos(), theta.sin()];
    let f = bin_hz(bin);
    let mut d = [Complex32::new(0.0, 0.0); MIC_COUNT];
    for (m, r) in RESPEAKER_V2_MICS_M.iter().enumerate() {
        let tau = (r[0] * u[0] + r[1] * u[1]) / SPEED_OF_SOUND;
        let phase = std::f32::consts::TAU * f * tau;
        d[m] = Complex32::from_polar(1.0, phase);
    }
    d
}

fn steering_vector_f64(angle_deg: f32, bin: usize) -> ([f64; MIC_COUNT], [f64; MIC_COUNT]) {
    let d = steering_vector(angle_deg, bin);
    let mut re = [0.0f64; MIC_COUNT];
    let mut im = [0.0f64; MIC_COUNT];
    for m in 0..MIC_COUNT {
        re[m] = d[m].re as f64;
        im[m] = d[m].im as f64;
    }
    (re, im)
}

/// 3D diffuse coherence：`Γ_mn = sinc(2π f ||rm-rn|| / c)`。
pub fn diffuse_covariance(bin: usize) -> [[f64; MIC_COUNT]; MIC_COUNT] {
    let f = bin_hz(bin) as f64;
    let c = SPEED_OF_SOUND as f64;
    let mut g = [[0.0f64; MIC_COUNT]; MIC_COUNT];
    for m in 0..MIC_COUNT {
        g[m][m] = 1.0;
        for n in (m + 1)..MIC_COUNT {
            let rm = RESPEAKER_V2_MICS_M[m];
            let rn = RESPEAKER_V2_MICS_M[n];
            let dx = (rm[0] - rn[0]) as f64;
            let dy = (rm[1] - rn[1]) as f64;
            let dist = (dx * dx + dy * dy).sqrt();
            let v = sinc(std::f64::consts::TAU * f * dist / c);
            g[m][n] = v;
            g[n][m] = v;
        }
    }
    g
}

fn das_weights(d: &[Complex32; MIC_COUNT]) -> [Complex32; MIC_COUNT] {
    let inv_m = 1.0 / MIC_COUNT as f32;
    std::array::from_fn(|m| d[m] * inv_m)
}

fn dc_das_weights() -> [Complex32; MIC_COUNT] {
    let inv_m = 1.0 / MIC_COUNT as f32;
    [Complex32::new(inv_m, 0.0); MIC_COUNT]
}

fn zero_weights() -> [Complex32; MIC_COUNT] {
    [Complex32::new(0.0, 0.0); MIC_COUNT]
}

fn wh_d(w: &[Complex32; MIC_COUNT], d: &[Complex32; MIC_COUNT]) -> Complex32 {
    let mut s = Complex32::new(0.0, 0.0);
    for m in 0..MIC_COUNT {
        s += w[m].conj() * d[m];
    }
    s
}

fn wng_linear(w: &[Complex32; MIC_COUNT]) -> f64 {
    let mut power = 0.0f64;
    for c in w {
        let re = c.re as f64;
        let im = c.im as f64;
        power += re * re + im * im;
    }
    if power <= 0.0 { 0.0 } else { 1.0 / power }
}

fn wng_db(w: &[Complex32; MIC_COUNT]) -> f32 {
    let lin = wng_linear(w);
    if lin <= 0.0 {
        f32::NEG_INFINITY
    } else {
        (10.0 * lin.log10()) as f32
    }
}

fn weights_finite(w: &[Complex32; MIC_COUNT]) -> bool {
    w.iter().all(|c| c.re.is_finite() && c.im.is_finite())
}

fn renormalize_distortionless(
    w: &[Complex32; MIC_COUNT],
    d: &[Complex32; MIC_COUNT],
) -> Option<[Complex32; MIC_COUNT]> {
    let den = wh_d(w, d);
    let n2 = den.norm_sqr();
    if n2 < 1e-20 || !n2.is_finite() {
        return None;
    }
    // 需要 (w')^H d = c*·den = 1，即 c = 1/conj(den) = den / |den|²。
    // 直接除 den 会残留相位旋转（仅当 den 为实数时才正确）。
    let scale = den / n2;
    let out = std::array::from_fn(|m| w[m] * scale);
    if !weights_finite(&out) {
        return None;
    }
    Some(out)
}

/// 尝试计算给定 loading 的 MVDR 权重；失败返回 None。
pub fn try_mvdr_weights(
    gamma: &[[f64; MIC_COUNT]; MIC_COUNT],
    d_re: &[f64; MIC_COUNT],
    d_im: &[f64; MIC_COUNT],
    loading: f64,
) -> Option<[Complex32; MIC_COUNT]> {
    let mut r = *gamma;
    for (i, row) in r.iter_mut().enumerate() {
        row[i] += loading;
    }
    let l = cholesky4(&r).ok()?;
    let (zr, zi) = chol_solve4_complex(&l, d_re, d_im);
    // den = d^H z
    let mut den_re = 0.0f64;
    let mut den_im = 0.0f64;
    for m in 0..MIC_COUNT {
        // conj(d) * z
        den_re += d_re[m] * zr[m] + d_im[m] * zi[m];
        den_im += d_re[m] * zi[m] - d_im[m] * zr[m];
    }
    let den_n2 = den_re * den_re + den_im * den_im;
    if den_n2 < 1e-30 || !den_n2.is_finite() {
        return None;
    }
    let inv_re = den_re / den_n2;
    let inv_im = -den_im / den_n2;
    let mut w = [Complex32::new(0.0, 0.0); MIC_COUNT];
    for m in 0..MIC_COUNT {
        // w = z / (d^H z) = z * conj(den)/|den|^2
        let re = zr[m] * inv_re - zi[m] * inv_im;
        let im = zr[m] * inv_im + zi[m] * inv_re;
        if !re.is_finite() || !im.is_finite() {
            return None;
        }
        w[m] = Complex32::new(re as f32, im as f32);
    }
    if !weights_finite(&w) {
        return None;
    }
    Some(w)
}

fn find_loaded_sd(
    gamma: &[[f64; MIC_COUNT]; MIC_COUNT],
    d_re: &[f64; MIC_COUNT],
    d_im: &[f64; MIC_COUNT],
    target_wng: f64,
) -> Option<[Complex32; MIC_COUNT]> {
    let lambda = INITIAL_LOADING;
    let w0 = try_mvdr_weights(gamma, d_re, d_im, lambda)?;
    if wng_linear(&w0) >= target_wng {
        return Some(w0);
    }

    let mut lambda_low = lambda;
    let mut lambda_high = lambda;
    let mut w_high = w0;
    let mut found = false;
    for _ in 0..64 {
        lambda_high *= 10.0;
        if lambda_high > MAXIMUM_LOADING {
            lambda_high = MAXIMUM_LOADING;
        }
        {
            let w = try_mvdr_weights(gamma, d_re, d_im, lambda_high)?;
            w_high = w;
            if wng_linear(&w_high) >= target_wng {
                found = true;
                break;
            }
        }
        if lambda_high >= MAXIMUM_LOADING {
            break;
        }
        lambda_low = lambda_high;
    }
    if !found {
        return None;
    }

    let mut best = w_high;
    let mut lo = lambda_low;
    let mut hi = lambda_high;
    for _ in 0..BINARY_ITERS {
        let mid = (lo * hi).sqrt().clamp(INITIAL_LOADING, MAXIMUM_LOADING);
        {
            let w = try_mvdr_weights(gamma, d_re, d_im, mid)?;
            if wng_linear(&w) >= target_wng {
                best = w;
                hi = mid;
            } else {
                lo = mid;
            }
        }
    }
    Some(best)
}

fn blend_and_enforce_wng(
    w_das: &[Complex32; MIC_COUNT],
    w_sd: &[Complex32; MIC_COUNT],
    d: &[Complex32; MIC_COUNT],
    mix: f32,
    target_wng: f64,
) -> Option<[Complex32; MIC_COUNT]> {
    // 若混合后 WNG 不足，逐步增大 DAS 比例。
    let mut m = mix.clamp(0.0, 1.0);
    for _ in 0..33 {
        let blended = std::array::from_fn(|i| {
            let a = 1.0 - m;
            Complex32::new(
                a * w_das[i].re + m * w_sd[i].re,
                a * w_das[i].im + m * w_sd[i].im,
            )
        });
        let Some(w) = renormalize_distortionless(&blended, d) else {
            m *= 0.5;
            continue;
        };
        if wng_linear(&w) >= target_wng {
            return Some(w);
        }
        if m <= 0.0 {
            return renormalize_distortionless(w_das, d);
        }
        m *= 0.5;
    }
    renormalize_distortionless(w_das, d)
}

impl WeightLut {
    pub fn generate(config: &BeamformerConfig) -> Result<Self, String> {
        let mut weights = vec![Complex32::new(0.0, 0.0); ANGLE_COUNT * FFT_BINS * MIC_COUNT];
        let mut das_fallback_bins = 0u64;
        let mut min_wng = f32::INFINITY;
        let target_wng = 10f64.powf(config.min_wng_db as f64 / 10.0);
        let nyquist = FFT_BINS - 1;

        for angle in 0..ANGLE_COUNT {
            for bin in 0..FFT_BINS {
                let w = if bin == nyquist {
                    zero_weights()
                } else if bin == 0 {
                    dc_das_weights()
                } else {
                    let d = steering_vector(angle as f32, bin);
                    let w_das = das_weights(&d);
                    let chosen = match config.algorithm {
                        BeamformerAlgorithm::DelaySum => w_das,
                        BeamformerAlgorithm::RobustSuperdirective => {
                            let freq = bin_hz(bin);
                            let mix = frequency_mix(
                                freq,
                                config.sd_low_start_hz,
                                config.sd_low_full_hz,
                                config.sd_high_full_hz,
                                config.sd_high_end_hz,
                            );
                            if mix <= 0.0 {
                                w_das
                            } else {
                                let gamma = diffuse_covariance(bin);
                                let (d_re, d_im) = steering_vector_f64(angle as f32, bin);
                                match find_loaded_sd(&gamma, &d_re, &d_im, target_wng) {
                                    Some(w_sd) => {
                                        match blend_and_enforce_wng(
                                            &w_das, &w_sd, &d, mix, target_wng,
                                        ) {
                                            Some(w) => w,
                                            None => {
                                                das_fallback_bins += 1;
                                                w_das
                                            }
                                        }
                                    }
                                    None => {
                                        das_fallback_bins += 1;
                                        w_das
                                    }
                                }
                            }
                        }
                    };
                    // 最终校验：非有限或破坏无失真约束则回退 DAS。
                    let d = steering_vector(angle as f32, bin);
                    if !weights_finite(&chosen)
                        || (wh_d(&chosen, &d) - Complex32::new(1.0, 0.0)).norm()
                            > DISTORTIONLESS_TOL as f32
                    {
                        das_fallback_bins += 1;
                        das_weights(&d)
                    } else {
                        chosen
                    }
                };

                if bin != nyquist {
                    let db = wng_db(&w);
                    if db.is_finite() {
                        min_wng = min_wng.min(db);
                    }
                }
                for m in 0..MIC_COUNT {
                    weights[lut_index(angle, bin, m)] = w[m];
                }
            }
        }

        if !min_wng.is_finite() {
            min_wng = 0.0;
        }

        Ok(Self {
            weights,
            das_fallback_bins,
            min_generated_wng_db: min_wng,
        })
    }

    pub fn weights_for(&self, angle_deg: f32) -> &[Complex32] {
        let idx = crate::doa::wrap_360(angle_deg.round()).round() as usize % ANGLE_COUNT;
        let start = idx * FFT_BINS * MIC_COUNT;
        &self.weights[start..start + FFT_BINS * MIC_COUNT]
    }

    /// 取出单 bin 的 4 路权重（测试用）。
    #[cfg(test)]
    pub fn bin_weights(&self, angle: usize, bin: usize) -> [Complex32; MIC_COUNT] {
        std::array::from_fn(|m| self.weights[lut_index(angle, bin, m)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beamformer::{BeamformerAlgorithm, BeamformerConfig, BeamformerDirectionSource};

    const WNG_MARGIN_DB: f32 = 0.05;

    fn sd_config() -> BeamformerConfig {
        BeamformerConfig {
            enabled: true,
            algorithm: BeamformerAlgorithm::RobustSuperdirective,
            direction_source: BeamformerDirectionSource::Fixed,
            fixed_internal_angle_deg: 0.0,
            fallback_internal_angle_deg: 0.0,
            direction_smoothing_ms: 0.0,
            min_wng_db: 3.0,
            sd_low_start_hz: 350.0,
            sd_low_full_hz: 500.0,
            sd_high_full_hz: 2500.0,
            sd_high_end_hz: 3500.0,
            output_gain_db: -3.0,
            wav: false,
            compare_wav: false,
            enable_drc: false,
        }
    }

    #[test]
    fn diffuse_covariance_is_symmetric_with_unit_diagonal() {
        for bin in [0usize, 1, 16, 80, 112, 256] {
            let g = diffuse_covariance(bin);
            for (i, row) in g.iter().enumerate() {
                assert!((row[i] - 1.0).abs() < 1e-12);
                for (j, &v) in row.iter().enumerate() {
                    assert!((v - g[j][i]).abs() < 1e-12);
                    assert!(v.is_finite());
                    assert!((-1.0 - 1e-9..=1.0 + 1e-9).contains(&v));
                }
            }
        }
    }

    #[test]
    fn steering_vector_has_unit_norm_elements() {
        for angle in [0.0f32, 45.0, 137.0, 359.0] {
            for bin in [0usize, 1, 32, 100, 256] {
                let d = steering_vector(angle, bin);
                for c in &d {
                    assert!((c.norm() - 1.0).abs() < 1e-5);
                }
            }
        }
    }

    #[test]
    fn das_weights_are_distortionless() {
        for angle in 0..ANGLE_COUNT {
            for bin in 0..(FFT_BINS - 1) {
                let d = steering_vector(angle as f32, bin);
                let w = if bin == 0 {
                    dc_das_weights()
                } else {
                    das_weights(&d)
                };
                let g = wh_d(&w, &d);
                assert!(
                    (g - Complex32::new(1.0, 0.0)).norm() < 1e-4,
                    "angle={angle} bin={bin} g={g:?}"
                );
            }
        }
    }

    #[test]
    fn renormalize_distortionless_handles_complex_response() {
        // den = w^H d 为纯虚数 j：旧实现（除以 den）会得到响应 -1。
        let w = [
            Complex32::new(1.0, 0.0),
            Complex32::new(0.0, 0.0),
            Complex32::new(0.0, 0.0),
            Complex32::new(0.0, 0.0),
        ];
        let d = [
            Complex32::new(0.0, 1.0),
            Complex32::new(1.0, 0.0),
            Complex32::new(1.0, 0.0),
            Complex32::new(1.0, 0.0),
        ];

        let normalized = renormalize_distortionless(&w, &d).unwrap();
        let response = wh_d(&normalized, &d);

        assert!(
            (response - Complex32::new(1.0, 0.0)).norm() < 1e-6,
            "response={response:?}"
        );
    }

    #[test]
    fn superdirective_weights_are_distortionless() {
        let lut = WeightLut::generate(&sd_config()).unwrap();
        for angle in [0usize, 45, 90, 180, 270] {
            for bin in 0..(FFT_BINS - 1) {
                let d = steering_vector(angle as f32, bin);
                let w = lut.bin_weights(angle, bin);
                let g = wh_d(&w, &d);
                assert!(
                    (g - Complex32::new(1.0, 0.0)).norm() < 1e-4,
                    "angle={angle} bin={bin} g={g:?}"
                );
            }
        }
    }

    #[test]
    fn all_non_nyquist_generated_weights_meet_wng_floor() {
        let cfg = sd_config();
        let lut = WeightLut::generate(&cfg).unwrap();
        let floor = cfg.min_wng_db - WNG_MARGIN_DB;
        for angle in 0..ANGLE_COUNT {
            for bin in 0..(FFT_BINS - 1) {
                let w = lut.bin_weights(angle, bin);
                let db = wng_db(&w);
                assert!(
                    db + 1e-4 >= floor,
                    "angle={angle} bin={bin} wng={db} floor={floor}"
                );
            }
        }
        assert!(lut.min_generated_wng_db + 1e-4 >= floor);
    }

    #[test]
    fn dc_weights_are_real_and_nyquist_is_zero() {
        let lut = WeightLut::generate(&sd_config()).unwrap();
        let nyquist = FFT_BINS - 1;
        for angle in 0..ANGLE_COUNT {
            let dc = lut.bin_weights(angle, 0);
            for c in &dc {
                assert!(c.im.abs() < 1e-7);
                assert!((c.re - 0.25).abs() < 1e-6);
            }
            let ny = lut.bin_weights(angle, nyquist);
            for c in &ny {
                assert_eq!(*c, Complex32::new(0.0, 0.0));
            }
        }
    }

    #[test]
    fn invalid_or_singular_case_falls_back_to_das() {
        // 全零协方差 + 零 loading → Cholesky 失败。
        let gamma = [[0.0f64; MIC_COUNT]; MIC_COUNT];
        let d_re = [1.0, 1.0, 1.0, 1.0];
        let d_im = [0.0, 0.0, 0.0, 0.0];
        assert!(try_mvdr_weights(&gamma, &d_re, &d_im, 0.0).is_none());

        // 生成路径遇失败时应得到与 DAS 一致的权重（通过强制 SD 在失败时回退）。
        let lut = WeightLut::generate(&sd_config()).unwrap();
        let angle = 30usize;
        let bin = 40usize;
        let d = steering_vector(angle as f32, bin);
        let w = lut.bin_weights(angle, bin);
        // 即便部分 bin 回退，最终仍须无失真。
        assert!((wh_d(&w, &d) - Complex32::new(1.0, 0.0)).norm() < 1e-4);
        // find_loaded_sd 在无法 bracket 时返回 None，调用方写入 DAS。
        assert!(find_loaded_sd(&gamma, &d_re, &d_im, 1e9).is_none());
    }

    #[test]
    fn frequency_mix_has_expected_boundaries() {
        let (ls, lf, hf, he) = (350.0, 500.0, 2500.0, 3500.0);
        assert_eq!(frequency_mix(0.0, ls, lf, hf, he), 0.0);
        assert_eq!(frequency_mix(350.0, ls, lf, hf, he), 0.0);
        assert!((frequency_mix(500.0, ls, lf, hf, he) - 1.0).abs() < 1e-5);
        assert_eq!(frequency_mix(1000.0, ls, lf, hf, he), 1.0);
        assert_eq!(frequency_mix(2500.0, ls, lf, hf, he), 1.0);
        assert!(frequency_mix(3000.0, ls, lf, hf, he) > 0.0);
        assert!(frequency_mix(3000.0, ls, lf, hf, he) < 1.0);
        assert_eq!(frequency_mix(3500.0, ls, lf, hf, he), 0.0);
        assert_eq!(frequency_mix(8000.0, ls, lf, hf, he), 0.0);
        let mid_low = frequency_mix(425.0, ls, lf, hf, he);
        assert!(mid_low > 0.0 && mid_low < 1.0);
    }
}
