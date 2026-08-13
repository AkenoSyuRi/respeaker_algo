//! ReSpeaker 4-Mic 频域 Beamformer（Delay-and-Sum / 鲁棒超指向 MVDR）。

pub mod matrix;
pub mod stft;
pub mod weights;

use serde::Deserialize;

use crate::beamformer::stft::BeamformerStft;
use crate::beamformer::weights::WeightLut;
use crate::doa::tracker::TrackStatus;
use crate::doa::{DoaResult, HOP_SIZE, MIC_COUNT, SAMPLE_RATE, circular_delta_deg, wrap_360};
use crate::wav::WavSink;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeamformerAlgorithm {
    DelaySum,
    RobustSuperdirective,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeamformerDirectionSource {
    Doa,
    Fixed,
}

#[derive(Clone, Debug)]
pub struct BeamformerConfig {
    /// 由 Pipeline 模块入口决定；构造时恒为 true，保留便于配置镜像。
    #[allow(dead_code)]
    pub enabled: bool,
    pub algorithm: BeamformerAlgorithm,
    pub direction_source: BeamformerDirectionSource,
    pub fixed_internal_angle_deg: f32,
    pub fallback_internal_angle_deg: f32,
    pub direction_smoothing_ms: f32,
    pub min_wng_db: f32,
    pub sd_low_start_hz: f32,
    pub sd_low_full_hz: f32,
    pub sd_high_full_hz: f32,
    pub sd_high_end_hz: f32,
    pub output_gain_db: f32,
    pub wav: bool,
    /// `true` 时 `*_respeaker_bf.wav` 写为双声道对比文件：
    /// 左声道 = mic1（BF 第一路输入）× output_gain_db，右声道 = BF 输出 × output_gain_db。
    pub compare_wav: bool,
}

impl Default for BeamformerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            algorithm: BeamformerAlgorithm::RobustSuperdirective,
            direction_source: BeamformerDirectionSource::Doa,
            fixed_internal_angle_deg: 0.0,
            fallback_internal_angle_deg: 0.0,
            direction_smoothing_ms: 64.0,
            min_wng_db: 3.0,
            sd_low_start_hz: 350.0,
            sd_low_full_hz: 500.0,
            sd_high_full_hz: 2500.0,
            sd_high_end_hz: 3500.0,
            output_gain_db: -3.0,
            wav: true,
            compare_wav: false,
        }
    }
}

