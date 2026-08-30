# 输出 DRC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 TSS 三区 look-ahead DRC 以纯 Rust 单通道形式接入 Beamformer 输出：增益之后、PCM16 之前；`compare_wav` 的 mic1 走同一条链。

**Architecture:** `src/drc/` 移植 `tss_algo_pipeline` 的 C++ `DrcProcessor`（含 `numpy_exp`，以通过 Python golden 的 `atol=5e-7`）。BF 在写 WAV 前调用 `TssDrc::process_in_place`，`finalize` 再 `flush` 32 点。Pipeline 只增加 `enable_drc`。

**Tech Stack:** Rust 2024、本仓库现有 `serde`/`serde_json`/`hound`（dev）、来源仓库仅用于一次性生成 golden。

**Spec:** `docs/plans/2026-08-30-output-drc-design.md`

---

## 文件划分

| 路径 | 职责 |
|------|------|
| `LICENSES/tss_drc.MIT` | MIT 原文 |
| `tools/generate_drc_golden.py` | 一次性生成，不进 `cargo test` |
| `src/drc/testdata/pipeline_16k_mono.json` | 提交的 golden |
| `src/drc/config.rs` | `DrcConfig::pipeline()` |
| `src/drc/core.rs` | 单样本 `DrcProcessor` |
| `src/drc/mod.rs` | `TssDrc` + 单测 |
| `src/main.rs` | `mod drc` |
| `src/beamformer/mod.rs` | 增益 → DRC → PCM；scratch 预分配 |
| `src/beamformer/weights.rs` | 结构体字面量补 `enable_drc` |
| `src/pipeline.rs` | 解析 `enable_drc` |
| `configs/doa_bf.toml` / `configs/bf_fixed.toml` | `enable_drc = true` |
| `README.md` / `AGENTS.md` | 输出路径说明 |

来源对照（只读，不要改来源仓库）：

- `C:\Projects\GitProjects\tss_algo_pipeline\native\src\tss_native_drc.cpp`
- `C:\Projects\GitProjects\tss_algo_pipeline\src\tss_algo_pipeline\drc\config.py`
- 来源提交必须是 `c61c5d6b904fa56055b06320810893e851c6e938`

Windows 提交用 `git commit -m "subject" -m "- body"`，只 `git add` 本任务列出的路径。

---

### Task 1: 许可证与 golden 向量

**Files:**

- Create: `LICENSES/tss_drc.MIT`
- Create: `tools/generate_drc_golden.py`
- Create: `src/drc/testdata/pipeline_16k_mono.json`

- [ ] **Step 1: 复制许可证**

把 `C:\Projects\GitProjects\tss_algo_pipeline\LICENSES\tss_drc.MIT` 原样复制到 `LICENSES/tss_drc.MIT`。

- [ ] **Step 2: 写生成脚本**

