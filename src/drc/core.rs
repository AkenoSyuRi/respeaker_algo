//! Sample-accurate port of tss_native DrcProcessor (sndfilter compressor, MIT).

#![allow(
    clippy::excessive_precision,
    clippy::approx_constant,
    clippy::too_many_arguments
)] // literals and signature pinned to tss_native_drc.cpp

use crate::drc::config::DrcConfig;

const ZERO: f32 = 0.0;
const ONE: f32 = 1.0;
const TINY: f32 = 1.0e-20;
const PI_OVER_TWO: f32 = 1.57079632679489661923;
const TWO_OVER_PI: f32 = 0.63661977236758134308;
const SPACING_DB: f32 = 5.0;
const MAX_DELAY: usize = 1024;
const SPU: i32 = 16;

fn numpy_exp(value: f32) -> f32 {
    const MAXIMUM_INPUT: f32 = 88.72283935546875;
    const MINIMUM_INPUT: f32 = -103.97208404541015625;
    // 0x1.800000p+23 — NumPy float32 exp rounding magic (2^23 + 2^22).
    const ROUND_MAGIC: f32 = 12582912.0;
    const LOG2_E: f32 = 1.4426950408889634074;
    const LOG_E2_HIGH: f32 = -6.93145752e-1;
    const LOG_E2_LOW: f32 = -1.42860677e-6;
    const P0: f32 = 9.999999999980870924916e-01;
    const P1: f32 = 7.257664613233124478488e-01;
    const P2: f32 = 2.473615434895520810817e-01;
    const P3: f32 = 5.114512081637298353406e-02;
    const P4: f32 = 6.757896990527504603057e-03;
    const P5: f32 = 5.082762527590693718096e-04;
    const Q0: f32 = 1.000000000000000000000e+00;
    const Q1: f32 = -2.742335390411667452936e-01;
    const Q2: f32 = 2.159509375685829852307e-02;

    if value.is_nan() {
        return value;
    }
    if value >= MAXIMUM_INPUT {
        return f32::INFINITY;
    }
    if value <= MINIMUM_INPUT {
        return ZERO;
    }

    let mut quadrant = value * LOG2_E;
    quadrant = (quadrant + ROUND_MAGIC) - ROUND_MAGIC;
    let mut reduced = quadrant.mul_add(LOG_E2_HIGH, value);
    reduced = quadrant.mul_add(LOG_E2_LOW, reduced);
    let mut numerator = P5.mul_add(reduced, P4);
    numerator = numerator.mul_add(reduced, P3);
    numerator = numerator.mul_add(reduced, P2);
    numerator = numerator.mul_add(reduced, P1);
    numerator = numerator.mul_add(reduced, P0);
    let mut denominator = Q2.mul_add(reduced, Q1);
    denominator = denominator.mul_add(reduced, Q0);
    (numerator / denominator) * 2f32.powi(quadrant as i32)
}