impl BeamformerConfig {
    pub fn validate(&self) -> Result<(), String> {
        for (name, v) in [
            ("fixed_internal_angle_deg", self.fixed_internal_angle_deg),
            (
                "fallback_internal_angle_deg",
                self.fallback_internal_angle_deg,
            ),
            ("direction_smoothing_ms", self.direction_smoothing_ms),
            ("min_wng_db", self.min_wng_db),
            ("sd_low_start_hz", self.sd_low_start_hz),
            ("sd_low_full_hz", self.sd_low_full_hz),
            ("sd_high_full_hz", self.sd_high_full_hz),
            ("sd_high_end_hz", self.sd_high_end_hz),
            ("output_gain_db", self.output_gain_db),
        ] {
            if !v.is_finite() {
                return Err(format!("beamformer.{name} 必须为有限值"));
            }
        }
        if self.direction_smoothing_ms < 0.0 {
            return Err(format!(
                "direction_smoothing_ms 必须 >= 0，当前 {}",
                self.direction_smoothing_ms
            ));
        }
        if !(0.0..=6.0).contains(&self.min_wng_db) {
            return Err(format!(
                "min_wng_db 必须在 0..=6.0，当前 {}",
                self.min_wng_db
            ));
        }
        let ls = self.sd_low_start_hz;
        let lf = self.sd_low_full_hz;
        let hf = self.sd_high_full_hz;
        let he = self.sd_high_end_hz;
        if !(0.0 <= ls && ls <= lf && lf <= hf && hf <= he && he <= 8000.0) {
            return Err(format!(
                "频率带必须满足 0 <= low_start <= low_full <= high_full <= high_end <= 8000，当前 {ls}/{lf}/{hf}/{he}"
            ));
        }
        if !(-24.0..=24.0).contains(&self.output_gain_db) {
            return Err(format!(
                "output_gain_db 建议在 [-24, 24]，当前 {}",
                self.output_gain_db
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct BeamformerStats {
    pub input_frames: u64,
    pub output_frames: u64,
    pub stft_frames: u64,
    pub clipped_samples: u64,
    pub das_fallback_bins: u64,
    pub min_generated_wng_db: f32,
}

pub struct BeamformerRuntime {
    config: BeamformerConfig,
    lut: WeightLut,
    stft: BeamformerStft,
    wav: Option<WavSink>,
    stats: BeamformerStats,
    current_deg: f32,
    gain: f32,
    pcm_scratch: Vec<i16>,
    /// compare_wav 时积压的 mic1 原始样本，与 STFT 输出配对写入双声道文件。
    mic0_buf: Vec<i16>,
    finalized: bool,
}

impl BeamformerRuntime {
    pub fn new(config: BeamformerConfig, out_dir: &str, prefix: &str) -> Result<Self, String> {
        config.validate()?;
        let lut = WeightLut::generate(&config)?;
        let wav = if config.wav {
            let path = format!("{out_dir}/{prefix}_respeaker_bf.wav");
            let channels = if config.compare_wav { 2 } else { 1 };
            Some(WavSink::create(&path, channels, SAMPLE_RATE)?)
        } else {
            None
        };
        let gain = 10f32.powf(config.output_gain_db / 20.0);
        let initial = match config.direction_source {
            BeamformerDirectionSource::Fixed => wrap_360(config.fixed_internal_angle_deg),
            BeamformerDirectionSource::Doa => wrap_360(config.fallback_internal_angle_deg),
        };
        let stats = BeamformerStats {
            das_fallback_bins: lut.das_fallback_bins,
            min_generated_wng_db: lut.min_generated_wng_db,
            ..BeamformerStats::default()
        };
        Ok(Self {
            config,
            lut,
            stft: BeamformerStft::new(),
            wav,
            stats,
            current_deg: initial,
            gain,
            pcm_scratch: Vec::with_capacity(HOP_SIZE),
            mic0_buf: Vec::with_capacity(HOP_SIZE),
            finalized: false,
        })
    }

    pub fn push_block(
        &mut self,
        mics_interleaved: &[i16],
        latest_doa: Option<&DoaResult>,
    ) -> Result<(), String> {
        if self.finalized {
            return Err("BeamformerRuntime 已 finalize".into());
        }
        if !mics_interleaved.len().is_multiple_of(MIC_COUNT) {
            return Err(format!(
                "BF 输入长度 {} 不是 4 的整数倍",
                mics_interleaved.len()
            ));
        }
        for frame in mics_interleaved.chunks_exact(MIC_COUNT) {
            if self.config.compare_wav {
                self.mic0_buf.push(frame[0]);
            }
            let sample = [
                frame[0] as f32 / 32768.0,
                frame[1] as f32 / 32768.0,
                frame[2] as f32 / 32768.0,
                frame[3] as f32 / 32768.0,
            ];
            let hop_ready = self.stft.push_frame(sample)?;
            self.stats.input_frames += 1;
            if hop_ready {
                self.on_hop(latest_doa)?;
            }
        }
        Ok(())
    }

    pub fn finalize(&mut self) -> Result<(), String> {
        if self.finalized {
            return Ok(());
        }
        let target = self.stats.input_frames as usize;
        let mut emitted = self.stats.output_frames as usize;
        // 用当前方向权重冲刷。
        let angle = self.current_deg;
        let weights = self.lut.weights_for(angle);
        let gain = self.gain;
        let compare = self.config.compare_wav;
        let mut clipped = 0u64;
        let mut pcm = std::mem::take(&mut self.pcm_scratch);
        let mut mic0 = std::mem::take(&mut self.mic0_buf);
        {
            let wav = &mut self.wav;
            self.stft
                .flush_zeros(weights, &mut emitted, target, |samples| {
                    if compare {
                        let input = mic0.drain(..samples.len());
                        write_compare_pcm(
                            input,
                            samples,
                            gain,
                            &mut pcm,
                            wav.as_mut(),
                            &mut clipped,
                        )
                    } else {
                        write_pcm(samples, gain, &mut pcm, wav.as_mut(), &mut clipped)
                    }
                })?;
        }
        self.mic0_buf = mic0;
        self.pcm_scratch = pcm;
        self.stats.clipped_samples += clipped;
        self.stats.output_frames = emitted as u64;
        self.stats.stft_frames = self.stft.stft_frames();
        if self.stats.output_frames != self.stats.input_frames {
            return Err(format!(
                "BF 输出帧数 {} != 输入帧数 {}",
                self.stats.output_frames, self.stats.input_frames
            ));
        }
        if let Some(w) = self.wav.as_mut() {
            w.finalize()?;
        }
        self.finalized = true;
        Ok(())
    }

    pub fn stats(&self) -> &BeamformerStats {
        &self.stats
    }

    /// 当前平滑后的内部方向（测试 / 调试）。
    #[cfg(test)]
    pub fn current_internal_deg(&self) -> f32 {
        self.current_deg
    }

    fn on_hop(&mut self, latest_doa: Option<&DoaResult>) -> Result<(), String> {
        let target = select_target_angle(&self.config, latest_doa);
        self.current_deg =
            smooth_direction(self.current_deg, target, self.config.direction_smoothing_ms);
        let weights = self.lut.weights_for(self.current_deg);
        let out = self.stft.process_ready_hop(weights)?;
        let mut clipped = 0u64;
        if self.config.compare_wav {
            let input = self.mic0_buf.drain(..out.len());
            write_compare_pcm(
                input,
                out,
                self.gain,
                &mut self.pcm_scratch,
                self.wav.as_mut(),
                &mut clipped,
            )?;
        } else {
            write_pcm(
                out,
                self.gain,
                &mut self.pcm_scratch,
                self.wav.as_mut(),
                &mut clipped,
            )?;
        }
        self.stats.clipped_samples += clipped;
        self.stats.output_frames += out.len() as u64;
        self.stats.stft_frames = self.stft.stft_frames();
        Ok(())
    }
}

/// 方向选择：Fixed / DOA Tracking|Coasting / fallback。绝不使用对外角。
pub fn select_target_angle(config: &BeamformerConfig, doa: Option<&DoaResult>) -> f32 {
    match config.direction_source {
        BeamformerDirectionSource::Fixed => wrap_360(config.fixed_internal_angle_deg),
        BeamformerDirectionSource::Doa => match doa {
            Some(r) => match r.status {
                TrackStatus::Tracking | TrackStatus::Coasting => r
                    .tracked_internal_deg
                    .map(wrap_360)
                    .unwrap_or_else(|| wrap_360(config.fallback_internal_angle_deg)),
                TrackStatus::Searching => wrap_360(config.fallback_internal_angle_deg),
            },
            None => wrap_360(config.fallback_internal_angle_deg),
        },
    }
}

/// 圆周 EMA；`direction_smoothing_ms == 0` 时直接跳到目标。
pub fn smooth_direction(current: f32, target: f32, smoothing_ms: f32) -> f32 {
    let cur = wrap_360(current);
    let tgt = wrap_360(target);
    if smoothing_ms <= 0.0 {
        return tgt;
    }
    let hop_seconds = HOP_SIZE as f32 / SAMPLE_RATE as f32;
    let tau = smoothing_ms / 1000.0;
    let alpha = 1.0 - (-hop_seconds / tau).exp();
    let delta = circular_delta_deg(tgt, cur);
    wrap_360(cur + alpha * delta)
}

/// 单样本增益 + 16-bit 削波，返回 PCM 值。
fn apply_gain(y: f32, gain: f32, clipped: &mut u64) -> i16 {
    let v = y * gain * 32768.0;
    let r = v.round();
    if r > i16::MAX as f32 {
        *clipped += 1;
        i16::MAX
    } else if r < i16::MIN as f32 {
        *clipped += 1;
        i16::MIN
    } else {
        r as i16
    }
}

fn write_pcm(
    samples: &[f32],
    gain: f32,
    scratch: &mut Vec<i16>,
    wav: Option<&mut WavSink>,
    clipped: &mut u64,
) -> Result<(), String> {
    scratch.clear();
    scratch.reserve(samples.len());
    for &y in samples {
        scratch.push(apply_gain(y, gain, clipped));
    }
    if let Some(w) = wav {
        w.write_samples(scratch)?;
    }
    Ok(())
}

/// 双声道对比写入：`[mic1×gain, bf_out×gain]` 逐帧交错，两声道同一时刻对齐。
fn write_compare_pcm(
    mic0: std::vec::Drain<'_, i16>,
    out: &[f32],
    gain: f32,
    scratch: &mut Vec<i16>,
    wav: Option<&mut WavSink>,
    clipped: &mut u64,
) -> Result<(), String> {
    debug_assert_eq!(mic0.len(), out.len());
    scratch.clear();
    scratch.reserve(out.len() * 2);
    for (input, &y) in mic0.zip(out) {
        let in_f = input as f32 / 32768.0;
        scratch.push(apply_gain(in_f, gain, clipped));
        scratch.push(apply_gain(y, gain, clipped));
    }
    if let Some(w) = wav {
        w.write_samples(scratch)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doa::geometry::RESPEAKER_V2_MICS_M;
    use crate::doa::{FFT_BINS, FRAME_SIZE};
    use std::f32::consts::PI;

    fn temp_dir_prefix(name: &str) -> (String, String) {
        let dir = std::env::temp_dir()
            .join(format!("respeaker_bf_{}_{name}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::create_dir_all(&dir);
        (dir, format!("t_{name}"))
    }

    fn fixed_das_config(angle: f32) -> BeamformerConfig {
        BeamformerConfig {
            enabled: true,
            algorithm: BeamformerAlgorithm::DelaySum,
            direction_source: BeamformerDirectionSource::Fixed,
            fixed_internal_angle_deg: angle,
            fallback_internal_angle_deg: 0.0,
            direction_smoothing_ms: 0.0,
            min_wng_db: 3.0,
            sd_low_start_hz: 350.0,
            sd_low_full_hz: 500.0,
            sd_high_full_hz: 2500.0,
            sd_high_end_hz: 3500.0,
            output_gain_db: 0.0,
            wav: false,
            compare_wav: false,
        }
    }

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

    fn dummy_doa(
        status: TrackStatus,
        tracked_internal: Option<f32>,
        tracked_external: Option<f32>,
    ) -> DoaResult {
        DoaResult {
            timestamp_ms: 64.0,
            raw_internal_deg: tracked_internal,
            tracked_internal_deg: tracked_internal,
            raw_angle_deg: tracked_external,
            tracked_angle_deg: tracked_external,
            confidence: 0.9,
            status,
            observation_used: true,
            peak_score: 1.0,
            second_peak_score: 0.1,
            peak_gap_ratio: 0.9,
            prominence: 0.5,
            mean_msc: 0.5,
            rms_dbfs: -20.0,
        }
    }

    #[test]
    fn fixed_das_preserves_target_plane_wave() {
        let theta = 45.0f32;
        let signal = synth_plane_wave(theta, 0.5);
        let (dir, prefix) = temp_dir_prefix("das");
        let mut correct_config = fixed_das_config(theta);
        correct_config.wav = true;
        let mut rt = BeamformerRuntime::new(correct_config, &dir, &prefix).unwrap();
        rt.push_block(&signal, None).unwrap();
        rt.finalize().unwrap();
        assert_eq!(rt.stats().output_frames, rt.stats().input_frames);
        assert!(rt.stats().input_frames > 0);

        let mut wrong_config = fixed_das_config(theta + 180.0);
        wrong_config.wav = true;
        let mut rt_wrong = BeamformerRuntime::new(wrong_config, &dir, "wrong").unwrap();
        rt_wrong.push_block(&signal, None).unwrap();
        rt_wrong.finalize().unwrap();
        assert_eq!(
            rt_wrong.stats().output_frames,
            rt_wrong.stats().input_frames
        );

        let read = |path: &str| {
            hound::WavReader::open(path)
                .unwrap()
                .samples::<i16>()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
        };
        let correct = read(&format!("{dir}/{prefix}_respeaker_bf.wav"));
        let wrong = read(&format!("{dir}/wrong_respeaker_bf.wav"));
        let rms = |samples: &[i16]| {
            let middle = &samples[HOP_SIZE..samples.len() - HOP_SIZE];
            (middle
                .iter()
                .map(|&sample| (sample as f64).powi(2))
                .sum::<f64>()
                / middle.len() as f64)
                .sqrt()
        };
        let correct_rms = rms(&correct);
        let wrong_rms = rms(&wrong);
        assert!(
            correct_rms > wrong_rms * 1.5,
            "目标方向 RMS={correct_rms}，反向 RMS={wrong_rms}"
        );
    }

    #[test]
    fn fixed_bf_rejects_wrong_input_length() {
        let (dir, prefix) = temp_dir_prefix("badlen");
        let mut rt = BeamformerRuntime::new(fixed_das_config(0.0), &dir, &prefix).unwrap();
        assert!(rt.push_block(&[0i16; 3], None).is_err());
        assert!(rt.push_block(&[0i16; 5], None).is_err());
    }

    #[test]
    fn doa_source_uses_tracking_and_coasting() {
        let cfg = BeamformerConfig {
            direction_source: BeamformerDirectionSource::Doa,
            direction_smoothing_ms: 0.0,
            fallback_internal_angle_deg: 10.0,
            wav: false,
            algorithm: BeamformerAlgorithm::DelaySum,
            ..Default::default()
        };

        let tracking = dummy_doa(TrackStatus::Tracking, Some(123.0), Some(999.0));
        assert!((select_target_angle(&cfg, Some(&tracking)) - 123.0).abs() < 1e-5);

        let coasting = dummy_doa(TrackStatus::Coasting, Some(200.0), Some(1.0));
        assert!((select_target_angle(&cfg, Some(&coasting)) - 200.0).abs() < 1e-5);

        let (dir, prefix) = temp_dir_prefix("track");
        let mut rt = BeamformerRuntime::new(cfg, &dir, &prefix).unwrap();
        let block = vec![0i16; HOP_SIZE * MIC_COUNT];
        rt.push_block(&block, Some(&tracking)).unwrap();
        assert!((rt.current_internal_deg() - 123.0).abs() < 1e-3);
        rt.push_block(&block, Some(&coasting)).unwrap();
        assert!((rt.current_internal_deg() - 200.0).abs() < 1e-3);
    }

    #[test]
    fn doa_source_falls_back_while_searching() {
        let cfg = BeamformerConfig {
            direction_source: BeamformerDirectionSource::Doa,
            direction_smoothing_ms: 0.0,
            fallback_internal_angle_deg: 33.0,
            wav: false,
            algorithm: BeamformerAlgorithm::DelaySum,
            ..Default::default()
        };

        let searching = dummy_doa(TrackStatus::Searching, Some(90.0), Some(90.0));
        assert!((select_target_angle(&cfg, Some(&searching)) - 33.0).abs() < 1e-5);
        assert!((select_target_angle(&cfg, None) - 33.0).abs() < 1e-5);
    }

    #[test]
    fn beamformer_uses_fallback_before_first_doa_result() {
        let cfg = BeamformerConfig {
            direction_source: BeamformerDirectionSource::Doa,
            direction_smoothing_ms: 0.0,
            fallback_internal_angle_deg: 77.0,
            wav: false,
            algorithm: BeamformerAlgorithm::DelaySum,
            ..Default::default()
        };
        let (dir, prefix) = temp_dir_prefix("fallback");
        let mut rt = BeamformerRuntime::new(cfg, &dir, &prefix).unwrap();
        let block = vec![0i16; HOP_SIZE * MIC_COUNT];
        rt.push_block(&block, None).unwrap();
        assert!((rt.current_internal_deg() - 77.0).abs() < 1e-3);
    }

    #[test]
    fn beamformer_never_uses_viewer_or_external_angle() {
        let cfg = BeamformerConfig {
            direction_source: BeamformerDirectionSource::Doa,
            direction_smoothing_ms: 0.0,
            fallback_internal_angle_deg: 0.0,
            ..Default::default()
        };
        // 内部 40°，对外被 offset 转成完全不同的角。
        let doa = dummy_doa(TrackStatus::Tracking, Some(40.0), Some(220.0));
        let target = select_target_angle(&cfg, Some(&doa));
        assert!((target - 40.0).abs() < 1e-5);
        assert!((target - 220.0).abs() > 1.0);
    }

    #[test]
    fn direction_smoothing_crosses_zero_degrees_correctly() {
        // 从 350° 向 10° 平滑，应跨越 0° 而不是走长弧。
        let mut cur = 350.0f32;
        for _ in 0..50 {
            cur = smooth_direction(cur, 10.0, 64.0);
        }
        let err = crate::doa::circular_distance_deg(cur, 10.0);
        assert!(err < 1.0, "cur={cur}");
        // 中间过程不应跑到 180° 附近。
        let mid = smooth_direction(350.0, 10.0, 64.0);
        assert!(
            !(20.0..=340.0).contains(&mid),
            "跨越 0° 的单步应仍靠近边界，mid={mid}"
        );
    }

    #[test]
    fn beamformer_output_pcm_saturates_without_wraparound() {
        let mut cfg = fixed_das_config(0.0);
        cfg.output_gain_db = 12.0;
        cfg.wav = true;
        let (dir, prefix) = temp_dir_prefix("clip");
        let mut rt = BeamformerRuntime::new(cfg, &dir, &prefix).unwrap();
        // 满幅同相输入 → 高增益后削波。
        let block: Vec<i16> = (0..HOP_SIZE * 8)
            .flat_map(|_| [i16::MAX, i16::MAX, i16::MAX, i16::MAX])
            .collect();
        rt.push_block(&block, None).unwrap();
        rt.finalize().unwrap();
        assert!(rt.stats().clipped_samples > 0);

        let path = format!("{dir}/{prefix}_respeaker_bf.wav");
        let mut reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.spec().sample_rate, 16_000);
        assert!(reader.samples::<i16>().all(|s| s.is_ok()));
        drop(reader);
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[16..20], &16u32.to_le_bytes());
        assert_eq!(&bytes[20..22], &[1, 0]);
        assert_eq!(&bytes[36..40], b"data");
    }

    #[test]
    fn compare_wav_writes_stereo_mic1_and_bf_output() {
        let (dir, prefix) = temp_dir_prefix("compare");
        let mut cfg = fixed_das_config(0.0);
        cfg.wav = true;
        cfg.compare_wav = true;
        cfg.output_gain_db = 0.0; // 增益 1×，输入声道可精确断言
        let mut rt = BeamformerRuntime::new(cfg, &dir, &prefix).unwrap();
        // 确定性输入：每帧 mic1 = 帧号，其余通道为 0。
        let frames = HOP_SIZE * 3;
        let mut block = Vec::with_capacity(frames * MIC_COUNT);
        for n in 0..frames {
            block.push(n as i16);
            block.extend_from_slice(&[0, 0, 0]);
        }
        rt.push_block(&block, None).unwrap();
        rt.finalize().unwrap();
        assert_eq!(rt.stats().output_frames, rt.stats().input_frames);

        let path = format!("{dir}/{prefix}_respeaker_bf.wav");
        let mut reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().sample_rate, 16_000);
        let samples: Vec<i16> = reader.samples::<i16>().map(Result::unwrap).collect();
        assert_eq!(samples.len(), frames * 2);
        // 左声道 = mic1 原始样本（增益 1×，逐帧配对）；右声道 = BF 输出，不应全零。
        let mut output_nonzero = false;
        for (k, pair) in samples.chunks_exact(2).enumerate() {
            assert_eq!(pair[0], k as i16, "输入声道第 {k} 帧");
            output_nonzero |= pair[1] != 0;
        }
        assert!(output_nonzero, "输出声道应有非零样本");
    }

    #[test]
    fn compare_wav_applies_gain_to_both_channels() {
        let (dir, prefix) = temp_dir_prefix("compare_gain");
        let mut cfg = fixed_das_config(0.0);
        cfg.wav = true;
        cfg.compare_wav = true;
        cfg.output_gain_db = 20.0; // 约 10 倍，输入声道应被放大并削波到满幅
        let mut rt = BeamformerRuntime::new(cfg, &dir, &prefix).unwrap();
        let block: Vec<i16> = (0..HOP_SIZE * 2)
            .flat_map(|_| [i16::MAX, 0, 0, 0])
            .collect();
        rt.push_block(&block, None).unwrap();
        rt.finalize().unwrap();
        assert!(rt.stats().clipped_samples > 0, "增益后应有削波统计");

        let path = format!("{dir}/{prefix}_respeaker_bf.wav");
        let mut reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 2);
        let samples: Vec<i16> = reader.samples::<i16>().map(Result::unwrap).collect();
        for pair in samples.chunks_exact(2) {
            // 满幅输入 × 10 倍 → 输入声道应削波到 i16::MAX。
            assert_eq!(pair[0], i16::MAX, "输入声道应满幅削波");
        }
    }

    #[test]
    fn config_validate_frequency_and_wng() {
        let mut cfg = BeamformerConfig::default();
        assert!(cfg.validate().is_ok());
        cfg.min_wng_db = 7.0;
        assert!(cfg.validate().is_err());
        cfg.min_wng_db = 3.0;
        cfg.sd_low_full_hz = 100.0; // < low_start
        assert!(cfg.validate().is_err());
        cfg.sd_low_full_hz = 500.0;
        // 高增益配置（如 20 dB）必须在允许范围内，超出后拒绝。
        cfg.output_gain_db = 20.0;
        assert!(cfg.validate().is_ok());
        cfg.output_gain_db = 25.0;
        assert!(cfg.validate().is_err());
        let _ = FFT_BINS;
    }
}
