//! 应用配置、持久化 profile 和内置 preset。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::pipeline::PipelineConfig;

pub const APP_CONFIG_VERSION: u32 = 1;
const MAX_PROFILE_NAME_LEN: usize = 64;

#[cfg(test)]
static TEST_CONFIG_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingConfig {
    #[serde(default)]
    pub duration_seconds: u64,
    #[serde(default = "default_out_dir")]
    pub out_dir: String,
    #[serde(default)]
    pub prefix: Option<String>,
}

fn default_out_dir() -> String {
    "recordings".to_string()
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            duration_seconds: 0,
            out_dir: default_out_dir(),
            prefix: None,
        }
    }
}

impl RecordingConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.out_dir.trim().is_empty() || self.out_dir.contains('\0') {
            return Err("out_dir 不能为空且不得包含 NUL 字符".into());
        }
        if let Some(prefix) = &self.prefix {
            validate_prefix(prefix)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default = "default_app_version")]
    pub version: u32,
    #[serde(default)]
    pub recording: RecordingConfig,
    #[serde(default)]
    pub pipeline_enabled: bool,
    #[serde(default)]
    pub pipeline: PipelineConfig,
}

fn default_app_version() -> u32 {
    APP_CONFIG_VERSION
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: APP_CONFIG_VERSION,
            recording: RecordingConfig::default(),
            pipeline_enabled: false,
            pipeline: PipelineConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != APP_CONFIG_VERSION {
            return Err(format!(
                "不支持的 App 配置版本 {}，当前仅支持 {}",
                self.version, APP_CONFIG_VERSION
            ));
        }
        self.recording.validate()?;
        self.pipeline.validate()
    }

    pub fn parse_toml(text: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        config.validate()?;
        Ok(config)
    }

    pub fn to_toml_string(&self) -> Result<String, String> {
        self.validate()?;
        toml::to_string_pretty(self).map_err(|e| format!("序列化 AppConfig 失败: {e}"))
    }
}

pub fn app_config_dir() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(path) = TEST_CONFIG_DIR
        .lock()
        .expect("测试配置目录锁 poisoned")
        .clone()
    {
        return Ok(path);
    }
    let local_app_data = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| "未找到 LOCALAPPDATA 环境变量，无法确定配置目录".to_string())?;
    if local_app_data.is_empty() {
        return Err("LOCALAPPDATA 为空".into());
    }
    Ok(PathBuf::from(local_app_data).join("respeaker_algo"))
}

#[cfg(test)]
pub(crate) fn set_test_config_dir(path: Option<PathBuf>) {
    *TEST_CONFIG_DIR.lock().expect("测试配置目录锁 poisoned") = path;
}

pub fn app_config_path() -> Result<PathBuf, String> {
    Ok(app_config_dir()?.join("config.toml"))
}

pub fn profiles_dir() -> Result<PathBuf, String> {
    Ok(app_config_dir()?.join("profiles"))
}

/// 将 UTF-8 内容写入同目录临时文件并替换目标。
///
/// Windows 的标准 `fs::rename` 不会替换已存在文件，因此使用
/// `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)` 保持覆盖写的原子性。
pub fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("路径 {} 无父目录", path.display()))?;
    fs::create_dir_all(dir).map_err(|e| format!("创建目录 {} 失败: {e}", dir.display()))?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("路径 {} 文件名无效", path.display()))?;
    let tmp = dir.join(format!(".{file_name}.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| format!("创建临时文件 {} 失败: {e}", tmp.display()))?;
    if let Err(error) = file
        .write_all(content.as_bytes())
        .and_then(|_| file.sync_all())
    {
        let _ = fs::remove_file(&tmp);
        return Err(format!("写入临时文件 {} 失败: {error}", tmp.display()));
    }
    drop(file);

    let result = replace_file(&tmp, path);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(windows)]
fn replace_file(tmp: &Path, target: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = tmp.as_os_str().encode_wide().chain([0]).collect();
    let destination: Vec<u16> = target.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: 两个 UTF-16 缓冲区均以 NUL 结尾，调用只读取其内容。
    let ok = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(format!(
            "原子替换 {} -> {} 失败: {}",
            tmp.display(),
            target.display(),
            io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(tmp: &Path, target: &Path) -> Result<(), String> {
    fs::rename(tmp, target).map_err(|e| {
        format!(
            "原子替换 {} -> {} 失败: {e}",
            tmp.display(),
            target.display()
        )
    })
}

pub fn validate_profile_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_PROFILE_NAME_LEN {
        return Err(format!("profile 名称长度必须在 1..={MAX_PROFILE_NAME_LEN}"));
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err("profile 名称不得包含 ..、路径分隔符或 NUL 字符".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Err("profile 名称只允许 ASCII 字母、数字、_、-".into());
    }
    Ok(())
}

pub fn profile_path(name: &str) -> Result<PathBuf, String> {
    validate_profile_name(name)?;
    Ok(profiles_dir()?.join(format!("{name}.toml")))
}

pub fn load_or_default() -> (AppConfig, Option<String>) {
    let path = match app_config_path() {
        Ok(path) => path,
        Err(error) => return (AppConfig::default(), Some(error)),
    };
    if !path.exists() {
        return (AppConfig::default(), None);
    }
    match fs::read_to_string(&path) {
        Ok(text) => match AppConfig::parse_toml(&text) {
            Ok(config) => (config, None),
            Err(error) => (
                AppConfig::default(),
                Some(format!("配置解析失败 {}: {error}", path.display())),
            ),
        },
        Err(error) => (
            AppConfig::default(),
            Some(format!("读取配置 {} 失败: {error}", path.display())),
        ),
    }
}

pub fn save_config(config: &AppConfig) -> Result<(), String> {
    atomic_write(&app_config_path()?, &config.to_toml_string()?)
}

pub fn save_profile(name: &str, config: &AppConfig) -> Result<(), String> {
    atomic_write(&profile_path(name)?, &config.to_toml_string()?)
}

pub fn load_profile(name: &str) -> Result<AppConfig, String> {
    let path = profile_path(name)?;
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("读取 profile {} 失败: {e}", path.display()))?;
    AppConfig::parse_toml(&text)
}

pub fn delete_profile(name: &str) -> Result<(), String> {
    let path = profile_path(name)?;
    fs::remove_file(&path).map_err(|e| format!("删除 profile {} 失败: {e}", path.display()))
}

pub fn list_profiles() -> Result<Vec<String>, String> {
    let dir = profiles_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| format!("读取 profiles 目录失败: {e}"))? {
        let path = entry
            .map_err(|e| format!("读取 profile 目录项失败: {e}"))?
            .path();
        if path.extension().and_then(|value| value.to_str()) != Some("toml") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|value| value.to_str())
            && validate_profile_name(stem).is_ok()
        {
            names.push(stem.to_string());
        }
    }
    names.sort();
    Ok(names)
}

pub fn validate_prefix(prefix: &str) -> Result<(), String> {
    if prefix.is_empty() || prefix.len() > 128 {
        return Err("prefix 长度必须在 1..=128".into());
    }
    if prefix.contains("..")
        || prefix.contains('/')
        || prefix.contains('\\')
        || prefix.contains('\0')
    {
        return Err("prefix 不得包含 ..、路径分隔符或 NUL 字符".into());
    }
    if !prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Err("prefix 只允许 ASCII 字母、数字、_、-".into());
    }
    Ok(())
}

