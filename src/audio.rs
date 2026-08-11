//! 跨采集、录音与算法 worker 共用的块类型与设备常量。

use std::sync::Arc;

pub const RESPEAKER_SAMPLE_RATE: u32 = 16_000;
pub const RESPEAKER_CHANNELS: usize = 6;
pub const RESPEAKER_MIC_CHANNELS: usize = 4;
pub const CAPTURE_QUEUE_CAPACITY: usize = 128;

/// 一次 WASAPI 读包对应的有序 6ch PCM 块；可经 `Arc` 廉价克隆。
#[derive(Clone, Debug)]
pub struct CaptureBlock {
    pub sequence: u64,
    pub start_frame: u64,
    pub frames: usize,
    pub samples: Arc<[i16]>,
}

impl CaptureBlock {
    pub fn from_samples(
        sequence: u64,
        start_frame: u64,
        samples: Vec<i16>,
    ) -> Result<Self, String> {
        if !samples.len().is_multiple_of(RESPEAKER_CHANNELS) {
            return Err(format!(
                "采样块长度 {} 不是 {} 的整数倍",
                samples.len(),
                RESPEAKER_CHANNELS
            ));
        }
        let frames = samples.len() / RESPEAKER_CHANNELS;
        let block = Self {
            sequence,
            start_frame,
            frames,
            samples: samples.into(),
        };
        block.validate()?;
        Ok(block)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.samples.len() != self.frames * RESPEAKER_CHANNELS {
            return Err(format!(
                "CaptureBlock 长度不匹配: samples={}, frames={}, channels={}",
                self.samples.len(),
                self.frames,
                RESPEAKER_CHANNELS
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_block_validates_length() {
        let block = CaptureBlock::from_samples(0, 0, vec![0i16; 12]).unwrap();
        assert_eq!(block.frames, 2);
        block.validate().unwrap();
    }

    #[test]
    fn capture_block_rejects_bad_length() {
        assert!(CaptureBlock::from_samples(0, 0, vec![0i16; 10]).is_err());
        let bad = CaptureBlock {
            sequence: 0,
            start_frame: 0,
            frames: 2,
            samples: Arc::from(vec![0i16; 10]),
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn capture_sequence_and_start_frame_are_contiguous() {
        let mut total_frames = 0u64;
        let packet_frames = [3usize, 5, 1];
        let mut blocks = Vec::new();
        for (sequence, &frames) in packet_frames.iter().enumerate() {
            let samples = vec![0i16; frames * RESPEAKER_CHANNELS];
            let block = CaptureBlock::from_samples(sequence as u64, total_frames, samples).unwrap();
            blocks.push(block);
            total_frames += frames as u64;
        }
        assert_eq!(blocks[0].sequence, 0);
        assert_eq!(blocks[0].start_frame, 0);
        assert_eq!(blocks[1].sequence, 1);
        assert_eq!(blocks[1].start_frame, 3);
        assert_eq!(blocks[2].sequence, 2);
        assert_eq!(blocks[2].start_frame, 8);
        assert_eq!(total_frames, 9);
    }

    #[test]
    fn capture_queue_capacity_remains_128() {
        assert_eq!(CAPTURE_QUEUE_CAPACITY, 128);
    }
}
