//! 4 通道流式 STFT / WOLA（periodic sqrt-Hann，512/256）。

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::sync::Arc;

use crate::doa::{FFT_BINS, FRAME_SIZE, HOP_SIZE, MIC_COUNT};

/// periodic sqrt-Hann：`hann[n]=0.5-0.5*cos(2πn/N)`，`window=sqrt(hann)`。
pub fn sqrt_hann_periodic(n: usize) -> [f32; FRAME_SIZE] {
    assert_eq!(n, FRAME_SIZE);
    std::array::from_fn(|i| {
        let hann = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FRAME_SIZE as f32).cos();
        hann.max(0.0).sqrt()
    })
}

/// 单通道 WOLA bypass：analysis + synthesis，不做 BF。用于重构测试。
#[cfg(test)]
pub struct WolaBypass {
    fft_fwd: Arc<dyn RealToComplex<f32>>,
    fft_inv: Arc<dyn ComplexToReal<f32>>,
    scratch_fwd: Vec<Complex32>,
    scratch_inv: Vec<Complex32>,
    window: [f32; FRAME_SIZE],
    time_buf: [f32; FRAME_SIZE],
    ola: [f32; FRAME_SIZE],
    spectrum: Vec<Complex32>,
    fft_time: Vec<f32>,
    hop_accum: Vec<f32>,
    input_frames: u64,
    discard_remaining: usize,
    output: Vec<f32>,
    finalized: bool,
}

#[cfg(test)]
impl WolaBypass {
    pub fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft_fwd = planner.plan_fft_forward(FRAME_SIZE);
        let fft_inv = planner.plan_fft_inverse(FRAME_SIZE);
        let scratch_fwd = fft_fwd.make_scratch_vec();
        let scratch_inv = fft_inv.make_scratch_vec();
        let spectrum = fft_fwd.make_output_vec();
        let fft_time = fft_inv.make_output_vec();
        // time_buf 初始为全零，等价于前缀预填 256 个零；首个输出 hop 为
        // 算法延迟，discard_remaining 丢弃后使 sample 0 对齐。
        Self {
            fft_fwd,
            fft_inv,
            scratch_fwd,
            scratch_inv,
            window: sqrt_hann_periodic(FRAME_SIZE),
            time_buf: [0.0; FRAME_SIZE],
            ola: [0.0; FRAME_SIZE],
            spectrum,
            fft_time,
            hop_accum: Vec::with_capacity(HOP_SIZE),
            input_frames: 0,
            discard_remaining: HOP_SIZE,
            output: Vec::new(),
            finalized: false,
        }
    }

    pub fn push_samples(&mut self, samples: &[f32]) -> Result<(), String> {
        if self.finalized {
            return Err("WolaBypass 已 finalize".into());
        }
        for &s in samples {
            self.hop_accum.push(s);
            self.input_frames += 1;
            if self.hop_accum.len() == HOP_SIZE {
                let hop: [f32; HOP_SIZE] = std::array::from_fn(|i| self.hop_accum[i]);
                self.hop_accum.clear();
                self.process_hop(&hop);
            }
        }
        Ok(())
    }

    pub fn finalize(&mut self) -> Result<&[f32], String> {
        if self.finalized {
            return Ok(&self.output);
        }
        if !self.hop_accum.is_empty() {
            while self.hop_accum.len() < HOP_SIZE {
                self.hop_accum.push(0.0);
            }
            let hop: [f32; HOP_SIZE] = std::array::from_fn(|i| self.hop_accum[i]);
            self.hop_accum.clear();
            self.process_hop(&hop);
        }
        // 额外零 hop，冲出最后真实样本的后半窗贡献。
        self.process_hop(&[0.0f32; HOP_SIZE]);
        self.output.truncate(self.input_frames as usize);
        self.finalized = true;
        Ok(&self.output)
    }

    fn process_hop(&mut self, hop: &[f32; HOP_SIZE]) {
        self.time_buf.copy_within(HOP_SIZE..FRAME_SIZE, 0);
        self.time_buf[HOP_SIZE..].copy_from_slice(hop);

        for i in 0..FRAME_SIZE {
            self.fft_time[i] = self.time_buf[i] * self.window[i];
        }
        self.fft_fwd
            .process_with_scratch(
                &mut self.fft_time,
                &mut self.spectrum,
                &mut self.scratch_fwd,
            )
            .expect("forward FFT");

        self.spectrum[0].im = 0.0;
        let nyq = FFT_BINS - 1;
        self.spectrum[nyq] = Complex32::new(0.0, 0.0);

        self.fft_inv
            .process_with_scratch(
                &mut self.spectrum,
                &mut self.fft_time,
                &mut self.scratch_inv,
            )
            .expect("inverse FFT");
        let scale = 1.0 / FRAME_SIZE as f32;
        for i in 0..FRAME_SIZE {
            self.ola[i] += self.fft_time[i] * scale * self.window[i];
        }
        for i in 0..HOP_SIZE {
            let y = self.ola[i];
            if self.discard_remaining > 0 {
                self.discard_remaining -= 1;
            } else {
                self.output.push(y);
            }
        }
        self.ola.copy_within(HOP_SIZE..FRAME_SIZE, 0);
        for v in &mut self.ola[HOP_SIZE..] {
            *v = 0.0;
        }
    }
}

