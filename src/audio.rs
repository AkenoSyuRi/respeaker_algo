//! 音频设备枚举与输入流配置。
//!
//! ReSpeaker Mic Array v2.0 在 Windows/macOS/Linux 上以 USB Audio Class 1.0
//! 声卡形式出现，无需厂商驱动即可采集。本模块负责：
//! - 枚举所有输入设备（`list_input_devices`）
//! - 按索引 / 名称关键字 / 自动识别 ReSpeaker 选择设备（`pick_input_device`）
//! - 在设备上报的配置中查找目标采样率×通道数（`find_stream_config`）

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, SampleRate, SupportedStreamConfig};

/// 单个输入设备的信息（`index` 与 `list-devices` 输出中的编号一致）。
/// 仅在非 Windows 平台由 cpal 枚举使用。
#[cfg(not(windows))]
pub struct InputDeviceInfo {
    pub index: usize,
    pub name: String,
}

/// 枚举所有音频输入设备。
#[cfg(not(windows))]
pub fn list_input_devices() -> Result<Vec<InputDeviceInfo>, String> {
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .map_err(|e| format!("枚举输入设备失败: {e}"))?;
    let mut out = Vec::new();
    for (index, dev) in devices.enumerate() {
        let name = dev.name().unwrap_or_else(|_| "<未知设备名>".to_string());
        out.push(InputDeviceInfo { index, name });
    }
    Ok(out)
}

/// 打印设备支持的输入配置（调试用，对应 `list-devices --verbose`，非 Windows）。
#[cfg(not(windows))]
pub fn print_device_configs(device: &Device) {
    match device.supported_input_configs() {
        Ok(ranges) => {
            for r in ranges {
                println!(
                    "      ch={} rate={}-{} {:?}",
                    r.channels(),
                    r.min_sample_rate().0,
                    r.max_sample_rate().0,
                    r.sample_format()
                );
            }
        }
        Err(e) => println!("      (查询支持的配置失败: {e})"),
    }
}

/// 按设备在 `list_input_devices` 中的索引查找设备。
fn device_by_index(index: usize) -> Result<Device, String> {
    let host = cpal::default_host();
    host.input_devices()
        .map_err(|e| format!("枚举输入设备失败: {e}"))?
        .enumerate()
        .find(|(i, _)| *i == index)
        .map(|(_, d)| d)
        .ok_or_else(|| format!("索引 {index} 不存在，请先用 list-devices 查看可用设备"))
}

/// 按名称关键字（不区分大小写）查找第一个匹配的输入设备。
fn device_by_name(keyword: &str) -> Result<Device, String> {
    let host = cpal::default_host();
    let lower = keyword.to_lowercase();
    host.input_devices()
        .map_err(|e| format!("枚举输入设备失败: {e}"))?
        .find(|d| {
            d.name()
                .map(|n| n.to_lowercase().contains(&lower))
                .unwrap_or(false)
        })
        .ok_or_else(|| format!("没有名称包含 {:?} 的输入设备", keyword))
}

/// 选择输入设备。
///
/// - `spec == None`：优先自动选择名称含 "ReSpeaker" 的设备，否则回退到系统默认输入设备；
/// - `spec` 为纯数字：按 `list-devices` 中的索引选择；
/// - 其它字符串：按名称包含匹配（不区分大小写）。
pub fn pick_input_device(spec: Option<&str>) -> Result<Device, String> {
    let host = cpal::default_host();

    match spec {
        Some(s) if s.trim().parse::<usize>().is_ok() => {
            let idx = s.trim().parse::<usize>().unwrap();
            device_by_index(idx)
        }
        Some(s) => device_by_name(s.trim()),
        None => {
            // 自动识别：优先 ReSpeaker，其次系统默认输入设备。
            if let Ok(d) = device_by_name("ReSpeaker") {
                return Ok(d);
            }
            host.default_input_device().ok_or_else(|| {
                "未找到输入设备。请插入 ReSpeaker Mic Array 并用 list-devices 查看，".to_string()
            })
        }
    }
}

/// 在设备支持的输入配置中查找 `rate × channels`，找不到时回退到默认配置（并打印警告）。
pub fn find_stream_config(
    device: &Device,
    rate: u32,
    channels: u16,
) -> Result<SupportedStreamConfig, String> {
    let ranges = device
        .supported_input_configs()
        .map_err(|e| format!("查询设备支持的输入配置失败: {e}"))?;

    for range in ranges {
        if range.channels() != channels {
            continue;
        }
        if rate >= range.min_sample_rate().0 && rate <= range.max_sample_rate().0 {
            return Ok(range.with_sample_rate(SampleRate(rate)));
        }
    }

    // 回退：使用设备默认配置。
    let dflt = device
        .default_input_config()
        .map_err(|e| format!("获取设备默认输入配置失败: {e}"))?;
    eprintln!(
        "警告: 设备不支持 {rate}Hz/{channels}ch，回退到默认配置 {}Hz/{}ch",
        dflt.sample_rate().0,
        dflt.channels()
    );
    Ok(dflt)
}
