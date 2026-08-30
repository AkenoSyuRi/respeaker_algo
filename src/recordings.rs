//! 录音 session manifest、历史目录扫描和安全文件操作。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app_config::AppConfig;
use crate::beamformer::BeamformerStats;
use crate::events::ServiceError;
use crate::pipeline::ModuleConfig;
use crate::pipeline_worker::PipelineWorkerStats;

const MANIFEST_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionResultStatus {
    Starting,
    Recording,
    Success,
    Failed,
    Interrupted,
    Legacy,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionFile {
    pub kind: String,
    /// 仅保存文件名，不向客户端暴露绝对路径。
    pub path: String,
    pub bytes: u64,
    pub exists: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionManifest {
    pub schema_version: u32,
    pub session_id: String,
    pub prefix: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: SessionResultStatus,
    pub active_config: AppConfig,
    pub captured_frames: u64,
    pub pipeline_stats: Option<PipelineWorkerStats>,
    pub bf_stats: Option<BeamformerStats>,
    pub error: Option<ServiceError>,
    pub files: Vec<SessionFile>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecordingEntry {
    pub id: String,
    pub prefix: String,
    pub status: SessionResultStatus,
    pub manifest: Option<SessionManifest>,
    pub files: Vec<SessionFile>,
}

impl SessionManifest {
    pub fn new(
        session_id: String,
        prefix: String,
        started_at: String,
        active_config: AppConfig,
    ) -> Self {
        Self {
            schema_version: MANIFEST_VERSION,
            session_id,
            prefix,
            started_at,
            finished_at: None,
            status: SessionResultStatus::Starting,
            active_config,
            captured_frames: 0,
            pipeline_stats: None,
            bf_stats: None,
            error: None,
            files: Vec::new(),
        }
    }

    pub fn manifest_path(out_dir: &str, prefix: &str) -> PathBuf {
        PathBuf::from(out_dir).join(format!("{prefix}_respeaker_session.json"))
    }

    pub fn refresh_files(&mut self, out_dir: &str) {
        self.files = ALL_KINDS
            .iter()
            .filter_map(|kind| {
                let name = kind_to_filename(&self.prefix, kind)?;
                let path = PathBuf::from(out_dir).join(&name);
                Some(SessionFile {
                    kind: (*kind).to_string(),
                    path: name,
                    bytes: fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                    exists: path.is_file(),
                })
            })
            .collect();
    }

    pub fn save_atomic(&self, out_dir: &str) -> Result<(), String> {
        let path = Self::manifest_path(out_dir, &self.prefix);
        let json =
            serde_json::to_string_pretty(self).map_err(|e| format!("序列化 manifest 失败: {e}"))?;
        crate::app_config::atomic_write(&path, &json)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("读取 manifest {} 失败: {e}", path.display()))?;
        serde_json::from_str(&text)
            .map_err(|e| format!("解析 manifest {} 失败: {e}", path.display()))
    }
}

const ALL_KINDS: &[&str] = &["algo", "mic", "ref", "bf", "doa_csv", "manifest"];

pub fn validate_session_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 {
        return Err("session id 长度无效".into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Err("session id 只允许 ASCII 字母、数字、_、-".into());
    }
    Ok(())
}

pub fn kind_to_filename(prefix: &str, kind: &str) -> Option<String> {
    match kind {
        "algo" => Some(format!("{prefix}_respeaker_algo.wav")),
        "mic" => Some(format!("{prefix}_respeaker_mic.wav")),
        "ref" => Some(format!("{prefix}_respeaker_ref.wav")),
        "bf" => Some(format!("{prefix}_respeaker_bf.wav")),
        "doa_csv" => Some(format!("{prefix}_respeaker_doa.csv")),
        "manifest" => Some(format!("{prefix}_respeaker_session.json")),
        _ => None,
    }
}

pub fn target_paths_for_session(out_dir: &str, prefix: &str, config: &AppConfig) -> Vec<PathBuf> {
    let mut paths = ["algo", "mic", "ref", "manifest"]
        .iter()
        .filter_map(|kind| kind_to_filename(prefix, kind))
        .map(|name| PathBuf::from(out_dir).join(name))
        .collect::<Vec<_>>();
    if config.pipeline_enabled {
        if config.pipeline.modules.iter().any(|module| {
            matches!(
                module,
                ModuleConfig::Beamformer {
                    enabled: true,
                    wav: true,
                    ..
                }
            )
        }) {
            paths.push(PathBuf::from(out_dir).join(kind_to_filename(prefix, "bf").unwrap()));
        }
        if config.pipeline.modules.iter().any(|module| {
            matches!(
                module,
                ModuleConfig::Doa {
                    enabled: true,
                    csv: true,
                    ..
                }
            )
        }) {
            paths.push(PathBuf::from(out_dir).join(kind_to_filename(prefix, "doa_csv").unwrap()));
        }
    }
    paths
}

pub fn check_no_clobber(out_dir: &str, prefix: &str, config: &AppConfig) -> Result<(), String> {
    for path in target_paths_for_session(out_dir, prefix, config) {
        if path.exists() {
            return Err(format!("目标文件已存在: {}", path.display()));
        }
    }
    Ok(())
}

fn canonical_under(path: &Path, root: &Path) -> Result<PathBuf, String> {
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("确认目录 {} 失败: {e}", root.display()))?;
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("确认路径 {} 失败: {e}", path.display()))?;
    if !canonical.starts_with(&canonical_root) {
        return Err("路径不在输出目录内".into());
    }
    Ok(canonical)
}