```python
"""Generate src/drc/testdata/pipeline_16k_mono.json from pinned tss_algo_pipeline."""

from __future__ import annotations

import json
import math
import subprocess
import sys
from pathlib import Path

SOURCE_REPO = "tss_algo_pipeline"
SOURCE_COMMIT = "c61c5d6b904fa56055b06320810893e851c6e938"
SOURCE_ROOT = Path(r"C:\Projects\GitProjects\tss_algo_pipeline")
SAMPLE_RATE = 16000
PREDELAY_S = 0.002
CHUNK_SIZES = [1, 15, 16, 17, 80, 161, 256, 3, 511, 988]
MODE = "independent"

REPO_ROOT = Path(__file__).resolve().parents[1]
OUT_PATH = REPO_ROOT / "src" / "drc" / "testdata" / "pipeline_16k_mono.json"


def _git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(SOURCE_ROOT), *args], text=True).strip()


def build_input(n: int = 2048) -> "object":
    import numpy as np

    t = np.arange(n, dtype=np.float32) / np.float32(SAMPLE_RATE)
    x = np.zeros(n, dtype=np.float32)
    two_pi = np.float32(2.0 * math.pi)
    x[0:200] = np.float32(1.0e-5)
    x[200:400] = np.float32(0.0178) * np.sin(two_pi * np.float32(440.0) * t[200:400])
    x[400:900] = np.sin(two_pi * np.float32(440.0) * t[400:900]).astype(np.float32)
    x[900:1400] = np.float32(0.0316) * np.sin(two_pi * np.float32(440.0) * t[900:1400])
    x[1400:] = np.float32(0.5) * np.sin(two_pi * np.float32(440.0) * t[1400:])
    return x


def main() -> int:
    if not SOURCE_ROOT.is_dir():
        print(f"missing source repo: {SOURCE_ROOT}", file=sys.stderr)
        return 1
    dirty = _git("status", "--porcelain")
    if dirty:
        print("source repo working tree is not clean", file=sys.stderr)
        print(dirty, file=sys.stderr)
        return 1
    head = _git("rev-parse", "HEAD")
    if head != SOURCE_COMMIT:
        print(f"source HEAD {head} != {SOURCE_COMMIT}", file=sys.stderr)
        return 1

    sys.path.insert(0, str(SOURCE_ROOT / "src"))
    from tss_algo_pipeline.drc import DrcConfig, TssDrc
    import numpy as np

    assert sum(CHUNK_SIZES) == 2048
    audio = build_input(2048)
    config = DrcConfig.default()
    config.predelay_s = PREDELAY_S
    drc = TssDrc(SAMPLE_RATE, 1, config)
    parts = []
    offset = 0
    for size in CHUNK_SIZES:
        frame = audio[offset : offset + size]
        parts.append(drc.process_frame_independent(frame))
        offset += size
    expected = np.concatenate(parts)
    assert offset == 2048
    assert expected.shape == (2048,)
    assert np.all(np.isfinite(audio)) and np.all(np.isfinite(expected))

    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "source_repo": SOURCE_REPO,
        "source_commit": SOURCE_COMMIT,
        "sample_rate": SAMPLE_RATE,
        "predelay_s": PREDELAY_S,
        "mode": MODE,
        "chunk_sizes": CHUNK_SIZES,
        "input": [float(v) for v in audio],
        "expected": [float(v) for v in expected],
    }
    OUT_PATH.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {OUT_PATH}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 3: 生成 JSON**

Run:

```powershell
uv run --directory C:\Projects\GitProjects\tss_algo_pipeline python C:\Projects\RustProjects\respeaker_algo\tools\generate_drc_golden.py
```

Expected: 打印 `wrote ...pipeline_16k_mono.json`，退出码 0。若报 HEAD 不匹配或 working tree dirty，停下来，不要改来源仓库凑数。

检查：`input`/`expected` 长度均为 2048，`sum(chunk_sizes)==2048`，`source_commit` 为上述值，数值均为有限。

- [ ] **Step 4: Commit**

```powershell
git add LICENSES/tss_drc.MIT tools/generate_drc_golden.py src/drc/testdata/pipeline_16k_mono.json
git commit -m "test(drc): 添加 TSS 16 kHz golden 与许可证" -m "- 固定 source_commit c61c5d6b904fa56055b06320810893e851c6e938" -m "- 生成脚本校验来源仓库干净且 HEAD 匹配后才写入"
```

---

### Task 2: 先写会失败的 DRC 单测与空模块

**Files:**

- Create: `src/drc/config.rs`
- Create: `src/drc/core.rs`
- Create: `src/drc/mod.rs`
- Modify: `src/main.rs`（增加 `mod drc;`）

- [ ] **Step 1: 挂上模块并写测试（实现先空，让测试失败）**

`src/main.rs` 在 `mod doa;` 旁增加：

```rust
mod drc;
```

`src/drc/config.rs`：

```rust
#[derive(Clone, Debug)]
pub struct DrcConfig {
    pub gate_enabled: bool,
    pub gate_open_threshold_db: f32,
    pub gate_close_threshold_db: f32,
    pub gate_attack_s: f32,
    pub gate_release_s: f32,
    pub gate_hold_ms: f32,
    pub gate_floor_db: f32,
    pub pregain_db: f32,
    pub compressor_threshold_db: f32,
    pub compressor_knee_db: f32,
    pub compressor_ratio: f32,
    pub compressor_attack_s: f32,
    pub compressor_release_s: f32,
    pub predelay_s: f32,
    pub release_zone1: f32,
    pub release_zone2: f32,
    pub release_zone3: f32,
    pub release_zone4: f32,
    pub postgain_db: f32,
    pub wet: f32,
    pub auto_makeup_gain: bool,
    pub limiter_enabled: bool,
    pub limiter_ceiling_dbfs: f32,
    pub limiter_release_s: f32,
}

