//! 流式 WAV 写入器（16-bit PCM，标准 `WAVEFORMATEX`）。
//!
//! 与 hound 的区别：hound 对 channels > 2 会写 `WAVEFORMATEXTENSIBLE` 并把
//! `dwChannelMask` 设为按位递增的布局（如 4 通道 = 0xF = FL/FR/FC/LFE），
//! 导致播放器把麦克风通道显示为 "L/R/C/LFE"。本写入器只写标准
//! `WAVEFORMATEX`（`wFormatTag = WAVE_FORMAT_PCM`，`cbSize = 0`），不含任何
//! 通道布局元信息，播放器会以数字 1..N 显示通道。

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};

/// 44 字节标准 WAV 头（WAVEFORMATEX PCM，无 extensible）。
const HEADER_LEN: u64 = 44;

pub struct WavSink {
    writer: BufWriter<File>,
    data_bytes: u64,
    finalized: bool,
}

impl WavSink {
    /// 创建文件并写入 WAV 头（data 长度占位，`finalize` 时回填）。
    pub fn create(path: &str, channels: u16, sample_rate: u32) -> Result<Self, String> {
        let file = File::create(path).map_err(|e| format!("创建 {} 失败: {e}", path))?;
        let mut header = [0u8; HEADER_LEN as usize];
        header[0..4].copy_from_slice(b"RIFF");
        header[8..12].copy_from_slice(b"WAVE");
        header[12..16].copy_from_slice(b"fmt ");
        header[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt chunk 大小（无 cbSize）
        header[20..22].copy_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
        header[22..24].copy_from_slice(&channels.to_le_bytes());
        header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
        let block_align = channels as u32 * 2; // 16-bit
        header[28..32].copy_from_slice(&(sample_rate * block_align).to_le_bytes());
        header[32..34].copy_from_slice(&(block_align as u16).to_le_bytes());
        header[34..36].copy_from_slice(&16u16.to_le_bytes()); // wBitsPerSample
        header[36..40].copy_from_slice(b"data");
        // data 长度（40..44）占位为 0，finalize 时回填

        let mut writer = BufWriter::new(file);
        writer
            .write_all(&header)
            .map_err(|e| format!("写入 {} 头失败: {e}", path))?;
        Ok(WavSink {
            writer,
            data_bytes: 0,
            finalized: false,
        })
    }

    /// 写入一个样本（交错顺序）。
    pub fn write_sample(&mut self, sample: i16) -> Result<(), String> {
        self.writer
            .write_all(&sample.to_le_bytes())
            .map_err(|e| format!("写入样本失败: {e}"))?;
        self.data_bytes += 2;
        Ok(())
    }

    /// 批量写入样本（交错顺序）。
    pub fn write_samples(&mut self, samples: &[i16]) -> Result<(), String> {
        for &s in samples {
            self.write_sample(s)?;
        }
        Ok(())
    }

    /// 回填 RIFF/data 长度并刷新。可重复调用。
    pub fn finalize(&mut self) -> Result<(), String> {
        if self.finalized {
            return Ok(());
        }
        let data_len = self.data_bytes;
        if data_len > u32::MAX as u64 {
            return Err(format!(
                "WAV 数据超过 4GB（{} bytes），标准 WAV 无法表示，请分段时间录制",
                data_len
            ));
        }
        // RIFF size = 36 + data 长度（从 offset 8 起算）
        self.writer
            .seek(SeekFrom::Start(4))
            .map_err(|e| format!("定位文件头失败: {e}"))?;
        self.writer
            .write_all(&(36 + data_len as u32).to_le_bytes())
            .map_err(|e| format!("回填 RIFF 长度失败: {e}"))?;
        self.writer
            .seek(SeekFrom::Start(HEADER_LEN - 4))
            .map_err(|e| format!("定位 data 长度失败: {e}"))?;
        self.writer
            .write_all(&(data_len as u32).to_le_bytes())
            .map_err(|e| format!("回填 data 长度失败: {e}"))?;
        self.writer
            .flush()
            .map_err(|e| format!("刷新文件失败: {e}"))?;
        self.finalized = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_wav(name: &str) -> String {
        std::env::temp_dir()
            .join(format!("respeaker_wavsink_test_{}_{name}", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }

    /// 写入并用 hound 读回验证数据一致。
    #[test]
    fn roundtrip_readable() {
        let path = temp_wav("rt.wav");
        {
            let mut w = WavSink::create(&path, 4, 16000).unwrap();
            for k in 0..4i16 {
                for c in 0..4i16 {
                    w.write_sample(k * 10 + c).unwrap();
                }
            }
            w.finalize().unwrap();
        }
        let mut r = hound::WavReader::open(&path).unwrap();
        let spec = r.spec();
        assert_eq!(spec.channels, 4);
        assert_eq!(spec.sample_rate, 16000);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, hound::SampleFormat::Int);
        let got: Vec<i16> = r.samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(got, vec![0, 1, 2, 3, 10, 11, 12, 13, 20, 21, 22, 23, 30, 31, 32, 33]);
        std::fs::remove_file(&path).ok();
    }

    /// 多通道文件必须使用标准 WAVEFORMATEX（PCM, fmt=16, 无 cbSize），
    /// 即不含 WAVEFORMATEXTENSIBLE / dwChannelMask，避免 L/R/C/LFE 布局。
    #[test]
    fn header_is_plain_waveformatex() {
        let path = temp_wav("hdr.wav");
        {
            let mut w = WavSink::create(&path, 4, 16000).unwrap();
            w.write_samples(&[1, 2, 3, 4]).unwrap();
            w.finalize().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        // fmt chunk 大小 = 16（无 WAVEFORMATEXTENSIBLE 扩展）
        assert_eq!(&bytes[16..20], &16u32.to_le_bytes());
        // wFormatTag @20 == 1 (WAVE_FORMAT_PCM)
        assert_eq!(&bytes[20..22], &[1, 0]);
        // data chunk 紧跟 fmt
        assert_eq!(&bytes[36..40], b"data");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn finalize_lengths_correct() {
        let path = temp_wav("len.wav");
        {
            let mut w = WavSink::create(&path, 2, 16000).unwrap();
            w.write_samples(&[7, 8, 9, 10]).unwrap(); // 2 帧 × 2ch × 2B = 8 bytes
            w.finalize().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), HEADER_LEN as usize + 8);
        // RIFF size = 36 + 8 = 44
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 44);
        // data size = 8（@40..44）
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 8);
        std::fs::remove_file(&path).ok();
    }
}