pub fn resolve_file_path(out_dir: &str, prefix: &str, kind: &str) -> Result<PathBuf, String> {
    let name = kind_to_filename(prefix, kind).ok_or_else(|| format!("未知文件类型 {kind}"))?;
    let root = PathBuf::from(out_dir);
    let path = root.join(name);
    if !path.is_file() {
        return Err("文件不存在".into());
    }
    canonical_under(&path, &root)
}

pub fn scan_catalog(out_dir: &str) -> Vec<RecordingEntry> {
    scan_catalog_with_active(out_dir, None)
}

pub fn scan_catalog_with_active(
    out_dir: &str,
    active_session_id: Option<&str>,
) -> Vec<RecordingEntry> {
    let root = Path::new(out_dir);
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut manifests = Vec::new();
    let mut files: BTreeMap<String, Vec<SessionFile>> = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if name.ends_with("_respeaker_session.json") {
            if let Ok(mut manifest) = SessionManifest::load(&path) {
                if active_session_id != Some(manifest.session_id.as_str())
                    && matches!(
                        manifest.status,
                        SessionResultStatus::Starting | SessionResultStatus::Recording
                    )
                {
                    manifest.status = SessionResultStatus::Interrupted;
                }
                manifests.push(manifest);
            }
        } else if path.is_file() {
            for suffix in [
                "_respeaker_algo.wav",
                "_respeaker_mic.wav",
                "_respeaker_ref.wav",
                "_respeaker_bf.wav",
                "_respeaker_doa.csv",
            ] {
                if let Some(prefix) = name.strip_suffix(suffix) {
                    let kind = ALL_KINDS
                        .iter()
                        .find(|kind| kind_to_filename(prefix, kind).as_deref() == Some(name))
                        .copied()
                        .unwrap_or("unknown");
                    files
                        .entry(prefix.to_string())
                        .or_default()
                        .push(SessionFile {
                            kind: kind.to_string(),
                            path: name.to_string(),
                            bytes: fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                            exists: true,
                        });
                    break;
                }
            }
        }
    }
    let mut out = manifests
        .into_iter()
        .map(|manifest| RecordingEntry {
            id: manifest.session_id.clone(),
            prefix: manifest.prefix.clone(),
            status: manifest.status.clone(),
            files: manifest.files.clone(),
            manifest: Some(manifest),
        })
        .collect::<Vec<_>>();
    for (prefix, files) in files {
        if out.iter().any(|entry| entry.prefix == prefix) {
            continue;
        }
        out.push(RecordingEntry {
            id: format!("legacy-{prefix}"),
            prefix,
            status: SessionResultStatus::Legacy,
            manifest: None,
            files,
        });
    }
    out.sort_by(|a, b| b.prefix.cmp(&a.prefix));
    out
}

pub fn scan_trash_catalog(out_dir: &str) -> Vec<RecordingEntry> {
    let root = Path::new(out_dir).join(".trash");
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = entries
        .flatten()
        .filter_map(|entry| {
            let session_dir = entry.path();
            if !session_dir.is_dir() {
                return None;
            }
            let manifest_path = fs::read_dir(&session_dir)
                .ok()?
                .flatten()
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with("_respeaker_session.json"))
                });
            if let Some(manifest_path) = manifest_path
                && let Ok(manifest) = SessionManifest::load(&manifest_path)
            {
                return Some(RecordingEntry {
                    id: manifest.session_id.clone(),
                    prefix: manifest.prefix.clone(),
                    status: manifest.status.clone(),
                    files: manifest.files.clone(),
                    manifest: Some(manifest),
                });
            }
            let id = session_dir.file_name().and_then(|name| name.to_str())?;
            let prefix = id.strip_prefix("legacy-")?;
            let files = files_in_directory(&session_dir, prefix);
            (!files.is_empty()).then(|| RecordingEntry {
                id: id.to_string(),
                prefix: prefix.to_string(),
                status: SessionResultStatus::Legacy,
                manifest: None,
                files,
            })
        })
        .collect::<Vec<_>>();
    out.sort_by(|a, b| b.prefix.cmp(&a.prefix));
    out
}

fn files_in_directory(dir: &Path, prefix: &str) -> Vec<SessionFile> {
    ALL_KINDS
        .iter()
        .filter_map(|kind| {
            let name = kind_to_filename(prefix, kind)?;
            let path = dir.join(&name);
            Some(SessionFile {
                kind: (*kind).to_string(),
                path: name,
                bytes: fs::metadata(&path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0),
                exists: path.is_file(),
            })
        })
        .filter(|file| file.exists)
        .collect()
}