impl DrcConfig {
    pub fn pipeline() -> Self {
        Self {
            gate_enabled: true,
            gate_open_threshold_db: -48.0,
            gate_close_threshold_db: -54.0,
            gate_attack_s: 0.003,
            gate_release_s: 0.050,
            gate_hold_ms: 10.0,
            gate_floor_db: -80.0,
            pregain_db: 5.0,
            compressor_threshold_db: -20.0,
            compressor_knee_db: 25.0,
            compressor_ratio: 8.0,
            compressor_attack_s: 0.001,
            compressor_release_s: 0.600,
            predelay_s: 0.002,
            release_zone1: 0.090,
            release_zone2: 0.160,
            release_zone3: 0.420,
            release_zone4: 0.980,
            postgain_db: 0.0,
            wet: 1.0,
            auto_makeup_gain: false,
            limiter_enabled: true,
            limiter_ceiling_dbfs: -1.0,
            limiter_release_s: 0.050,
        }
    }
}
```

`src/drc/core.rs` 先放空壳（Task 3 填满）：

```rust
use crate::drc::config::DrcConfig;

pub struct DrcProcessor {
    delaybufsize: u32,
}

impl DrcProcessor {
    pub fn new(_sample_rate: u32, _config: &DrcConfig) -> Self {
        Self { delaybufsize: 0 }
    }

    pub fn latency_samples(&self) -> usize {
        self.delaybufsize as usize
    }

    pub fn reset(&mut self) {}

    pub fn process_sample(&mut self, input: f32) -> f32 {
        input
    }
}
```

`src/drc/mod.rs`：

```rust
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
        let input: Vec<f32> = (0..512)
            .map(|i| 0.1 * ((i as f32 * 0.17).sin()))
            .collect();
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
        let input: Vec<f32> = (0..1024)
            .map(|i| 0.2 * ((i as f32 * 0.31).sin()))
            .collect();
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
        let input: Vec<f32> = (0..256)
            .map(|i| 0.4 * ((i as f32 * 0.11).sin()))
            .collect();
        assert!(input[224..].iter().any(|v| v.abs() > 0.1));

        let mut a = TssDrc::pipeline(16_000).unwrap();
        let mut head = input.clone();
        a.process_in_place(&mut head);
        let mut tail = [0.0f32; 32];
        a.flush(&mut tail);
        assert!(tail.iter().any(|v| v.abs() > 1.0e-4), "flush tail must carry delayed signal");

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
        let follow: Vec<f32> = (0..64)
            .map(|i| 0.15 * ((i as f32 * 0.2).sin()))
            .collect();

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
        let after: Vec<f32> = (0..256)
            .map(|i| 0.2 * ((i as f32 * 0.13).sin()))
            .collect();

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
        assert!(t.iter().chain(z.iter()).chain(a.iter()).all(|v| v.is_finite()));

        let mut split = t;
        split.extend_from_slice(&z);
        split.extend_from_slice(&a);
        assert_eq!(once, split);
    }

    #[test]
    fn golden_matches_source_within_5e7() {
        let golden = load_golden();
        assert_eq!(golden.source_commit, "c61c5d6b904fa56055b06320810893e851c6e938");
        assert_eq!(golden.sample_rate, 16_000);
        assert_eq!(golden.predelay_s, 0.002);
        assert_eq!(golden.mode, "independent");
        assert_eq!(golden.chunk_sizes, vec![1, 15, 16, 17, 80, 161, 256, 3, 511, 988]);
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
```

- [ ] **Step 2: 跑测试，确认失败**

Run:

```powershell
cargo test --all-targets -- latency_samples_is_32_at_16k -- --nocapture
```

Expected: FAIL，`latency_samples` 为 0，不是 32。

- [ ] **Step 3: Commit 测试与空壳**

```powershell
git add src/main.rs src/drc/config.rs src/drc/core.rs src/drc/mod.rs
git commit -m "test(drc): 添加单通道 DRC 行为与 golden 测试" -m "- 先挂空 DrcProcessor，latency 测试应失败" -m "- golden 只读本仓库 JSON"
```

---

### Task 3: 实现单通道 DRC（对齐 C++ `DrcProcessor`）

**Files:**

- Modify: `src/drc/core.rs`（整文件替换）
- Modify: `src/drc/mod.rs`（`TssDrc` 已够用，不必改 API）

**移植规则：** 以 `tss_native_drc.cpp` 的 `DrcProcessor` 为准，不要按 Python 向量化循环“意译”。`exp` 必须用下面的 `numpy_exp`（C++ 注释写明：否则 float32 包络对不齐 Python）。`db2lin` 用 `10f32.powf(0.05 * db)`，与 C++ `std::pow` 一致。

- [ ] **Step 1: 写入完整 `src/drc/core.rs`**

文件头保留 MIT 说明：

```rust
//! Sample-accurate port of tss_native DrcProcessor (sndfilter compressor, MIT).

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
    const ROUND_MAGIC: f32 = 0x1.800000p+23;
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
    libm::ldexpf(numerator / denominator, quadrant as i32)
}
```

若不想加 `libm` 依赖：用 `(numerator / denominator) * 2f32.powi(quadrant as i32)` 代替 `ldexpf`（`quadrant` 在该值域是整数）。**不要新增 Cargo 依赖。** 把最后一行改成：

```rust
    (numerator / denominator) * 2f32.powi(quadrant as i32)