#[cfg(test)]
impl Default for WolaBypass {
    fn default() -> Self {
        Self::new()
    }
}

/// 4 通道 BF STFT：每 hop 用 `FFT_BINS * MIC_COUNT` 权重做 `y = w^H x`。
pub struct BeamformerStft {
    fft_fwd: Arc<dyn RealToComplex<f32>>,
    fft_inv: Arc<dyn ComplexToReal<f32>>,
    scratch_fwd: Vec<Complex32>,
    scratch_inv: Vec<Complex32>,
    window: [f32; FRAME_SIZE],
    time_buf: [[f32; FRAME_SIZE]; MIC_COUNT],
    ola: [f32; FRAME_SIZE],
    spectrum: [[Complex32; FFT_BINS]; MIC_COUNT],
    y_spec: Vec<Complex32>,
    fft_time: Vec<f32>,
    hop_ch: [[f32; HOP_SIZE]; MIC_COUNT],
    hop_fill: usize,
    input_frames: u64,
    discard_remaining: usize,
    /// 最近一次 `process_ready_hop` 产生的输出（0 或 HOP_SIZE）。
    last_hop_out: Vec<f32>,
    stft_frames: u64,
    finalized: bool,
}

impl BeamformerStft {
    pub fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft_fwd = planner.plan_fft_forward(FRAME_SIZE);
        let fft_inv = planner.plan_fft_inverse(FRAME_SIZE);
        let scratch_fwd = fft_fwd.make_scratch_vec();
        let scratch_inv = fft_inv.make_scratch_vec();
        let y_spec = fft_fwd.make_output_vec();
        let fft_time = fft_inv.make_output_vec();
        // time_buf 初始全零 = 前缀预填 256 零；丢弃首个输出 hop 以对齐 sample 0。
        Self {
            fft_fwd,
            fft_inv,
            scratch_fwd,
            scratch_inv,
            window: sqrt_hann_periodic(FRAME_SIZE),
            time_buf: [[0.0; FRAME_SIZE]; MIC_COUNT],
            ola: [0.0; FRAME_SIZE],
            spectrum: [[Complex32::new(0.0, 0.0); FFT_BINS]; MIC_COUNT],
            y_spec,
            fft_time,
            hop_ch: [[0.0; HOP_SIZE]; MIC_COUNT],
            hop_fill: 0,
            input_frames: 0,
            discard_remaining: HOP_SIZE,
            last_hop_out: Vec::with_capacity(HOP_SIZE),
            stft_frames: 0,
            finalized: false,
        }
    }

    pub fn stft_frames(&self) -> u64 {
        self.stft_frames
    }

    /// 推入一帧 4ch 归一化样本。凑满 hop 时返回 true。
    pub fn push_frame(&mut self, frame: [f32; MIC_COUNT]) -> Result<bool, String> {
        if self.finalized {
            return Err("BeamformerStft 已 finalize".into());
        }
        for (ch, &sample) in frame.iter().enumerate() {
            self.hop_ch[ch][self.hop_fill] = sample;
        }
        self.hop_fill += 1;
        self.input_frames += 1;
        if self.hop_fill == HOP_SIZE {
            self.hop_fill = 0;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// 对当前已凑满的 hop 做 STFT/BF/OLA，返回新输出（可能为空，若仍在丢弃预填）。
    pub fn process_ready_hop(&mut self, weights: &[Complex32]) -> Result<&[f32], String> {
        if weights.len() < FFT_BINS * MIC_COUNT {
            return Err("BF 权重长度不足".into());
        }
        for ch in 0..MIC_COUNT {
            self.time_buf[ch].copy_within(HOP_SIZE..FRAME_SIZE, 0);
            self.time_buf[ch][HOP_SIZE..].copy_from_slice(&self.hop_ch[ch]);
        }

        for ch in 0..MIC_COUNT {
            for i in 0..FRAME_SIZE {
                self.fft_time[i] = self.time_buf[ch][i] * self.window[i];
            }
            self.fft_fwd
                .process_with_scratch(&mut self.fft_time, &mut self.y_spec, &mut self.scratch_fwd)
                .map_err(|e| format!("forward FFT 失败: {e:?}"))?;
            self.spectrum[ch][..FFT_BINS].copy_from_slice(&self.y_spec[..FFT_BINS]);
        }

        let nyq = FFT_BINS - 1;
        for bin in 0..FFT_BINS {
            let mut y = Complex32::new(0.0, 0.0);
            let base = bin * MIC_COUNT;
            for ch in 0..MIC_COUNT {
                y += weights[base + ch].conj() * self.spectrum[ch][bin];
            }
            self.y_spec[bin] = y;
        }
        self.y_spec[0].im = 0.0;
        self.y_spec[nyq] = Complex32::new(0.0, 0.0);

        self.fft_inv
            .process_with_scratch(&mut self.y_spec, &mut self.fft_time, &mut self.scratch_inv)
            .map_err(|e| format!("inverse FFT 失败: {e:?}"))?;
        let scale = 1.0 / FRAME_SIZE as f32;
        for i in 0..FRAME_SIZE {
            self.ola[i] += self.fft_time[i] * scale * self.window[i];
        }

        self.last_hop_out.clear();
        for i in 0..HOP_SIZE {
            let y = self.ola[i];
            if self.discard_remaining > 0 {
                self.discard_remaining -= 1;
            } else {
                self.last_hop_out.push(y);
            }
        }
        self.ola.copy_within(HOP_SIZE..FRAME_SIZE, 0);
        for v in &mut self.ola[HOP_SIZE..] {
            *v = 0.0;
        }
        self.stft_frames += 1;
        Ok(&self.last_hop_out)
    }

    /// 零填充 partial hop 并额外冲刷，直到 `emitted` 达到 `input_frames`。
    pub fn flush_zeros(
        &mut self,
        weights: &[Complex32],
        emitted: &mut usize,
        target: usize,
        mut on_samples: impl FnMut(&[f32]) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.finalized {
            return Ok(());
        }
        if self.hop_fill > 0 {
            for ch in 0..MIC_COUNT {
                for i in self.hop_fill..HOP_SIZE {
                    self.hop_ch[ch][i] = 0.0;
                }
            }
            self.hop_fill = 0;
            let out = self.process_ready_hop(weights)?;
            let need = target.saturating_sub(*emitted);
            let take = need.min(out.len());
            if take > 0 {
                on_samples(&out[..take])?;
                *emitted += take;
            }
        }
        while *emitted < target {
            for ch in 0..MIC_COUNT {
                self.hop_ch[ch] = [0.0; HOP_SIZE];
            }
            let out = self.process_ready_hop(weights)?;
            if out.is_empty() {
                // 仍在丢弃预填延迟；继续冲刷。
                continue;
            }
            let need = target - *emitted;
            let take = need.min(out.len());
            on_samples(&out[..take])?;
            *emitted += take;
        }
        self.finalized = true;
        Ok(())
    }
}

impl Default for BeamformerStft {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqrt_hann_cola_square_sum() {
        let w = sqrt_hann_periodic(FRAME_SIZE);
        for n in 0..HOP_SIZE {
            let s = w[n] * w[n] + w[n + HOP_SIZE] * w[n + HOP_SIZE];
            assert!((s - 1.0).abs() < 1e-5, "COLA square sum at {n}: {s}");
        }
    }

    fn synth_tone(seconds: f32, freq: f32) -> Vec<f32> {
        let n = (seconds * 16_000.0) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                0.5 * (std::f32::consts::TAU * freq * t).sin()
            })
            .collect()
    }

    #[test]
    fn wola_bypass_roundtrip() {
        let x = synth_tone(0.25, 440.0);
        let mut wola = WolaBypass::new();
        wola.push_samples(&x).unwrap();
        let y = wola.finalize().unwrap();
        assert_eq!(y.len(), x.len());
        let mut max_err = 0.0f32;
        for (a, b) in x.iter().zip(y.iter()) {
            max_err = max_err.max((a - b).abs());
        }
        assert!(max_err < 2e-3, "max_err={max_err}");
    }

    #[test]
    fn wola_chunk_size_independent() {
        let x = synth_tone(0.3, 1000.0);
        let mut a = WolaBypass::new();
        a.push_samples(&x).unwrap();
        let ya = a.finalize().unwrap().to_vec();

        let mut b = WolaBypass::new();
        let mut i = 0;
        for chunk in [1usize, 3, 17, 64, 256, 511, 100].into_iter().cycle() {
            if i >= x.len() {
                break;
            }
            let end = (i + chunk).min(x.len());
            b.push_samples(&x[i..end]).unwrap();
            i = end;
        }
        let yb = b.finalize().unwrap().to_vec();
        assert_eq!(ya.len(), yb.len());
        for (u, v) in ya.iter().zip(yb.iter()) {
            assert!((u - v).abs() < 1e-6);
        }
    }

    #[test]
    fn wola_finalize_preserves_exact_length() {
        for n in [0usize, 1, 100, 256, 257, 511, 512, 1000, 4096] {
            let x: Vec<f32> = (0..n).map(|i| (i as f32 * 0.001).sin()).collect();
            let mut wola = WolaBypass::new();
            wola.push_samples(&x).unwrap();
            let y = wola.finalize().unwrap();
            assert_eq!(y.len(), n, "n={n}");
        }
    }
}