pub fn find_entry(out_dir: &str, session_id: &str) -> Result<RecordingEntry, String> {
    validate_session_id(session_id)?;
    scan_catalog(out_dir)
        .into_iter()
        .find(|entry| entry.id == session_id)
        .ok_or_else(|| format!("session {session_id} 不存在"))
}

pub fn find_trashed_entry(out_dir: &str, session_id: &str) -> Result<RecordingEntry, String> {
    validate_session_id(session_id)?;
    scan_trash_catalog(out_dir)
        .into_iter()
        .find(|entry| entry.id == session_id)
        .ok_or_else(|| format!("回收站中的 session {session_id} 不存在"))
}

fn trash_root(out_dir: &str, session_id: &str) -> Result<(PathBuf, PathBuf), String> {
    validate_session_id(session_id)?;
    let root = PathBuf::from(out_dir);
    let trash = root.join(".trash").join(session_id);
    fs::create_dir_all(&trash).map_err(|e| format!("创建回收站目录失败: {e}"))?;
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("确认输出目录失败: {e}"))?;
    let canonical_trash = trash
        .canonicalize()
        .map_err(|e| format!("确认回收站目录失败: {e}"))?;
    if !canonical_trash.starts_with(&canonical_root) {
        return Err("回收站路径不在输出目录内".into());
    }
    Ok((root, trash))
}

pub fn trash_session(out_dir: &str, session_id: &str, prefix: &str) -> Result<(), String> {
    let (root, trash) = trash_root(out_dir, session_id)?;
    crate::app_config::validate_prefix(prefix)?;
    let mut sources = Vec::new();
    for kind in ALL_KINDS {
        let name = kind_to_filename(prefix, kind).unwrap();
        let source = root.join(&name);
        if source.is_file() {
            sources.push((source, trash.join(name)));
        }
    }
    for (source, destination) in &sources {
        canonical_under(source, &root)?;
        if destination.exists() {
            return Err(format!("回收站目标已存在: {}", destination.display()));
        }
    }
    for (source, destination) in sources {
        fs::rename(&source, &destination)
            .map_err(|e| format!("移入回收站 {} 失败: {e}", source.display()))?;
    }
    Ok(())
}

pub fn restore_session(out_dir: &str, session_id: &str, prefix: &str) -> Result<(), String> {
    let (root, trash) = trash_root(out_dir, session_id)?;
    crate::app_config::validate_prefix(prefix)?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(&trash).map_err(|e| format!("读取回收站失败: {e}"))? {
        let source = entry.map_err(|e| format!("读取回收站项失败: {e}"))?.path();
        let Some(name) = source.file_name().and_then(|n| n.to_str()) else {
            return Err("回收站存在无效文件名".into());
        };
        let allowed = ALL_KINDS
            .iter()
            .filter_map(|kind| kind_to_filename(prefix, kind))
            .any(|expected| expected == name);
        if !allowed || !source.is_file() {
            return Err("回收站包含不属于该 session 的文件".into());
        }
        let destination = root.join(name);
        if destination.exists() {
            return Err(format!("目标已存在，无法恢复: {}", destination.display()));
        }
        entries.push((source, destination));
    }
    for (source, destination) in &entries {
        canonical_under(source, &trash)?;
        if !destination.starts_with(&root) {
            return Err("恢复目标路径越界".into());
        }
    }
    for (source, destination) in entries {
        fs::rename(&source, &destination)
            .map_err(|e| format!("恢复 {} 失败: {e}", source.display()))?;
    }
    let _ = fs::remove_dir(&trash);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "respeaker_recordings_{name}_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn no_clobber_detects_existing_target() {
        let dir = temp_dir("clobber");
        fs::write(dir.join("p_respeaker_algo.wav"), b"").unwrap();
        let config = AppConfig::default();
        assert!(check_no_clobber(dir.to_str().unwrap(), "p", &config).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn restore_rejects_path_traversal() {
        let dir = temp_dir("traversal");
        assert!(restore_session(dir.to_str().unwrap(), "../outside", "p").is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn trash_and_restore_only_move_known_files() {
        let dir = temp_dir("trash");
        let out = dir.to_str().unwrap();
        fs::write(dir.join("p_respeaker_algo.wav"), b"wav").unwrap();
        trash_session(out, "session-1", "p").unwrap();
        assert!(!dir.join("p_respeaker_algo.wav").exists());
        restore_session(out, "session-1", "p").unwrap();
        assert_eq!(fs::read(dir.join("p_respeaker_algo.wav")).unwrap(), b"wav");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn manifest_roundtrip_and_interrupted_scan() {
        let dir = temp_dir("manifest");
        let out = dir.to_str().unwrap();
        let mut manifest = SessionManifest::new(
            "session-1".into(),
            "p".into(),
            "now".into(),
            AppConfig::default(),
        );
        manifest.status = SessionResultStatus::Recording;
        manifest.save_atomic(out).unwrap();
        let entries = scan_catalog(out);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, SessionResultStatus::Interrupted);
        let active = scan_catalog_with_active(out, Some("session-1"));
        assert_eq!(active[0].status, SessionResultStatus::Recording);
        let _ = fs::remove_dir_all(dir);
    }
}
