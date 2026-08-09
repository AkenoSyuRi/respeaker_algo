//! 内置算法 Pipeline 配置、校验与串联运行时。

use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::doa::output::DoaRuntime;
use crate::doa::{DoaConfig, DoaResult};

const PIPELINE_CONFIG_VERSION: u32 = 1;

fn default_enabled() -> bool {
    true
}

fn default_beta() -> f32 {
    DoaConfig::default().beta
}

fn default_cpsd_tau_ms() -> f32 {
    DoaConfig::default().cpsd_tau_ms
}

fn default_acquire_confidence() -> f32 {
    DoaConfig::default().acquire_confidence
}

fn default_update_confidence() -> f32 {
    DoaConfig::default().update_confidence
}

fn default_max_coast_ms() -> u32 {
    DoaConfig::default().max_coast_ms
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineConfig {
    version: u32,
    #[serde(default)]
    modules: Vec<ModuleConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ModuleConfig {
    Doa {
        #[serde(default = "default_enabled")]
        enabled: bool,
        #[serde(default)]
        csv: bool,
        #[serde(default = "default_beta")]
        beta: f32,
        #[serde(default = "default_cpsd_tau_ms")]
        cpsd_tau_ms: f32,
        #[serde(default)]
        angle_offset_deg: f32,
        #[serde(default)]
        clockwise: bool,
        #[serde(default = "default_acquire_confidence")]
        acquire_confidence: f32,
        #[serde(default = "default_update_confidence")]
        update_confidence: f32,
        #[serde(default = "default_max_coast_ms")]
        max_coast_ms: u32,
    },
}

impl PipelineConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("读取 Pipeline 配置 {} 失败: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("解析 Pipeline 配置 {} 失败: {e}", path.display()))
    }

    fn parse(text: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != PIPELINE_CONFIG_VERSION {
            return Err(format!(
                "不支持的 Pipeline 配置版本 {}，当前仅支持 {}",
                self.version, PIPELINE_CONFIG_VERSION
            ));
        }

        let mut doa_seen = false;
        for module in &self.modules {
            match module {
                ModuleConfig::Doa {
                    enabled,
                    beta,
                    cpsd_tau_ms,
                    angle_offset_deg,
                    clockwise: _,
                    acquire_confidence,
                    update_confidence,
                    max_coast_ms,
                    csv: _,
                } => {
                    if !enabled {
                        continue;
                    }
                    if doa_seen {
                        return Err("Pipeline 中只能启用一个 DOA 模块".into());
                    }
                    doa_seen = true;
                    DoaConfig {
                        beta: *beta,
                        cpsd_tau_ms: *cpsd_tau_ms,
                        angle_offset_deg: *angle_offset_deg,
                        acquire_confidence: *acquire_confidence,
                        update_confidence: *update_confidence,
                        max_coast_ms: *max_coast_ms,
                        ..DoaConfig::default()
                    }
                    .validate()?;
                }
            }
        }
        Ok(())
    }
}

/// 每个采集块的完整只读输入；模块不得修改录音缓冲。
pub struct PipelineInputBlock<'a> {
    pub algo: &'a [i16],
    pub mic: &'a [i16],
    pub reference: &'a [i16],
}

/// 模块间共享的有类型状态。后续 BF 将消费先前 DOA 发布的结果。
#[derive(Default)]
struct PipelineState {
    latest_doa: Option<DoaResult>,
}

enum PipelineModuleRuntime {
    Doa(DoaRuntime),
}

/// 按配置顺序串联执行的内置算法 Pipeline。
pub struct PipelineRuntime {
    modules: Vec<PipelineModuleRuntime>,
    state: PipelineState,
}

