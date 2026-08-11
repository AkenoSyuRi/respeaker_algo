//! Windows WASAPI 独占模式采集后端（仅 Windows 编译）。
//!
//! Windows 声卡共享模式混音器会把该 UAC 设备降级为 48kHz/2ch，拿不到
//! `6_channels_firmware.bin` 声明的 16kHz/6ch。本程序只使用独占模式，以
//! `WAVEFORMATEXTENSIBLE`（16-bit PCM / 6ch / 16kHz）直接初始化
//! `IAudioClient`，获取 ch0 算法输出、ch1-4 原始 mic 和 ch5 回放参考。
//!
//! 基于 `wasapi` crate（wasapi-rs）封装，事件驱动独占模式：
//! `StreamMode::EventsExclusive { period_hns }`。
//!
//! 注意：独占模式下设备不能被其它程序占用（否则 `AUDCLNT_E_DEVICE_IN_USE`），
//! 且系统对该设备的音效处理（如增强）不生效。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::thread;

use wasapi::{
    AudioClient, DeviceCollection, Direction, SampleType, StreamMode, WasapiError, WaveFormat,
};

use crate::audio::CaptureBlock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureSendOutcome {
    Sent,
    ConsumerStopped,
}

fn try_send_capture_block(
    tx: &SyncSender<CaptureBlock>,
    block: CaptureBlock,
    stop: &AtomicBool,
) -> Result<CaptureSendOutcome, String> {
    match tx.try_send(block) {
        Ok(()) => Ok(CaptureSendOutcome::Sent),
        Err(TrySendError::Full(_)) => {
            stop.store(true, Ordering::SeqCst);
            Err("WASAPI capture queue overrun: consumer too slow".into())
        }
        Err(TrySendError::Disconnected(_)) => {
            if stop.load(Ordering::SeqCst) {
                Ok(CaptureSendOutcome::ConsumerStopped)
            } else {
                Err("capture consumer disconnected unexpectedly".into())
            }
        }
    }
}

/// 单个 WASAPI 输入设备的信息。
pub struct WasapiDeviceInfo {
    pub name: String,
    /// 设备 ID（跨线程/进程稳定，用于采集线程重新定位设备）。
    pub id: String,
}

/// 初始化 COM（MTA）。返回错误字符串，成功返回 Ok(())。
fn init_com() -> Result<(), String> {
    let hr = wasapi::initialize_mta();
    if hr.0 >= 0 {
        Ok(())
    } else {
        Err(format!("COM 初始化失败: HRESULT={:#x}", hr.0 as u32))
    }
}

/// 枚举所有活动的 WASAPI 输入设备，用于自动查找 ReSpeaker。
fn list_input_devices() -> Result<Vec<WasapiDeviceInfo>, String> {
    init_com()?;
    let collection = DeviceCollection::new(&Direction::Capture)
        .map_err(|e| format!("枚举 WASAPI 输入设备失败: {e}"))?;
    let count = collection
        .get_nbr_devices()
        .map_err(|e| format!("获取设备数量失败: {e}"))?;
    let mut out = Vec::new();
    for index in 0..count {
        let dev = collection
            .get_device_at_index(index)
            .map_err(|e| format!("枚举设备 {index} 失败: {e}"))?;
        let name = dev
            .get_friendlyname()
            .map_err(|e| format!("获取设备 {index} 名称失败: {e}"))?;
        let id = dev.get_id().unwrap_or_else(|_| "<无ID>".to_string());
        out.push(WasapiDeviceInfo { name, id });
    }
    Ok(out)
}

/// 自动选择名称包含 `ReSpeaker` 的输入设备。
pub fn pick_respeaker_input_device() -> Result<WasapiDeviceInfo, String> {
    let all = list_input_devices()?;
    if all.is_empty() {
        return Err("未找到任何音频输入设备".into());
    }
    all.into_iter()
        .find(|d| d.name.to_lowercase().contains("respeaker"))
        .ok_or_else(|| "未找到名称包含 ReSpeaker 的输入设备；请确认设备已连接".into())
}

