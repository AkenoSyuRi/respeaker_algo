//! Windows WASAPI 独占模式采集后端（仅 Windows 编译）。
//!
//! 背景：Windows 声卡共享模式混音器会把 UAC 设备降级为 48kHz/2ch（cpal
//! WASAPI 后端即共享模式），拿不到 `6_channels_firmware.bin` 声明的
//! 16kHz/6ch。独占模式用 `WAVEFORMATEXTENSIBLE`（16-bit PCM / 6ch / 16kHz）
//! 直接初始化 `IAudioClient`，可拿到完整 6 通道原始数据（ch0 算法输出、
//! ch1-4 mic 原始、ch5 回放），供后续 DOA / BF 使用。
//!
//! 基于 `wasapi` crate（wasapi-rs）封装，事件驱动独占模式：
//! `StreamMode::EventsExclusive { period_hns }`。
//!
//! 注意：独占模式下设备不能被其它程序占用（否则 `AUDCLNT_E_DEVICE_IN_USE`），
//! 且系统对该设备的音效处理（如增强）不生效。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::thread;

use wasapi::{
    AudioClient, DeviceCollection, Direction, SampleType, StreamMode, WasapiError, WaveFormat,
};

/// 单个 WASAPI 输入设备的信息。
pub struct WasapiDeviceInfo {
    pub index: usize,
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

/// 枚举所有活动的输入设备（WASAPI 视图，索引与 list-devices 的编号一致）。
pub fn list_devices() -> Result<Vec<WasapiDeviceInfo>, String> {
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
        out.push(WasapiDeviceInfo {
            index: index as usize,
            name,
            id,
        });
    }
    Ok(out)
}

/// 按 `spec` 选择设备：None → 自动识别 ReSpeaker；数字 → 索引；其它 → 名称包含匹配。
pub fn pick_input_device(spec: Option<&str>) -> Result<WasapiDeviceInfo, String> {
    let all = list_devices()?;
    if all.is_empty() {
        return Err("未找到任何音频输入设备".into());
    }
    let matched = match spec {
        Some(s) if s.trim().parse::<usize>().is_ok() => {
            let idx = s.trim().parse::<usize>().unwrap();
            all.iter().find(|d| d.index == idx).map(|d| d.name.clone())
        }
        Some(s) => {
            let lower = s.to_lowercase();
            all.iter()
                .find(|d| d.name.to_lowercase().contains(&lower))
                .map(|d| d.name.clone())
        }
        None => all
            .iter()
            .find(|d| d.name.contains("ReSpeaker"))
            .map(|d| d.name.clone()),
    };
    let Some(name) = matched else {
        let hint = if spec.is_none() {
            "；请用 list-devices 查看可用设备并用 --device 指定"
        } else {
            ""
        };
        return Err(format!("未找到匹配的输入设备: {:?}{}", spec, hint));
    };
    all.into_iter()
        .find(|d| d.name == name)
        .ok_or_else(|| format!("设备 {name} 已消失"))
}

/// 采集会话句柄：Drop 时置停止标志并等待采集线程退出。
pub struct WasapiSession {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
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
    tx: SyncSender<Vec<i16>>,
    stop: Arc<AtomicBool>,
) -> Result<WasapiSession, String> {
    let id = dev.id;
    let name = dev.name;
    let stop_thread = Arc::clone(&stop);
    let join = thread::Builder::new()
        .name("wasapi-capture".into())
        .spawn(move || {
            if let Err(e) = run_capture(&id, &name, rate, channels, tx, stop_thread) {
                eprintln!("WASAPI 采集错误: {e}");
            }
        })
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
    tx: SyncSender<Vec<i16>>,
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
    let period_hns = if min_period > 0 { min_period } else { default_period };
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
    println!(
        "WASAPI 独占采集已启动: {name}（{rate}Hz/{channels}ch，buffer={buffer_frames} 帧）"
    );

    let frame_bytes = channels as usize * 2;
    // 独占模式下 GetNextPacketSize 不可用（wasapi-rs 直接返回 None），
    // 直接 read_from_device：返回的帧数为 0 表示当前无更多数据。
    let mut data = vec![0u8; buffer_frames as usize * frame_bytes];
    let mut result: Result<(), String> = Ok(());

    // 轮询模式：周期取包，1ms 空闲间隔。
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        loop {
            let (frames, flags) = match capture.read_from_device(&mut data) {
                Ok(v) => v,
                Err(e) => {
                    result = Err(format!("读取采集缓冲失败: {e}"));
                    break;
                }
            };
            if frames == 0 {
                break;
            }
            let sample_count = frames as usize * channels as usize;
            if flags.silent {
                let _ = tx.send(vec![0i16; sample_count]);
            } else {
                let bytes_read = frames as usize * frame_bytes;
                let samples: Vec<i16> = data[..bytes_read]
                    .chunks_exact(2)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let _ = tx.send(samples);
            }
        }
        if result.is_err() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    let _ = audio_client.stop_stream();
    result
}