impl PipelineRuntime {
    pub fn new(config: PipelineConfig, out_dir: &str, prefix: &str) -> Result<Self, String> {
        let mut modules = Vec::new();
        for module in config.modules {
            match module {
                ModuleConfig::Doa {
                    enabled,
                    csv,
                    beta,
                    cpsd_tau_ms,
                    angle_offset_deg,
                    clockwise,
                    acquire_confidence,
                    update_confidence,
                    max_coast_ms,
                } => {
                    if !enabled {
                        continue;
                    }
                    let csv_path = csv.then(|| format!("{out_dir}/{prefix}_respeaker_doa.csv"));
                    modules.push(PipelineModuleRuntime::Doa(DoaRuntime::new(
                        DoaConfig {
                            beta,
                            cpsd_tau_ms,
                            angle_offset_deg,
                            clockwise,
                            acquire_confidence,
                            update_confidence,
                            max_coast_ms,
                            ..DoaConfig::default()
                        },
                        csv_path.as_deref(),
                    )?));
                }
            }
        }
        Ok(Self {
            modules,
            state: PipelineState::default(),
        })
    }

    pub fn push_block(&mut self, input: PipelineInputBlock<'_>) -> Result<(), String> {
        let _raw_recording_views = (input.algo, input.reference);
        for module in &mut self.modules {
            match module {
                PipelineModuleRuntime::Doa(runtime) => {
                    if let Some(result) = runtime.push_block(input.mic)? {
                        self.state.latest_doa = Some(result);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn finalize(&mut self) -> Result<(), String> {
        for module in &mut self.modules {
            match module {
                PipelineModuleRuntime::Doa(runtime) => runtime.finalize()?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_doa_config_is_valid() {
        PipelineConfig::parse(include_str!("../configs/doa.toml")).unwrap();
    }

    #[test]
    fn empty_module_list_is_allowed() {
        PipelineConfig::parse("version = 1").unwrap();
    }

    #[test]
    fn doa_fields_use_defaults() {
        let config = PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "doa"
"#,
        )
        .unwrap();
        let ModuleConfig::Doa {
            enabled,
            csv,
            beta,
            cpsd_tau_ms,
            acquire_confidence,
            update_confidence,
            max_coast_ms,
            ..
        } = &config.modules[0];
        let defaults = DoaConfig::default();
        assert!(*enabled);
        assert!(!csv);
        assert_eq!(*beta, defaults.beta);
        assert_eq!(*cpsd_tau_ms, defaults.cpsd_tau_ms);
        assert_eq!(*acquire_confidence, defaults.acquire_confidence);
        assert_eq!(*update_confidence, defaults.update_confidence);
        assert_eq!(*max_coast_ms, defaults.max_coast_ms);
    }

    #[test]
    fn rejects_invalid_version_unknown_module_duplicate_and_invalid_doa() {
        for text in [
            "version = 2",
            "version = 1\n[[modules]]\ntype = \"bf\"",
            "version = 1\n[[modules]]\ntype = \"doa\"\n[[modules]]\ntype = \"doa\"",
            "version = 1\n[[modules]]\ntype = \"doa\"\nbeta = 1.5",
        ] {
            assert!(PipelineConfig::parse(text).is_err(), "配置应被拒绝: {text}");
        }
    }

    #[test]
    fn disabled_modules_are_allowed() {
        PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "doa"
enabled = false
[[modules]]
type = "doa"
enabled = false
"#,
        )
        .unwrap();
    }

    #[test]
    fn pipeline_does_not_modify_recording_buffers() {
        let config = PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "doa"
"#,
        )
        .unwrap();
        let mut runtime = PipelineRuntime::new(config, "target/out", "pipeline_test").unwrap();
        let algo = vec![1i16; 100];
        let mic = vec![2i16; 400];
        let reference = vec![3i16; 100];
        let expected = (algo.clone(), mic.clone(), reference.clone());
        runtime
            .push_block(PipelineInputBlock {
                algo: &algo,
                mic: &mic,
                reference: &reference,
            })
            .unwrap();
        assert_eq!((algo, mic, reference), expected);
    }
}