fn clampf(value: f32, minimum: f32, maximum: f32) -> f32 {
    value.max(minimum).min(maximum)
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

fn finite_input(value: f32) -> f32 {
    finite_or(value, ZERO)
}

fn db2lin(db: f32) -> f32 {
    10.0f32.powf(0.05 * db)
}

fn lin2db(linear: f32) -> f32 {
    20.0 * linear.max(TINY).log10()
}

fn time_to_coeff(seconds: f32, sample_rate: u32) -> f32 {
    if seconds <= ZERO || sample_rate == 0 {
        return ONE;
    }
    ONE - numpy_exp(-ONE / (seconds * sample_rate as f32))
}

fn knee_curve(x: f32, k: f32, linear_threshold: f32) -> f32 {
    linear_threshold + (ONE - numpy_exp(-k * (x - linear_threshold))) / k
}

fn knee_slope(x: f32, k: f32, linear_threshold: f32) -> f32 {
    let denominator = (k * linear_threshold + ONE) * numpy_exp(k * (x - linear_threshold)) - ONE;
    if denominator.abs() < TINY {
        return ONE;
    }
    k * x / denominator
}

fn compressor_curve(
    x: f32,
    k: f32,
    slope: f32,
    linear_threshold: f32,
    linear_threshold_knee: f32,
    threshold: f32,
    knee: f32,
    knee_db_offset: f32,
) -> f32 {
    if x < linear_threshold {
        return x;
    }
    if knee <= ZERO {
        return db2lin(threshold + slope * (lin2db(x) - threshold));
    }
    if x < linear_threshold_knee {
        return knee_curve(x, k, linear_threshold);
    }
    db2lin(knee_db_offset + slope * (lin2db(x) - threshold - knee))
}

fn adaptive_release_curve(x: f32, a: f32, b: f32, c: f32, d: f32) -> f32 {
    let x2 = x * x;
    a * x2 * x + b * x2 + c * x + d
}

pub struct DrcProcessor {
    sample_rate: u32,
    delay_buffer: [f32; MAX_DELAY],
    metergain: f32,
    meterrelease: f32,
    threshold: f32,
    knee: f32,
    linearpregain: f32,
    linearthreshold: f32,
    slope: f32,
    attacksamplesinv: f32,
    satreleasesamplesinv: f32,
    wet: f32,
    dry: f32,
    k: f32,
    kneedboffset: f32,
    linearthresholdknee: f32,
    mastergain: f32,
    release_a: f32,
    release_b: f32,
    release_c: f32,
    release_d: f32,
    detectoravg: f32,
    compgain: f32,
    maxcompdiffdb: f32,
    spu_samples_remaining: i32,
    current_scaled_desired_gain: f32,
    current_envelope_rate: f32,
    delaybufsize: u32,
    delaywritepos: u32,
    gate_enabled: bool,
    gate_is_open: bool,
    gate_open_threshold: f32,
    gate_close_threshold: f32,
    gate_attack_coeff: f32,
    gate_release_coeff: f32,
    gate_envelope: f32,
    gate_min_gain: f32,
    gate_hold_samples: u32,
    gate_hold_counter: u32,
    limiter_enabled: bool,
    limiter_ceiling_linear: f32,
    limiter_release_coeff: f32,
    limiter_gain: f32,
}

impl DrcProcessor {
    pub fn new(sample_rate: u32, config: &DrcConfig) -> Self {
        let mut this = Self {
            sample_rate,
            delay_buffer: [0.0; MAX_DELAY],
            metergain: 0.0,
            meterrelease: 0.0,
            threshold: 0.0,
            knee: 0.0,
            linearpregain: 1.0,
            linearthreshold: 1.0,
            slope: 1.0,
            attacksamplesinv: 1.0,
            satreleasesamplesinv: 1.0,
            wet: 1.0,
            dry: 0.0,
            k: 5.0,
            kneedboffset: 0.0,
            linearthresholdknee: 0.0,
            mastergain: 1.0,
            release_a: 0.0,
            release_b: 0.0,
            release_c: 0.0,
            release_d: 0.0,
            detectoravg: 1.0,
            compgain: 1.0,
            maxcompdiffdb: -1.0,
            spu_samples_remaining: 0,
            current_scaled_desired_gain: 0.0,
            current_envelope_rate: 1.0,
            delaybufsize: 0,
            delaywritepos: 0,
            gate_enabled: false,
            gate_is_open: true,
            gate_open_threshold: 0.0,
            gate_close_threshold: 0.0,
            gate_attack_coeff: 1.0,
            gate_release_coeff: 1.0,
            gate_envelope: 1.0,
            gate_min_gain: 1.0,
            gate_hold_samples: 0,
            gate_hold_counter: 0,
            limiter_enabled: false,
            limiter_ceiling_linear: 1.0,
            limiter_release_coeff: 1.0,
            limiter_gain: 1.0,
        };
        this.configure(config);
        this
    }

    fn configure(&mut self, config: &DrcConfig) {
        let rate = self.sample_rate as f32;
        let pregain = clampf(finite_or(config.pregain_db, 5.0), -60.0, 60.0);
        let threshold = clampf(
            finite_or(config.compressor_threshold_db, -20.0),
            -160.0,
            ZERO,
        );
        let knee = clampf(finite_or(config.compressor_knee_db, 25.0), ZERO, 80.0);
        let ratio = clampf(finite_or(config.compressor_ratio, 8.0), ONE, 100.0);
        let attack = clampf(finite_or(config.compressor_attack_s, 0.001), ZERO, 10.0);
        let release = clampf(finite_or(config.compressor_release_s, 0.600), ZERO, 10.0);
        let max_predelay = MAX_DELAY as f32 / rate;
        let predelay = clampf(finite_or(config.predelay_s, 0.006), ZERO, max_predelay);
        let postgain = clampf(finite_or(config.postgain_db, ZERO), -60.0, 60.0);
        let wet = clampf(finite_or(config.wet, ONE), ZERO, ONE);
        let delay_samples = ((predelay * rate + 0.5) as u32).min(MAX_DELAY as u32);

        let linear_pregain = db2lin(pregain);
        let linear_threshold = db2lin(threshold);
        let slope = ONE / ratio;
        let attack_samples = attack * rate;
        let attacksamplesinv = if attack_samples > ZERO {
            ONE / attack_samples
        } else {
            ONE
        };
        let release_samples = if release > ZERO { release * rate } else { ONE };
        let satreleasesamplesinv = ONE / (rate * 0.0025);
        let dry = ONE - wet;
        let meterrelease = ONE - numpy_exp(-ONE / (rate * 0.325));

        let mut k = 5.0f32;
        let mut knee_db_offset = ZERO;
        let mut linear_threshold_knee = ZERO;
        if knee > ZERO {
            let x_knee = db2lin(threshold + knee);
            let mut min_k = 0.1f32;
            let mut max_k = 10000.0f32;
            for _ in 0..15 {
                if knee_slope(x_knee, k, linear_threshold) < slope {
                    max_k = k;
                } else {
                    min_k = k;
                }
                k = (min_k * max_k).sqrt();
            }
            knee_db_offset = lin2db(knee_curve(x_knee, k, linear_threshold));
            linear_threshold_knee = x_knee;
        }

        let mut mastergain = db2lin(postgain);
        if config.auto_makeup_gain {
            let full_level = compressor_curve(
                ONE,
                k,
                slope,
                linear_threshold,
                linear_threshold_knee,
                threshold,
                knee,
                knee_db_offset,
            );
            if full_level > TINY {
                mastergain *= (ONE / full_level).powf(0.6);
            }
        }

        let zone1 = clampf(finite_or(config.release_zone1, 0.090), 1.0e-4, ONE);
        let zone2 = clampf(finite_or(config.release_zone2, 0.160), zone1, ONE);
        let zone3 = clampf(finite_or(config.release_zone3, 0.420), zone2, ONE);
        let zone4 = clampf(finite_or(config.release_zone4, 0.980), zone3, ONE);
        let y1 = release_samples * zone1;
        let y2 = release_samples * zone2;
        let y3 = release_samples * zone3;
        let y4 = release_samples * zone4;

        self.meterrelease = meterrelease;
        self.threshold = threshold;
        self.knee = knee;
        self.linearpregain = linear_pregain;
        self.linearthreshold = linear_threshold;
        self.slope = slope;
        self.attacksamplesinv = attacksamplesinv;
        self.satreleasesamplesinv = satreleasesamplesinv;
        self.wet = wet;
        self.dry = dry;
        self.k = k;
        self.kneedboffset = knee_db_offset;
        self.linearthresholdknee = linear_threshold_knee;
        self.mastergain = mastergain;
        self.release_a = (-y1 + 3.0 * y2 - 3.0 * y3 + y4) / 6.0;
        self.release_b = y1 - 2.5 * y2 + 2.0 * y3 - 0.5 * y4;
        self.release_c = (-11.0 * y1 + 18.0 * y2 - 9.0 * y3 + 2.0 * y4) / 6.0;
        self.release_d = y1;

        self.gate_enabled = config.gate_enabled;
        self.gate_is_open = true;
        self.gate_open_threshold = ZERO;
        self.gate_close_threshold = ZERO;
        self.gate_attack_coeff = ONE;
        self.gate_release_coeff = ONE;
        self.gate_envelope = ONE;
        self.gate_min_gain = ONE;
        self.gate_hold_samples = 0;
        self.gate_hold_counter = 0;
        if self.gate_enabled {
            let open_threshold_db = clampf(
                finite_or(config.gate_open_threshold_db, -48.0),
                -160.0,
                ZERO,
            );
            let close_threshold_db = clampf(
                finite_or(config.gate_close_threshold_db, -54.0),
                -160.0,
                ZERO,
            )
            .min(open_threshold_db);
            let gate_attack = clampf(finite_or(config.gate_attack_s, 0.003), ZERO, 10.0);
            let gate_release = clampf(finite_or(config.gate_release_s, 0.050), ZERO, 10.0);
            let hold_ms = clampf(finite_or(config.gate_hold_ms, 10.0), ZERO, 10000.0);
            let floor_db = clampf(finite_or(config.gate_floor_db, -80.0), -160.0, ZERO);
            let user_hold_samples = (rate * hold_ms * 0.001 + 0.5) as u32;
            self.gate_open_threshold = db2lin(open_threshold_db);
            self.gate_close_threshold = db2lin(close_threshold_db);
            self.gate_attack_coeff = time_to_coeff(gate_attack, self.sample_rate);
            self.gate_release_coeff = time_to_coeff(gate_release, self.sample_rate);
            self.gate_min_gain = clampf(db2lin(floor_db), ZERO, ONE);
            self.gate_hold_samples = user_hold_samples + delay_samples;
            self.gate_hold_counter = self.gate_hold_samples;
        }

        self.limiter_enabled = config.limiter_enabled;
        self.limiter_ceiling_linear = db2lin(clampf(
            finite_or(config.limiter_ceiling_dbfs, -1.0),
            -60.0,
            ZERO,
        ));
        self.limiter_release_coeff = time_to_coeff(
            clampf(finite_or(config.limiter_release_s, 0.050), ZERO, 10.0),
            self.sample_rate,
        );
        self.delaybufsize = delay_samples;
        self.reset();
    }

    pub fn reset(&mut self) {
        self.metergain = ZERO;
        self.detectoravg = ONE;
        self.compgain = ONE;
        self.maxcompdiffdb = -ONE;
        self.spu_samples_remaining = 0;
        self.current_scaled_desired_gain = ZERO;
        self.current_envelope_rate = ONE;
        self.gate_is_open = true;
        self.gate_envelope = ONE;
        self.gate_hold_counter = self.gate_hold_samples;
        self.delaywritepos = 0;
        self.delay_buffer.fill(ZERO);
        self.limiter_gain = ONE;
    }

    pub fn latency_samples(&self) -> usize {
        self.delaybufsize as usize
    }

    fn update_gate(&mut self, absolute_input: f32) -> f32 {
        if !self.gate_enabled {
            return ONE;
        }
        let mut target = ZERO;
        if self.gate_is_open {
            if absolute_input >= self.gate_close_threshold {
                self.gate_hold_counter = self.gate_hold_samples;
                target = ONE;
            } else if self.gate_hold_counter > 0 {
                self.gate_hold_counter -= 1;
                target = ONE;
            } else {
                self.gate_is_open = false;
            }
        } else if absolute_input >= self.gate_open_threshold {
            self.gate_is_open = true;
            self.gate_hold_counter = self.gate_hold_samples;
            target = ONE;
        }
        let coefficient = if target > self.gate_envelope {
            self.gate_attack_coeff
        } else {
            self.gate_release_coeff
        };
        self.gate_envelope = clampf(
            finite_or(
                self.gate_envelope + (target - self.gate_envelope) * coefficient,
                target,
            ),
            ZERO,
            ONE,
        );
        self.gate_min_gain + self.gate_envelope * (ONE - self.gate_min_gain)
    }

    fn refresh_chunk_envelope(&mut self) {
        self.detectoravg = clampf(finite_or(self.detectoravg, ONE), ZERO, ONE);
        let scaled_desired_gain = self.detectoravg.asin() * TWO_OVER_PI;
        let mut compressor_difference_db = ONE;
        if scaled_desired_gain > TINY {
            compressor_difference_db = finite_or(lin2db(self.compgain / scaled_desired_gain), ONE);
        }
        let envelope_rate = if compressor_difference_db < ZERO {
            compressor_difference_db = finite_or(compressor_difference_db, -ONE);
            self.maxcompdiffdb = -ONE;
            let x = (clampf(compressor_difference_db, -12.0, ZERO) + 12.0) * 0.25;
            let mut release_samples = adaptive_release_curve(
                x,
                self.release_a,
                self.release_b,
                self.release_c,
                self.release_d,
            );
            if release_samples <= ONE {
                release_samples = ONE;
            }
            db2lin(SPACING_DB / release_samples)
        } else {
            compressor_difference_db = finite_or(compressor_difference_db, ONE);
            if self.maxcompdiffdb < compressor_difference_db {
                self.maxcompdiffdb = compressor_difference_db;
            }
            let mut attenuation = self.maxcompdiffdb;
            if attenuation < 0.5 {
                attenuation = 0.5;
            }
            let mut rate = ONE - (0.25 / attenuation).powf(self.attacksamplesinv);
            rate = clampf(finite_or(rate, ONE), ZERO, ONE);
            rate
        };
        self.current_scaled_desired_gain = scaled_desired_gain;
        self.current_envelope_rate = envelope_rate;
        self.spu_samples_remaining = SPU;
    }

    fn compute_gain(&mut self, gate_detector: f32, compressor_detector: f32) -> f32 {
        if self.spu_samples_remaining <= 0 {
            self.refresh_chunk_envelope();
        }
        let gate_sample = finite_input(gate_detector);
        let compressor_sample = finite_input(compressor_detector);
        let gate_gain = self.update_gate(gate_sample.abs());
        let pregained_detector = finite_or(compressor_sample * self.linearpregain, ZERO);
        let input_max = pregained_detector.abs();
        let mut attenuation = ONE;
        if input_max >= 0.0001 {
            attenuation = compressor_curve(
                input_max,
                self.k,
                self.slope,
                self.linearthreshold,
                self.linearthresholdknee,
                self.threshold,
                self.knee,
                self.kneedboffset,
            ) / input_max;
        }
        attenuation = clampf(finite_or(attenuation, ONE), ZERO, ONE);

        let mut detector_rate = ONE;
        if attenuation > self.detectoravg {
            let mut attenuation_db = -lin2db(attenuation);
            if attenuation_db < 2.0 {
                attenuation_db = 2.0;
            }
            detector_rate = db2lin(attenuation_db * self.satreleasesamplesinv) - ONE;
        }
        self.detectoravg = clampf(
            finite_or(
                self.detectoravg + (attenuation - self.detectoravg) * detector_rate,
                ONE,
            ),
            ZERO,
            ONE,
        );

        if self.current_envelope_rate < ONE {
            self.compgain +=
                (self.current_scaled_desired_gain - self.compgain) * self.current_envelope_rate;
        } else {
            if self.compgain < TINY {
                self.compgain = TINY;
            }
            self.compgain *= self.current_envelope_rate;
            if self.compgain > ONE {
                self.compgain = ONE;
            }
        }
        self.compgain = clampf(finite_or(self.compgain, ONE), TINY, ONE);
        let compressor_mix_gain = (PI_OVER_TWO * self.compgain).sin();
        let compressor_output_gain = self.dry + self.wet * self.mastergain * compressor_mix_gain;
        let gain = finite_or(gate_gain * compressor_output_gain, ONE);

        let compressor_gain_db = lin2db(compressor_mix_gain);
        if compressor_gain_db < self.metergain {
            self.metergain = compressor_gain_db;
        } else {
            self.metergain += (compressor_gain_db - self.metergain) * self.meterrelease;
        }
        self.metergain = finite_or(self.metergain, ZERO);
        self.spu_samples_remaining -= 1;
        gain
    }

    fn apply_gain(&mut self, input: f32, gain: f32) -> f32 {
        let pregained_input = finite_or(finite_input(input) * self.linearpregain, ZERO);
        let mut delayed_input = pregained_input;
        if self.delaybufsize > 0 {
            let pos = self.delaywritepos as usize;
            delayed_input = self.delay_buffer[pos];
            self.delay_buffer[pos] = pregained_input;
            self.delaywritepos += 1;
            if self.delaywritepos >= self.delaybufsize {
                self.delaywritepos = 0;
            }
        }
        finite_or(delayed_input * finite_or(gain, ONE), ZERO)
    }

    fn update_limiter_gain(&mut self, required_gain: f32) -> f32 {
        let required = clampf(finite_or(required_gain, ONE), ZERO, ONE);
        let mut current = clampf(finite_or(self.limiter_gain, ONE), ZERO, ONE);
        if required < current {
            self.limiter_gain = required;
        } else {
            let distance_to_unity = ONE - current;
            let release_delta = distance_to_unity * self.limiter_release_coeff;
            current += release_delta;
            self.limiter_gain = current.min(required);
        }
        self.limiter_gain = clampf(self.limiter_gain, ZERO, ONE);
        self.limiter_gain
    }

    pub fn process_sample(&mut self, input: f32) -> f32 {
        let gain = self.compute_gain(input, input);
        let mut output = self.apply_gain(input, gain);
        if self.limiter_enabled {
            let magnitude = if output.is_finite() {
                output.abs()
            } else {
                ZERO
            };
            let required = if magnitude > self.limiter_ceiling_linear {
                self.limiter_ceiling_linear / magnitude
            } else {
                ONE
            };
            output *= self.update_limiter_gain(required);
        }
        output
    }
}