/// 采集会话句柄：Drop 时置停止标志并等待采集线程退出。
pub struct WasapiSession {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<Result<(), String>>>,
}

impl WasapiSession {
    /// 正常路径：置 stop、join，并返回采集线程结果。
    pub fn stop_and_join(&mut self) -> Result<(), String> {
        self.stop.store(true, Ordering::SeqCst);
        match self.join.take() {
            Some(join) => match join.join() {
                Ok(result) => result,
                Err(_) => Err("WASAPI 采集线程 panic".into()),
            },
            None => Ok(()),
        }
    }
}

impl Drop for WasapiSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// 启动 WASAPI 独占模式采集（在独立线程中运行）。
pub fn start_capture(
    dev: WasapiDeviceInfo,
    rate: u32,
    channels: u16,
    tx: SyncSender<CaptureBlock>,
    stop: Arc<AtomicBool>,
) -> Result<WasapiSession, String> {
    let id = dev.id;
    let name = dev.name;
    let stop_thread = Arc::clone(&stop);
    let join = thread::Builder::new()
        .name("wasapi-capture".into())
        .spawn(move || run_capture(&id, &name, rate, channels, tx, stop_thread))
        .map_err(|e| format!("创建采集线程失败: {e}"))?;
    Ok(WasapiSession {
        stop,
        join: Some(join),
    })
}