pub fn preset_pure() -> AppConfig {
    AppConfig::default()
}

pub fn preset_doa() -> Result<AppConfig, String> {
    Ok(AppConfig {
        pipeline_enabled: true,
        pipeline: PipelineConfig::parse_toml(include_str!("../configs/doa.toml"))?,
        ..AppConfig::default()
    })
}

pub fn preset_doa_bf() -> Result<AppConfig, String> {
    Ok(AppConfig {
        pipeline_enabled: true,
        pipeline: PipelineConfig::parse_toml(include_str!("../configs/doa_bf.toml"))?,
        ..AppConfig::default()
    })
}

pub fn preset_bf_fixed() -> Result<AppConfig, String> {
    Ok(AppConfig {
        pipeline_enabled: true,
        pipeline: PipelineConfig::parse_toml(include_str!("../configs/bf_fixed.toml"))?,
        ..AppConfig::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_pure_recording() {
        let config = AppConfig::default();
        assert_eq!(config.version, APP_CONFIG_VERSION);
        assert!(!config.pipeline_enabled);
        config.validate().unwrap();
        assert_eq!(
            AppConfig::parse_toml(&config.to_toml_string().unwrap())
                .unwrap()
                .version,
            1
        );
    }

    #[test]
    fn omitted_enable_drc_defaults_to_true() {
        let config = PipelineConfig::parse_toml(
            "version = 1\n[[modules]]\ntype = \"beamformer\"\ndirection_source = \"fixed\"",
        )
        .unwrap();
        let json = serde_json::to_value(config).unwrap();
        assert_eq!(json["modules"][0]["enable_drc"], true);
    }

    #[test]
    fn invalid_order_is_rejected_without_panicking() {
        let text = "version = 1\n[[modules]]\ntype = \"beamformer\"\ndirection_source = \"doa\"\n[[modules]]\ntype = \"doa\"";
        assert!(PipelineConfig::parse_toml(text).is_err());
    }

    #[test]
    fn unknown_nan_and_inf_values_are_rejected() {
        assert!(AppConfig::parse_toml("version = 1\nunknown = 1").is_err());
        assert!(AppConfig::parse_toml(
            "version = 1\n[pipeline]\nversion = 1\n[[pipeline.modules]]\ntype = \"doa\"\nbeta = nan"
        )
        .is_err());
        assert!(AppConfig::parse_toml(
            "version = 1\n[pipeline]\nversion = 1\n[[pipeline.modules]]\ntype = \"doa\"\nbeta = inf"
        )
        .is_err());
    }

    #[test]
    fn import_export_roundtrip_keeps_fields() {
        let original = preset_doa_bf().unwrap();
        let imported = AppConfig::parse_toml(&original.to_toml_string().unwrap()).unwrap();
        assert_eq!(original.pipeline_enabled, imported.pipeline_enabled);
        assert_eq!(original.recording.out_dir, imported.recording.out_dir);
        assert_eq!(
            serde_json::to_value(original).unwrap(),
            serde_json::to_value(imported).unwrap()
        );
    }

    #[test]
    fn profile_names_reject_path_traversal() {
        for name in ["", "../evil", "a/b", "a\\b", "a..b", "a b", "a.toml"] {
            assert!(validate_profile_name(name).is_err(), "{name}");
        }
        assert!(validate_profile_name("valid_name-123").is_ok());
    }

    #[test]
    fn presets_are_valid() {
        preset_pure().validate().unwrap();
        preset_doa().unwrap().validate().unwrap();
        preset_doa_bf().unwrap().validate().unwrap();
        preset_bf_fixed().unwrap().validate().unwrap();
    }

    #[test]
    fn atomic_write_replaces_existing_file() {
        let path =
            std::env::temp_dir().join(format!("respeaker_atomic_{}.txt", std::process::id()));
        fs::write(&path, "old").unwrap();
        atomic_write(&path, "new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        let _ = fs::remove_file(path);
    }
}
