//! 任意块长 4 通道交织 PCM → 512/256 分帧。
//!
//! 采集端发送的 `Vec<i16>` 块长不固定，不能把每个采集包直接当成一个 STFT
//! 帧。本模块跨包维护 4 路环形缓冲：
//! - 首次累计满 `FRAME_SIZE`（512）个采样后产生第一帧；
//! - 之后每新增 `HOP_SIZE`（256）个采样产生一帧；
//! - `copy_frame()` 输出从最旧采样到最新采样，输入块边界不影响输出帧。

use crate::doa::{FRAME_SIZE, HOP_SIZE, MIC_COUNT};

/// 分帧器：4 路环形缓冲。
pub struct FrameAssembler {
    ring: [[f32; FRAME_SIZE]; MIC_COUNT],
    write_pos: usize,
    filled: usize,
    samples_since_emit: usize,
    total_samples: u64,
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            ring: [[0.0; FRAME_SIZE]; MIC_COUNT],
            write_pos: 0,
            filled: 0,
            samples_since_emit: 0,
            total_samples: 0,
        }
    }

    /// 写入同一时刻的 4 路归一化采样。
    /// 首次累计满 512 点后返回 true；以后每新增 256 点返回 true。
    pub fn push(&mut self, sample: [f32; MIC_COUNT]) -> bool {
        for (ch, &s) in sample.iter().enumerate() {
            self.ring[ch][self.write_pos] = s;
        }
        self.write_pos = (self.write_pos + 1) % FRAME_SIZE;
        self.filled = (self.filled + 1).min(FRAME_SIZE);
        self.total_samples += 1;
        self.samples_since_emit += 1;

        if self.total_samples < FRAME_SIZE as u64 {
            return false;
        }
        let ready = self.total_samples == FRAME_SIZE as u64 || self.samples_since_emit >= HOP_SIZE;
        if ready {
            self.samples_since_emit = 0;
        }
        ready
    }

    /// 按时间先后复制当前 512 点窗；`out[通道][时间]`。
    pub fn copy_frame(&self, out: &mut [[f32; FRAME_SIZE]; MIC_COUNT]) {
        debug_assert_eq!(self.filled, FRAME_SIZE);
        for (ch, row) in out.iter_mut().enumerate() {
            for (n, dst) in row.iter_mut().enumerate() {
                *dst = self.ring[ch][(self.write_pos + n) % FRAME_SIZE];
            }
        }
    }

    /// 累计写入的总采样数（每通道）。
    pub fn total_samples(&self) -> u64 {
        self.total_samples
    }
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 递增序列，验证第一帧结束样本 = 512，后续 = 768、1024、1280……
    #[test]
    fn frame_boundaries() {
        let mut f = FrameAssembler::new();
        let mut emits: Vec<u64> = Vec::new();
        // 输入 2000 个采样，每通道值 = total index（递增，便于检查顺序）
        for n in 0..2000u64 {
            let s = n as f32;
            if f.push([s, s, s, s]) {
                emits.push(f.total_samples());
            }
        }
        assert_eq!(emits, vec![512, 768, 1024, 1280, 1536, 1792]);
    }

    /// 第一帧 512 点时间顺序正确（最旧 → 最新）。
    #[test]
    fn first_frame_chronological() {
        let mut f = FrameAssembler::new();
        let mut frame = [[0.0f32; FRAME_SIZE]; MIC_COUNT];
        // 第 511 个采样（n=510，累计 511）后不发射
        for n in 0..511u64 {
            let s = n as f32;
            assert!(!f.push([s, s, s, s]), "n={n} 不应发射");
        }
        // 第 512 个采样（n=511）后发射第一帧
        assert!(f.push([511.0f32; MIC_COUNT]));
        f.copy_frame(&mut frame);
        // 时间顺序递增：frame[0][0] == 0, frame[0][511] == 511
        assert_eq!(frame[0][0], 0.0);
        assert_eq!(frame[0][511], 511.0);
        for (n, &v) in frame[0].iter().enumerate() {
            assert!((v - n as f32).abs() < 1e-6);
        }
    }

    /// 任意块长一致性：同一段 4 路数据，一次输入 vs 逐帧输入，发射时机一致。
    #[test]
    fn block_size_independent_emits() {
        let n_samples = 3000;
        // 一次性输入
        let mut f_all = FrameAssembler::new();
        let mut emit_all = Vec::new();
        for n in 0..n_samples {
            let s = (n * 7 % 1000) as f32;
            if f_all.push([s, s + 1.0, s + 2.0, s + 3.0]) {
                emit_all.push(f_all.total_samples());
            }
        }
        // 不规则块长输入（循环块，直到累计满 n_samples）
        let mut f_block = FrameAssembler::new();
        let mut emit_block = Vec::new();
        let mut n = 0;
        let chunks = [17usize, 103, 7, 512, 31, 256, 900, 37, 100];
        'outer: loop {
            for chunk in chunks {
                for _ in 0..chunk {
                    if n >= n_samples {
                        break 'outer;
                    }
                    let s = (n * 7 % 1000) as f32;
                    if f_block.push([s, s + 1.0, s + 2.0, s + 3.0]) {
                        emit_block.push(f_block.total_samples());
                    }
                    n += 1;
                }
            }
        }
        assert_eq!(emit_all, emit_block);
    }
}