```

其余函数与 C++ 同序：

```rust
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
        let threshold = clampf(finite_or(config.compressor_threshold_db, -20.0), -160.0, ZERO);
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
            let open_threshold_db =
                clampf(finite_or(config.gate_open_threshold_db, -48.0), -160.0, ZERO);
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
            compressor_difference_db =
                finite_or(lin2db(self.compgain / scaled_desired_gain), ONE);
        }
        let envelope_rate = if compressor_difference_db < ZERO {
            compressor_difference_db = finite_or(compressor_difference_db, -ONE);
            self.maxcompdiffdb = -ONE;
            let x = (clampf(compressor_difference_db, -12.0, ZERO) + 12.0) * 0.25;
            let mut release_samples =
                adaptive_release_curve(x, self.release_a, self.release_b, self.release_c, self.release_d);
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
            let magnitude = if output.is_finite() { output.abs() } else { ZERO };
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
```

`process_in_place` 只改传入切片；delay line 是 `[f32; 1024]`，实时路径不得 `Vec::push`。

- [ ] **Step 2: 跑 DRC 测试**

Run:

```powershell
cargo test --all-targets -- drc::
```

Expected: 全部 PASS。若 `golden_matches_source_within_5e7` 失败，先打印 `max abs`：

1. 对照 C++ `configure` / `process_sample` 是否漏字段（尤其 `release_zone*`、`spu_samples_remaining` 初值 0）。
2. 确认 `numpy_exp` 的 `mul_add` 顺序与 C++ `std::fma` 一致。
3. 不要放宽 `ATOL`。

- [ ] **Step 3: Commit**

```powershell
git add src/drc/core.rs src/drc/mod.rs src/drc/config.rs
git commit -m "feat(drc): 移植 TSS 单通道 look-ahead DRC" -m "- 对齐 C++ DrcProcessor 与 numpy_exp" -m "- 覆盖 gate/comp/limiter、flush、非有限输入与 golden 5e-7"
```

---

### Task 4: Pipeline 解析 `enable_drc`

**Files:**

- Modify: `src/pipeline.rs`
- Modify: `src/beamformer/mod.rs`（只加配置字段与 Default，先不改 runtime）
- Modify: `src/beamformer/weights.rs`（`sd_config` 补字段）

- [ ] **Step 1: 写会失败的配置测试**

在 `src/pipeline.rs` 的 `beamformer_omitted_fields_match_documented_defaults` 里，解构增加 `enable_drc`，并断言：

```rust
assert_eq!(*enable_drc, d.enable_drc);
assert!(!d.enable_drc);
```

在 `beamformer_validate_passes_through_algorithm_wav_and_compare` 的 TOML 增加 `enable_drc = true`，解构后 `assert!(*enable_drc);`。

- [ ] **Step 2: 跑测试确认编译失败**

Run:

```powershell
cargo test --all-targets -- beamformer_omitted_fields_match_documented_defaults
```

Expected: FAIL/compile error，没有 `enable_drc` 字段。

- [ ] **Step 3: 加上字段并贯通**

`BeamformerConfig` 增加 `pub enable_drc: bool`。`Default` 为 `false`。`validate` 不校验该布尔值。

所有完整结构体字面量补 `enable_drc: false`：

- `src/beamformer/mod.rs`：`Default`、`fixed_das_config`
- `src/beamformer/weights.rs`：`sd_config`
- `src/pipeline.rs`：`validate` 与 `PipelineRuntime::new` 两处 `BeamformerConfig { ... }`

`ModuleConfig::Beamformer`：

```rust
#[serde(default)]
enable_drc: bool,
```

默认即 `false`，不必单独 default 函数。所有 `ModuleConfig::Beamformer { ... }` 解构都加上 `enable_drc`（validate、new、两则测试）。

- [ ] **Step 4: 跑 pipeline 测试**

Run:

```powershell
cargo test --all-targets -- pipeline::
```

Expected: PASS。

- [ ] **Step 5: Commit**

```powershell
git add src/pipeline.rs src/beamformer/mod.rs src/beamformer/weights.rs
git commit -m "feat(pipeline): 增加 beamformer.enable_drc" -m "- 默认 false，省略字段与 Default 一致" -m "- 校验会把用户原值传入 BeamformerConfig"
```

---

### Task 5: Beamformer 输出路径接入 DRC

**Files:**

- Modify: `src/beamformer/mod.rs`

- [ ] **Step 1: 扩展 runtime 字段**

`BeamformerRuntime` 增加：

```rust
drc_bf: Option<crate::drc::TssDrc>,
drc_mic: Option<crate::drc::TssDrc>,
float_scratch: Vec<f32>,
#[cfg(test)]
gained_bf: Vec<f32>,
#[cfg(test)]
gained_mic: Vec<f32>,
```

`new` 里：

```rust
let wav_channels = if config.compare_wav { 2 } else { 1 };
let pcm_cap = wav_channels * HOP_SIZE;
let (drc_bf, drc_mic) = if config.enable_drc {
    (
        Some(crate::drc::TssDrc::pipeline(SAMPLE_RATE)?),
        if config.compare_wav {
            Some(crate::drc::TssDrc::pipeline(SAMPLE_RATE)?)
        } else {
            None
        },
    )
} else {
    (None, None)
};
let mut float_scratch = Vec::with_capacity(HOP_SIZE);
float_scratch.resize(HOP_SIZE, 0.0);
```

构造：

```rust
pcm_scratch: Vec::with_capacity(pcm_cap),
mic0_buf: Vec::with_capacity(2 * HOP_SIZE),
float_scratch,
drc_bf,
drc_mic,
```

测试访问：

```rust
#[cfg(test)]
fn scratch_capacities(&self) -> (usize, usize, usize) {
    (
        self.pcm_scratch.capacity(),
        self.float_scratch.capacity(),
        self.mic0_buf.capacity(),
    )
}

#[cfg(test)]
fn gained_bf(&self) -> &[f32] {
    &self.gained_bf
}

#[cfg(test)]
fn gained_mic(&self) -> &[f32] {
    &self.gained_mic
}
```

- [ ] **Step 2: 改写 PCM 路径**

把 `apply_gain` 拆成「已是线性增益后的浮点 → PCM」：

```rust
fn float_to_pcm(y: f32, clipped: &mut u64) -> i16 {
    let r = (y * 32768.0).round();
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

fn apply_gain(y: f32, gain: f32, clipped: &mut u64) -> i16 {
    float_to_pcm(y * gain, clipped)
}
```

新增（`enable_drc` 时用）。`float_scratch` 长度保持 `HOP_SIZE`，按 `samples.len()` 填前缀：

```rust
fn write_pcm_with_drc(
    samples: &[f32],
    gain: f32,
    drc: &mut crate::drc::TssDrc,
    float_scratch: &mut [f32],
    scratch: &mut Vec<i16>,
    wav: Option<&mut WavSink>,
    clipped: &mut u64,
    #[cfg(test)] gained_trace: &mut Vec<f32>,
) -> Result<(), String> {
    debug_assert!(float_scratch.len() >= samples.len());
    debug_assert!(scratch.capacity() >= samples.len());
    scratch.clear();
    for (i, &y) in samples.iter().enumerate() {
        let gained = y * gain;
        #[cfg(test)]
        gained_trace.push(gained);
        float_scratch[i] = gained;
    }
    drc.process_in_place(&mut float_scratch[..samples.len()]);
    for y in &float_scratch[..samples.len()] {
        scratch.push(float_to_pcm(*y, clipped));
    }
    if let Some(w) = wav {
        w.write_samples(scratch)?;
    }
    Ok(())
}
```

`write_compare_pcm_with_drc`：先处理 mic1 到偶下标，再处理 BF 到奇下标；两次都复用同一 `float_scratch`。`pcm_scratch.capacity()` 必须 `>= 2 * samples.len()`。`clear` 后 `resize(2 * n, 0)` 再填，避免 `push` 扩容。

`write_pcm` / `write_compare_pcm`（无 DRC）同样改为：`clear` 后按已知长度 `resize` 再赋值，或 `extend` 到已预留容量。`compare` 无 DRC 时 `pcm_scratch` 容量已是 `2 * HOP_SIZE`。

`on_hop` / `finalize` 的 STFT 回调：

```rust
if let Some(drc) = self.drc_bf.as_mut() {
    // compare 时再 as_mut drc_mic
    write_*_with_drc(...)
} else {
    write_*(...)
}
```

`finalize`：STFT `flush_zeros` 之后，若启用 DRC：

```rust
let mut tail = [0.0f32; 32];
if let Some(drc) = self.drc_bf.as_mut() {
    drc.flush(&mut tail);
    // mono: 把 tail 转 PCM 写入
    // compare: 同时 flush drc_mic 到另一 tail，交错写入
}
```

`output_frames` 仍只加 STFT 样本，不要加 32。`output_frames == input_frames` 保持。

`finalize` 里现有 `mem::take` scratch 的写法要同时 take `float_scratch`、两个 DRC，flush 完再放回。

- [ ] **Step 3: 写 BF 接入测试（追加到 `src/beamformer/mod.rs` tests）**

```rust
    #[test]
    fn enable_drc_false_keeps_compare_length() {
        let (dir, prefix) = temp_dir_prefix("drc_off");
        let mut cfg = fixed_das_config(0.0);
        cfg.wav = true;
        cfg.compare_wav = true;
        cfg.enable_drc = false;
        cfg.output_gain_db = 0.0;
        let frames = HOP_SIZE * 3;
        let mut block = Vec::with_capacity(frames * MIC_COUNT);
        for n in 0..frames {
            block.push(n as i16);
            block.extend_from_slice(&[0, 0, 0]);
        }
        let mut rt = BeamformerRuntime::new(cfg, &dir, &prefix).unwrap();
        rt.push_block(&block, None).unwrap();
        rt.finalize().unwrap();
        assert_eq!(rt.stats().output_frames, rt.stats().input_frames);
        let path = format!("{dir}/{prefix}_respeaker_bf.wav");
        let reader = hound::WavReader::open(&path).unwrap();
        let samples: Vec<i16> = reader.samples::<i16>().map(Result::unwrap).collect();
        assert_eq!(samples.len(), frames * 2);
    }

    #[test]
    fn enable_drc_extends_wav_by_32_and_starts_near_zero() {
        let (dir, prefix) = temp_dir_prefix("drc_on");
        let mut cfg = fixed_das_config(0.0);
        cfg.wav = true;
        cfg.enable_drc = true;
        cfg.output_gain_db = 15.0;
        let frames = HOP_SIZE * 4;
        let block: Vec<i16> = (0..frames)
            .flat_map(|_| [2000i16, 2000, 2000, 2000])
            .collect();
        let mut rt = BeamformerRuntime::new(cfg, &dir, &prefix).unwrap();
        rt.push_block(&block, None).unwrap();
        rt.finalize().unwrap();
        assert_eq!(rt.stats().output_frames, rt.stats().input_frames);
        let path = format!("{dir}/{prefix}_respeaker_bf.wav");
        let reader = hound::WavReader::open(&path).unwrap();
        let samples: Vec<i16> = reader.samples::<i16>().map(Result::unwrap).collect();
        assert_eq!(samples.len(), frames + 32);
        assert!(samples[..32].iter().all(|s| s.abs() <= 2));
    }

    #[test]
    fn enable_drc_flush_keeps_delayed_tail() {
        let (dir, prefix) = temp_dir_prefix("drc_tail");
        let mut cfg = fixed_das_config(0.0);
        cfg.wav = true;
        cfg.enable_drc = true;
        cfg.output_gain_db = 15.0;
        let frames = HOP_SIZE * 4;
        let block: Vec<i16> = (0..frames)
            .flat_map(|_| [4000i16, 4000, 4000, 4000])
            .collect();
        let mut rt = BeamformerRuntime::new(cfg, &dir, &prefix).unwrap();
        rt.push_block(&block, None).unwrap();
        rt.finalize().unwrap();
        let gained = rt.gained_bf().to_vec();
        assert!(gained[gained.len().saturating_sub(32)..]
            .iter()
            .any(|v| v.abs() > 1.0e-3));

        let mut expected = gained;
        expected.extend(std::iter::repeat_n(0.0, 32));
        crate::drc::TssDrc::pipeline(SAMPLE_RATE)
            .unwrap()
            .process_in_place(&mut expected);

        let path = format!("{dir}/{prefix}_respeaker_bf.wav");
        let reader = hound::WavReader::open(&path).unwrap();
        let pcm: Vec<i16> = reader.samples::<i16>().map(Result::unwrap).collect();
        assert_eq!(pcm.len(), expected.len());
        assert!(pcm[pcm.len() - 32..].iter().any(|s| *s != 0));
        for (i, (p, e)) in pcm.iter().zip(expected.iter()).enumerate() {
            let recon = *p as f32 / 32768.0;
            let err = (recon - *e).abs();
            assert!(err <= 1.5 / 32768.0, "sample {i} err={err}");
        }
    }

    #[test]
    fn compare_wav_drc_matches_independent_instances() {
        let (dir, prefix) = temp_dir_prefix("drc_cmp");
        let mut cfg = fixed_das_config(0.0);
        cfg.wav = true;
        cfg.compare_wav = true;
        cfg.enable_drc = true;
        cfg.output_gain_db = 15.0;
        let frames = HOP_SIZE * 4;
        let mut block = Vec::with_capacity(frames * MIC_COUNT);
        for n in 0..frames {
            block.push(((n % 2000) + 500) as i16);
            block.extend_from_slice(&[800, 800, 800]);
        }
        let mut rt = BeamformerRuntime::new(cfg, &dir, &prefix).unwrap();
        rt.push_block(&block, None).unwrap();
        rt.finalize().unwrap();

        let mut exp_l = rt.gained_mic().to_vec();
        exp_l.extend(std::iter::repeat_n(0.0, 32));
        crate::drc::TssDrc::pipeline(SAMPLE_RATE)
            .unwrap()
            .process_in_place(&mut exp_l);
        let mut exp_r = rt.gained_bf().to_vec();
        exp_r.extend(std::iter::repeat_n(0.0, 32));
        crate::drc::TssDrc::pipeline(SAMPLE_RATE)
            .unwrap()
            .process_in_place(&mut exp_r);

        let path = format!("{dir}/{prefix}_respeaker_bf.wav");
        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 2);
        let pcm: Vec<i16> = reader.samples::<i16>().map(Result::unwrap).collect();
        assert_eq!(pcm.len(), exp_l.len() * 2);
        assert!(pcm[pcm.len() - 64..].chunks_exact(2).any(|p| p[0] != 0 && p[1] != 0));
        for (i, pair) in pcm.chunks_exact(2).enumerate() {
            let l = pair[0] as f32 / 32768.0;
            let r = pair[1] as f32 / 32768.0;
            assert!((l - exp_l[i]).abs() <= 1.5 / 32768.0, "L {i}");
            assert!((r - exp_r[i]).abs() <= 1.5 / 32768.0, "R {i}");
        }
    }

    #[test]
    fn compare_wav_scratch_capacity_stays_fixed() {
        let (dir, prefix) = temp_dir_prefix("drc_cap");
        let mut cfg = fixed_das_config(0.0);
        cfg.wav = true;
        cfg.compare_wav = true;
        cfg.enable_drc = true;
        let mut rt = BeamformerRuntime::new(cfg, &dir, &prefix).unwrap();
        let before = rt.scratch_capacities();
        assert!(before.0 >= 2 * HOP_SIZE);
        assert!(before.1 >= HOP_SIZE);
        assert!(before.2 >= 2 * HOP_SIZE);
        let frames = HOP_SIZE * 4;
        let block: Vec<i16> = (0..frames)
            .flat_map(|_| [1000i16, 1000, 1000, 1000])
            .collect();
        rt.push_block(&block, None).unwrap();
        rt.finalize().unwrap();
        assert_eq!(rt.scratch_capacities(), before);
    }
```

`enable_drc_false_keeps_compare_length` 必须在现有 `compare_wav_writes_stereo_mic1_and_bf_output` 仍 PASS 的前提下成立。

- [ ] **Step 4: 跑 BF + DRC 测试**

Run:

```powershell
cargo test --all-targets -- beamformer::
```

Expected: 全部 PASS，包括旧的 compare/clip 测试。

- [ ] **Step 5: Commit**

```powershell
git add src/beamformer/mod.rs
git commit -m "feat(bf): 输出增益之后接入可选 DRC" -m "- compare_wav 左右各用独立 TssDrc" -m "- finalize flush 32 点，不计入 output_frames" -m "- scratch 按声道与 2*HOP 预留，hop 路径不扩容"
```

---

### Task 6: 配置示例与文档

**Files:**

- Modify: `configs/doa_bf.toml`
- Modify: `configs/bf_fixed.toml`
- Modify: `README.md`
- Modify: `AGENTS.md`

- [ ] **Step 1: 打开示例配置**

两份 TOML 在 `compare_wav = true` 下增加一行：

```toml
enable_drc = true
```

- [ ] **Step 2: 改 README**

通道表 BF 行改为：

```text
| — | Beamformer 输出（可选） | `*_respeaker_bf.wav`（mono；`compare_wav = true` 时为双声道：左=mic1、右=BF。两边都先乘 `output_gain_db`，若 `enable_drc` 再过 DRC） |
```

`doa_bf` / `bf_fixed` 说明改为「15 dB 输出增益 + 输出 DRC，双声道对比」。

默认可省略字段列表增加 `` `enable_drc = false` ``。不要写「与 TSS 管线电平一致」。

- [ ] **Step 3: 改 AGENTS.md 第一段 BF 句**

把 `compare_wav = true writes ... left = mic1 × gain, right = BF output × gain` 改成：增益之后可接 DRC；`compare_wav` 左右都是增益（+ 可选 DRC）。

- [ ] **Step 4: Commit**

```powershell
git add configs/doa_bf.toml configs/bf_fixed.toml README.md AGENTS.md
git commit -m "docs(drc): 打开 15 dB 配置的输出 DRC 并更新说明" -m "- doa_bf / bf_fixed 设置 enable_drc = true" -m "- 明确 DRC 输入比 TSS 管线低 5 dB，不宣称电平一致"
```

---

### Task 7: 提交前四件套

- [ ] **Step 1: 格式**

```powershell
cargo fmt --all -- --check
```

Expected: 退出码 0。失败则 `cargo fmt --all` 后再 check，把格式改动并入未提交文件或新 commit `style: rustfmt`。

- [ ] **Step 2: clippy**

```powershell
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: 退出码 0。

- [ ] **Step 3: 测试**

```powershell
cargo test --all-targets
```

Expected: 全部 PASS。

- [ ] **Step 4: release 构建**

```powershell
cargo build --release
```

Expected: 退出码 0。

不要跑硬件录音。需要听感时由用户执行：

```powershell
cargo run --release -- --duration 10 --pipeline-config configs/doa_bf.toml
```

---

## Spec 覆盖对照

| Spec 条目 | Task |
|-----------|------|
| `src/drc/` 单通道 API | 2–3 |
| 参数表含 `release_zone1..4`、2 ms、+5 dB pregain | 2 `DrcConfig::pipeline` + 3 |
| golden + 生成脚本 + dirty/HEAD 检查 | 1 |
| 行为测试 + 非有限 + 长静音 + flush 等价 | 2–3 |
| `enable_drc` 默认 false | 4 |
| 增益 → DRC → PCM；compare 双实例 | 5 |
| WAV +32、不计入 `output_frames`、不补偿开头 | 5 |
| scratch：PCM 按声道、`float` = HOP、`mic0` = 2×HOP | 5 |
| 配置与文档 | 6 |
| 四件套验证 | 7 |
| 不移植多通道 / 不做 pipeline 音频总线 / 不改 15 dB | 全程遵守 |

硬件听感不在本计划内。