fn run_capture(
    id: &str,
    name: &str,
    rate: u32,
    channels: u16,
    tx: SyncSender<CaptureBlock>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    init_com()?;

    // 按稳定设备 ID 重新定位设备（COM 对象不跨线程共享）。
    let collection = DeviceCollection::new(&Direction::Capture)
        .map_err(|e| format!("枚举 WASAPI 设备失败: {e}"))?;
    let count = collection
        .get_nbr_devices()
        .map_err(|e| format!("获取设备数量失败: {e}"))?;
    let mut client: Option<AudioClient> = None;
    for index in 0..count {
        let dev = collection
            .get_device_at_index(index)
            .map_err(|e| format!("枚举设备失败: {e}"))?;
        if dev.get_id().map(|x| x == id).unwrap_or(false) {
            client = Some(dev.get_iaudioclient().map_err(|e| format!("{e}"))?);
            break;
        }
    }
    let mut audio_client = client.ok_or_else(|| format!("设备 {name} 已断开"))?;

    // 16-bit PCM 多通道格式。
    // 注意：该设备（usbaudio.sys）在独占模式下只接受 channel mask = 0；
    // 显式指定 5.1 布局（0x3F）会被拒（AUDCLNT_E_UNSUPPORTED_FORMAT），
    // 且 wasapi::WaveFormat::new 对 None 会自动生成 0x3F，因此必须传 Some(0)。
    // mask 仅影响 Windows 端声道布局解释，不影响 6 通道原始数据的顺序。
    let format = WaveFormat::new(
        16,
        16,
        &SampleType::Int,
        rate as usize,
        channels as usize,
        Some(0),
    );

    // 事件驱动独占（EventsExclusive）要求 buffer == period，3ms 缓冲对该
    // 设备过小（实测事件节拍异常、无数据）；改用轮询独占：buffer 与 period
    // 分离，100ms 缓冲 + 轮询读包。
    let (default_period, min_period) = audio_client
        .get_device_period()
        .map_err(|e| format!("获取设备周期失败: {e}"))?;
    let period_hns = if min_period > 0 {
        min_period
    } else {
        default_period
    };
    let buffer_hns: i64 = 1_000_000; // 100ms

    audio_client
        .initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::PollingExclusive {
                buffer_duration_hns: buffer_hns,
                period_hns,
            },
        )
        .map_err(|e| match &e {
            WasapiError::Windows(w) if w.code().0 as u32 == 0x8889_000A => format!(
                "设备 {name} 正被其它程序占用（独占模式要求独占访问）。\n请关闭占用它的程序（如录音软件、系统默认监听）后重试。"
            ),
            _ => format!(
                "AudioClient 独占初始化失败（{rate}Hz/{channels}ch）: {e}\n该设备可能不支持此格式，或需先释放设备占用。"
            ),
        })?;

    let capture = audio_client
        .get_audiocaptureclient()
        .map_err(|e| format!("获取采集客户端失败: {e}"))?;
    let buffer_frames = audio_client
        .get_buffer_size()
        .map_err(|e| format!("获取缓冲帧数失败: {e}"))?;
    audio_client
        .start_stream()
        .map_err(|e| format!("启动音频流失败: {e}"))?;
    println!("WASAPI 独占采集已启动: {name}（{rate}Hz/{channels}ch，buffer={buffer_frames} 帧）");

    let frame_bytes = channels as usize * 2;
    // 独占模式下 GetNextPacketSize 不可用（wasapi-rs 直接返回 None），
    // 直接 read_from_device：返回的帧数为 0 表示当前无更多数据。
    let mut data = vec![0u8; buffer_frames as usize * frame_bytes];
    let mut sequence = 0u64;
    let mut total_frames = 0u64;

    // 轮询模式：周期取包，1ms 空闲间隔。
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        loop {
            let (frames, flags) = match capture.read_from_device(&mut data) {
                Ok(v) => v,
                Err(e) => {
                    let _ = audio_client.stop_stream();
                    return Err(format!("读取采集缓冲失败: {e}"));
                }
            };
            if frames == 0 {
                break;
            }
            let sample_count = frames as usize * channels as usize;
            let samples = if flags.silent {
                vec![0i16; sample_count]
            } else {
                let bytes_read = frames as usize * frame_bytes;
                data[..bytes_read]
                    .chunks_exact(2)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]))
                    .collect()
            };
            let block = CaptureBlock::from_samples(sequence, total_frames, samples)?;
            sequence += 1;
            total_frames += frames as u64;
            match try_send_capture_block(&tx, block, &stop) {
                Ok(CaptureSendOutcome::Sent) => {}
                Ok(CaptureSendOutcome::ConsumerStopped) => break,
                Err(error) => {
                    let _ = audio_client.stop_stream();
                    return Err(error);
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    let _ = audio_client.stop_stream();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    fn silent_block(sequence: u64) -> CaptureBlock {
        CaptureBlock::from_samples(sequence, sequence, vec![0i16; 6]).unwrap()
    }

    #[test]
    fn capture_overrun_remains_fatal() {
        let (tx, _rx) = sync_channel(1);
        let stop = AtomicBool::new(false);
        assert_eq!(
            try_send_capture_block(&tx, silent_block(0), &stop).unwrap(),
            CaptureSendOutcome::Sent
        );
        let error = try_send_capture_block(&tx, silent_block(1), &stop).unwrap_err();
        assert!(error.contains("overrun"), "{error}");
        assert!(stop.load(Ordering::SeqCst));
    }

    #[test]
    fn unexpected_capture_disconnect_is_an_error() {
        let (tx, rx) = sync_channel(1);
        drop(rx);
        let stop = AtomicBool::new(false);
        let error = try_send_capture_block(&tx, silent_block(0), &stop).unwrap_err();
        assert!(error.contains("disconnected"), "{error}");
        assert!(!stop.load(Ordering::SeqCst));
    }

    #[test]
    fn capture_disconnect_after_stop_is_normal() {
        let (tx, rx) = sync_channel(1);
        drop(rx);
        let stop = AtomicBool::new(true);
        assert_eq!(
            try_send_capture_block(&tx, silent_block(0), &stop).unwrap(),
            CaptureSendOutcome::ConsumerStopped
        );
    }
}
