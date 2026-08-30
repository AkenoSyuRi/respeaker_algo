mod config;
mod core;

use crate::drc::config::DrcConfig;
use crate::drc::core::DrcProcessor;

pub struct TssDrc {
    inner: DrcProcessor,
}

impl TssDrc {
    pub fn pipeline(sample_rate: u32) -> Result<Self, String> {
        if sample_rate == 0 {
            return Err("sample_rate must be positive".into());
        }
        Ok(Self {
            inner: DrcProcessor::new(sample_rate, &DrcConfig::pipeline()),
        })
    }

    pub fn latency_samples(&self) -> usize {
        self.inner.latency_samples()
    }

    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.inner.reset();
    }

    pub fn process_in_place(&mut self, samples: &mut [f32]) {
        for sample in samples {
            *sample = self.inner.process_sample(*sample);
        }
    }

    pub fn flush(&mut self, dest: &mut [f32]) {
        dest.fill(0.0);
        self.process_in_place(dest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    const GOLDEN: &str = include_str!("testdata/pipeline_16k_mono.json");
    const ATOL: f32 = 5.0e-7;

    #[derive(Deserialize)]
    struct Golden {
        source_commit: String,
        sample_rate: u32,
        predelay_s: f32,
        mode: String,
        chunk_sizes: Vec<usize>,
        input: Vec<f32>,
        expected: Vec<f32>,
    }

    fn load_golden() -> Golden {
        serde_json::from_str(GOLDEN).unwrap()
    }

    fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max)
    }

    #[test]
    fn pipeline_rejects_zero_sample_rate() {
        assert!(TssDrc::pipeline(0).is_err());
    }

    #[test]
    fn latency_samples_is_32_at_16k() {
        let drc = TssDrc::pipeline(16_000).unwrap();
        assert_eq!(drc.latency_samples(), 32);
    }

    #[test]
    fn reset_repeats_first_block() {
        let mut drc = TssDrc::pipeline(16_000).unwrap();
        let input: Vec<f32> = (0..512).map(|i| 0.1 * ((i as f32 * 0.17).sin())).collect();
        let mut first = input.clone();
        drc.process_in_place(&mut first);
        drc.process_in_place(&mut input.clone());
        drc.reset();
        let mut repeated = input;
        drc.process_in_place(&mut repeated);
        assert_eq!(first, repeated);
    }

    #[test]
    fn look_ahead_delay_peaks_near_32() {
        let mut drc = TssDrc::pipeline(16_000).unwrap();
        let mut x = vec![0.0f32; 128];
        x[0] = 0.8;
        drc.process_in_place(&mut x);
        let peak = x
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        assert_eq!(peak, 32);
    }

    #[test]
    fn gate_attenuates_silence() {
        let mut drc = TssDrc::pipeline(16_000).unwrap();
        let mut quiet = vec![1.0e-5f32; 2048];
        drc.process_in_place(&mut quiet);
        let floor = 10.0f32.powf(-80.0 / 20.0);
        let tail = &quiet[1024..];
        let peak = tail.iter().copied().map(f32::abs).fold(0.0f32, f32::max);
        assert!(peak < floor * 20.0, "peak={peak}");
    }

    #[test]
    fn compressor_reduces_loud_tone() {
        let mut drc = TssDrc::pipeline(16_000).unwrap();
        let mut tone: Vec<f32> = (0..2048)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16_000.0).sin())
            .collect();
        drc.process_in_place(&mut tone);
        let steady = &tone[512..];
        let peak = steady.iter().copied().map(f32::abs).fold(0.0f32, f32::max);
        assert!(peak < 0.95, "peak={peak}");
    }

    #[test]
    fn limiter_caps_ceiling() {
        let mut drc = TssDrc::pipeline(16_000).unwrap();
        let mut loud = vec![1.0f32; 1024];
        drc.process_in_place(&mut loud);
        let ceiling = 10.0f32.powf(-1.0 / 20.0);
        let peak = loud.iter().copied().map(f32::abs).fold(0.0f32, f32::max);
        assert!(peak <= ceiling + 2.0e-3, "peak={peak} ceiling={ceiling}");
    }

    #[test]
    fn chunk_consistency_256() {
        let input: Vec<f32> = (0..1024).map(|i| 0.2 * ((i as f32 * 0.31).sin())).collect();
        let mut whole = input.clone();
        TssDrc::pipeline(16_000)
            .unwrap()
            .process_in_place(&mut whole);
        let mut split = input;
        let mut drc = TssDrc::pipeline(16_000).unwrap();
        for chunk in split.chunks_mut(256) {
            drc.process_in_place(chunk);
        }
        assert_eq!(whole, split);
    }

    #[test]
    fn flush_equals_trailing_zeros() {
        let input: Vec<f32> = (0..256).map(|i| 0.4 * ((i as f32 * 0.11).sin())).collect();
        assert!(input[224..].iter().any(|v| v.abs() > 0.1));

        let mut a = TssDrc::pipeline(16_000).unwrap();
        let mut head = input.clone();
        a.process_in_place(&mut head);
        let mut tail = [0.0f32; 32];
        a.flush(&mut tail);
        assert!(
            tail.iter().any(|v| v.abs() > 1.0e-4),
            "flush tail must carry delayed signal"
        );

        let mut b = TssDrc::pipeline(16_000).unwrap();
        let mut padded = input;
        padded.extend(std::iter::repeat_n(0.0, 32));
        b.process_in_place(&mut padded);

        let mut actual = head;
        actual.extend_from_slice(&tail);
        assert_eq!(actual, padded);
    }

    #[test]
    fn nonfinite_matches_zeros() {
        let mut dirty = vec![0.2f32; 64];
        dirty[3] = f32::NAN;
        dirty[7] = f32::INFINITY;
        dirty[11] = f32::NEG_INFINITY;
        let zeros = {
            let mut z = dirty.clone();
            z[3] = 0.0;
            z[7] = 0.0;
            z[11] = 0.0;
            z
        };
        let follow: Vec<f32> = (0..64).map(|i| 0.15 * ((i as f32 * 0.2).sin())).collect();

        let mut a = TssDrc::pipeline(16_000).unwrap();
        let mut b = TssDrc::pipeline(16_000).unwrap();
        let mut dirty_out = dirty;
        let mut zero_out = zeros;
        a.process_in_place(&mut dirty_out);
        b.process_in_place(&mut zero_out);
        assert_eq!(dirty_out, zero_out);
        assert!(dirty_out.iter().all(|v| v.is_finite()));

        let mut follow_a = follow.clone();
        let mut follow_b = follow;
        a.process_in_place(&mut follow_a);
        b.process_in_place(&mut follow_b);
        assert_eq!(follow_a, follow_b);
        assert!(follow_a.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn long_silence_stays_finite() {
        let tone: Vec<f32> = (0..512)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16_000.0).sin())
            .collect();
        let zeros = vec![0.0f32; 16_000];
        let after: Vec<f32> = (0..256).map(|i| 0.2 * ((i as f32 * 0.13).sin())).collect();

        let mut seq = tone.clone();
        seq.extend_from_slice(&zeros);
        seq.extend_from_slice(&after);
        let mut once = seq.clone();
        TssDrc::pipeline(16_000)
            .unwrap()
            .process_in_place(&mut once);
        assert!(once.iter().all(|v| v.is_finite()));

        let mut drc = TssDrc::pipeline(16_000).unwrap();
        let mut t = tone;
        drc.process_in_place(&mut t);
        let mut z = zeros;
        drc.process_in_place(&mut z);
        let mut a = after;
        drc.process_in_place(&mut a);
        assert!(
            t.iter()
                .chain(z.iter())
                .chain(a.iter())
                .all(|v| v.is_finite())
        );

        let mut split = t;
        split.extend_from_slice(&z);
        split.extend_from_slice(&a);
        assert_eq!(once, split);
    }

    #[test]
    fn golden_matches_source_within_5e7() {
        let golden = load_golden();
        assert_eq!(
            golden.source_commit,
            "c61c5d6b904fa56055b06320810893e851c6e938"
        );
        assert_eq!(golden.sample_rate, 16_000);
        assert_eq!(golden.predelay_s, 0.002);
        assert_eq!(golden.mode, "independent");
        assert_eq!(
            golden.chunk_sizes,
            vec![1, 15, 16, 17, 80, 161, 256, 3, 511, 988]
        );
        assert_eq!(golden.input.len(), 2048);
        assert_eq!(golden.expected.len(), 2048);
        assert_eq!(golden.chunk_sizes.iter().sum::<usize>(), 2048);

        let mut whole = golden.input.clone();
        TssDrc::pipeline(16_000)
            .unwrap()
            .process_in_place(&mut whole);
        assert!(
            max_abs_diff(&whole, &golden.expected) <= ATOL,
            "whole max abs {}",
            max_abs_diff(&whole, &golden.expected)
        );

        let mut split = golden.input;
        let mut drc = TssDrc::pipeline(16_000).unwrap();
        let mut offset = 0;
        for size in golden.chunk_sizes {
            drc.process_in_place(&mut split[offset..offset + size]);
            offset += size;
        }
        assert!(
            max_abs_diff(&split, &golden.expected) <= ATOL,
            "chunked max abs {}",
            max_abs_diff(&split, &golden.expected)
        );
    }
}
