//! 内置算法 Pipeline 配置、校验与串联运行时。

use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::beamformer::{
    BeamformerAlgorithm, BeamformerConfig, BeamformerDirectionSource, BeamformerRuntime,
};
use crate::doa::output::DoaRuntime;
use crate::doa::{DoaConfig, DoaResult};
use crate::web::{WebBroadcaster, WebServerHandle};

const PIPELINE_CONFIG_VERSION: u32 = 1;

fn default_enabled() -> bool {
    true
}

fn default_enable_viewer() -> bool {
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

fn default_beamformer_algorithm() -> BeamformerAlgorithm {
    BeamformerAlgorithm::RobustSuperdirective
}

fn default_beamformer_direction_source() -> BeamformerDirectionSource {
    BeamformerDirectionSource::Doa
}

fn default_fixed_internal_angle_deg() -> f32 {
    0.0
}

fn default_fallback_internal_angle_deg() -> f32 {
    0.0
}

fn default_direction_smoothing_ms() -> f32 {
    64.0
}

fn default_min_wng_db() -> f32 {
    3.0
}

fn default_sd_low_start_hz() -> f32 {
    350.0
}

fn default_sd_low_full_hz() -> f32 {
    500.0
}

fn default_sd_high_full_hz() -> f32 {
    2500.0
}

fn default_sd_high_end_hz() -> f32 {
    3500.0
}

fn default_output_gain_db() -> f32 {
    -3.0
}

fn default_beamformer_wav() -> bool {
    true
}

fn default_compare_wav() -> bool {
    false
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
        #[serde(default = "default_enable_viewer")]
        enable_viewer: bool,
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
    Beamformer {
        #[serde(default = "default_enabled")]
        enabled: bool,
        #[serde(default = "default_beamformer_algorithm")]
        algorithm: BeamformerAlgorithm,
        #[serde(default = "default_beamformer_direction_source")]
        direction_source: BeamformerDirectionSource,
        #[serde(default = "default_fixed_internal_angle_deg")]
        fixed_internal_angle_deg: f32,
        #[serde(default = "default_fallback_internal_angle_deg")]
        fallback_internal_angle_deg: f32,
        #[serde(default = "default_direction_smoothing_ms")]
        direction_smoothing_ms: f32,
        #[serde(default = "default_min_wng_db")]
        min_wng_db: f32,
        #[serde(default = "default_sd_low_start_hz")]
        sd_low_start_hz: f32,
        #[serde(default = "default_sd_low_full_hz")]
        sd_low_full_hz: f32,
        #[serde(default = "default_sd_high_full_hz")]
        sd_high_full_hz: f32,
        #[serde(default = "default_sd_high_end_hz")]
        sd_high_end_hz: f32,
        #[serde(default = "default_output_gain_db")]
        output_gain_db: f32,
        #[serde(default = "default_beamformer_wav")]
        wav: bool,
        #[serde(default = "default_compare_wav")]
        compare_wav: bool,
        #[serde(default)]
        enable_drc: bool,
    },
}

impl PipelineConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("读取 Pipeline 配置 {} 失败: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("解析 Pipeline 配置 {} 失败: {e}", path.display()))
    }

    pub(crate) fn parse(text: &str) -> Result<Self, String> {
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
        let mut bf_seen = false;
        for module in &self.modules {
            match module {
                ModuleConfig::Doa {
                    enabled,
                    enable_viewer: _,
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
                ModuleConfig::Beamformer {
                    enabled,
                    algorithm,
                    direction_source,
                    fixed_internal_angle_deg,
                    fallback_internal_angle_deg,
                    direction_smoothing_ms,
                    min_wng_db,
                    sd_low_start_hz,
                    sd_low_full_hz,
                    sd_high_full_hz,
                    sd_high_end_hz,
                    output_gain_db,
                    wav,
                    compare_wav,
                    enable_drc,
                } => {
                    if !enabled {
                        continue;
                    }
                    if bf_seen {
                        return Err("Pipeline 中只能启用一个 Beamformer 模块".into());
                    }
                    bf_seen = true;
                    if *direction_source == BeamformerDirectionSource::Doa && !doa_seen {
                        return Err(
                            "beamformer.direction_source = \"doa\" 要求配置顺序中前面已有 enabled DOA"
                                .into(),
                        );
                    }
                    BeamformerConfig {
                        enabled: true,
                        algorithm: *algorithm,
                        direction_source: *direction_source,
                        fixed_internal_angle_deg: *fixed_internal_angle_deg,
                        fallback_internal_angle_deg: *fallback_internal_angle_deg,
                        direction_smoothing_ms: *direction_smoothing_ms,
                        min_wng_db: *min_wng_db,
                        sd_low_start_hz: *sd_low_start_hz,
                        sd_low_full_hz: *sd_low_full_hz,
                        sd_high_full_hz: *sd_high_full_hz,
                        sd_high_end_hz: *sd_high_end_hz,
                        output_gain_db: *output_gain_db,
                        wav: *wav,
                        compare_wav: *compare_wav,
                        enable_drc: *enable_drc,
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
    pub start_frame: u64,
    pub frames: usize,
    pub algo: &'a [i16],
    pub mic: &'a [i16],
    pub reference: &'a [i16],
}

impl PipelineInputBlock<'_> {
    pub fn validate(&self) -> Result<(), String> {
        if self.algo.len() != self.frames {
            return Err(format!(
                "PipelineInputBlock algo 长度 {} != frames {}",
                self.algo.len(),
                self.frames
            ));
        }
        if self.mic.len() != self.frames * 4 {
            return Err(format!(
                "PipelineInputBlock mic 长度 {} != frames*4 {}",
                self.mic.len(),
                self.frames * 4
            ));
        }
        if self.reference.len() != self.frames {
            return Err(format!(
                "PipelineInputBlock reference 长度 {} != frames {}",
                self.reference.len(),
                self.frames
            ));
        }
        Ok(())
    }
}

/// 模块间共享的有类型状态。
#[derive(Default)]
struct PipelineState {
    latest_doa: Option<DoaResult>,
}

#[allow(clippy::large_enum_variant)]
enum PipelineModuleRuntime {
    Doa(DoaRuntime),
    Beamformer(BeamformerRuntime),
}

/// 按配置顺序串联执行的内置算法 Pipeline。
pub struct PipelineRuntime {
    modules: Vec<PipelineModuleRuntime>,
    state: PipelineState,
    web_server: Option<WebServerHandle>,
    web_broadcaster: Option<WebBroadcaster>,
    doa_sequence: u64,
}

impl PipelineRuntime {
    pub fn new(config: PipelineConfig, out_dir: &str, prefix: &str) -> Result<Self, String> {
        let mut modules = Vec::new();
        let mut doa_enable_viewer = None;
        for module in config.modules {
            match module {
                ModuleConfig::Doa {
                    enabled,
                    enable_viewer,
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
                    doa_enable_viewer = Some(enable_viewer);
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
                ModuleConfig::Beamformer {
                    enabled,
                    algorithm,
                    direction_source,
                    fixed_internal_angle_deg,
                    fallback_internal_angle_deg,
                    direction_smoothing_ms,
                    min_wng_db,
                    sd_low_start_hz,
                    sd_low_full_hz,
                    sd_high_full_hz,
                    sd_high_end_hz,
                    output_gain_db,
                    wav,
                    compare_wav,
                    enable_drc,
                } => {
                    if !enabled {
                        continue;
                    }
                    modules.push(PipelineModuleRuntime::Beamformer(BeamformerRuntime::new(
                        BeamformerConfig {
                            enabled: true,
                            algorithm,
                            direction_source,
                            fixed_internal_angle_deg,
                            fallback_internal_angle_deg,
                            direction_smoothing_ms,
                            min_wng_db,
                            sd_low_start_hz,
                            sd_low_full_hz,
                            sd_high_full_hz,
                            sd_high_end_hz,
                            output_gain_db,
                            wav,
                            compare_wav,
                            enable_drc,
                        },
                        out_dir,
                        prefix,
                    )?));
                }
            }
        }
        let (web_server, web_broadcaster) = if doa_enable_viewer == Some(true) {
            let (server, broadcaster) = WebServerHandle::start()?;
            (Some(server), Some(broadcaster))
        } else {
            (None, None)
        };
        Ok(Self {
            modules,
            state: PipelineState::default(),
            web_server,
            web_broadcaster,
            doa_sequence: 0,
        })
    }

    pub fn push_block(&mut self, input: PipelineInputBlock<'_>) -> Result<(), String> {
        input.validate()?;
        let _raw_recording_views = (input.algo, input.reference, input.start_frame);
        let web_broadcaster = self.web_broadcaster.clone();
        for module in &mut self.modules {
            match module {
                PipelineModuleRuntime::Doa(runtime) => {
                    for result in runtime.push_block(input.mic)? {
                        self.state.latest_doa = Some(result.clone());
                        self.doa_sequence += 1;
                        if let Some(broadcaster) = &web_broadcaster {
                            broadcaster.publish(self.doa_sequence, result)?;
                        }
                    }
                }
                PipelineModuleRuntime::Beamformer(runtime) => {
                    runtime.push_block(input.mic, self.state.latest_doa.as_ref())?;
                }
            }
        }
        Ok(())
    }

    pub fn finalize(&mut self) -> Result<(), String> {
        let mut first_err: Option<String> = None;
        for module in &mut self.modules {
            let result = match module {
                PipelineModuleRuntime::Doa(runtime) => runtime.finalize(),
                PipelineModuleRuntime::Beamformer(runtime) => {
                    let r = runtime.finalize();
                    if r.is_ok() {
                        let s = runtime.stats();
                        println!(
                            "BF stats: in={} out={} stft={} clipped={} das_fallback_bins={} min_wng_db={:.2}",
                            s.input_frames,
                            s.output_frames,
                            s.stft_frames,
                            s.clipped_samples,
                            s.das_fallback_bins,
                            s.min_generated_wng_db
                        );
                    }
                    r
                }
            };
            if let Err(e) = result {
                first_err.get_or_insert(e);
            }
        }
        if let Some(server) = &mut self.web_server
            && let Err(e) = server.shutdown()
        {
            first_err.get_or_insert(e);
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
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
    fn bundled_doa_bf_and_fixed_configs_are_valid() {
        PipelineConfig::parse(include_str!("../configs/doa_bf.toml")).unwrap();
        PipelineConfig::parse(include_str!("../configs/bf_fixed.toml")).unwrap();
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
            enable_viewer,
            csv,
            beta,
            cpsd_tau_ms,
            acquire_confidence,
            update_confidence,
            max_coast_ms,
            ..
        } = &config.modules[0]
        else {
            panic!("expected doa");
        };
        let defaults = DoaConfig::default();
        assert!(*enabled);
        assert!(*enable_viewer);
        assert!(!csv);
        assert_eq!(*beta, defaults.beta);
        assert_eq!(*cpsd_tau_ms, defaults.cpsd_tau_ms);
        assert_eq!(*acquire_confidence, defaults.acquire_confidence);
        assert_eq!(*update_confidence, defaults.update_confidence);
        assert_eq!(*max_coast_ms, defaults.max_coast_ms);
    }

    #[test]
    fn beamformer_omitted_fields_match_documented_defaults() {
        let config = PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "beamformer"
direction_source = "fixed"
"#,
        )
        .unwrap();
        let ModuleConfig::Beamformer {
            enabled,
            algorithm,
            direction_source,
            fixed_internal_angle_deg,
            fallback_internal_angle_deg,
            direction_smoothing_ms,
            min_wng_db,
            sd_low_start_hz,
            sd_low_full_hz,
            sd_high_full_hz,
            sd_high_end_hz,
            output_gain_db,
            wav,
            compare_wav,
            enable_drc,
        } = &config.modules[0]
        else {
            panic!("expected beamformer");
        };
        let d = BeamformerConfig::default();
        assert!(*enabled);
        assert_eq!(*algorithm, d.algorithm);
        assert_eq!(*direction_source, BeamformerDirectionSource::Fixed);
        assert_eq!(*fixed_internal_angle_deg, d.fixed_internal_angle_deg);
        assert_eq!(*fallback_internal_angle_deg, d.fallback_internal_angle_deg);
        assert_eq!(*direction_smoothing_ms, d.direction_smoothing_ms);
        assert_eq!(*min_wng_db, d.min_wng_db);
        assert_eq!(*sd_low_start_hz, d.sd_low_start_hz);
        assert_eq!(*sd_low_full_hz, d.sd_low_full_hz);
        assert_eq!(*sd_high_full_hz, d.sd_high_full_hz);
        assert_eq!(*sd_high_end_hz, d.sd_high_end_hz);
        assert_eq!(*output_gain_db, d.output_gain_db);
        assert_eq!(*wav, d.wav);
        assert_eq!(*compare_wav, d.compare_wav);
        assert!(!d.compare_wav);
        assert_eq!(*enable_drc, d.enable_drc);
        assert!(!d.enable_drc);
    }

    #[test]
    fn beamformer_validate_passes_through_algorithm_wav_and_compare() {
        let config = PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "beamformer"
direction_source = "fixed"
algorithm = "robust_superdirective"
wav = false
compare_wav = true
enable_drc = true
"#,
        )
        .unwrap();
        let ModuleConfig::Beamformer {
            algorithm,
            wav,
            compare_wav,
            enable_drc,
            ..
        } = &config.modules[0]
        else {
            panic!("expected beamformer");
        };
        // 校验必须按用户配置的 algorithm / wav / compare_wav 原值构造，而非写死默认值。
        assert_eq!(*algorithm, BeamformerAlgorithm::RobustSuperdirective);
        assert!(!wav);
        assert!(*compare_wav);
        assert!(*enable_drc);
        config.validate().unwrap();
    }

    #[test]
    fn rejects_invalid_version_unknown_module_duplicate_and_invalid_doa() {
        for text in [
            "version = 2",
            "version = 1\n[[modules]]\ntype = \"unknown_mod\"",
            "version = 1\n[[modules]]\ntype = \"doa\"\n[[modules]]\ntype = \"doa\"",
            "version = 1\n[[modules]]\ntype = \"doa\"\nbeta = 1.5",
            "version = 1\n[[modules]]\ntype = \"beamformer\"\ndirection_source = \"doa\"",
            "version = 1\n[[modules]]\ntype = \"beamformer\"\n[[modules]]\ntype = \"beamformer\"\ndirection_source = \"fixed\"",
            "version = 1\n[[modules]]\ntype = \"beamformer\"\ndirection_source = \"fixed\"\nsd_low_start_hz = 600\nsd_low_full_hz = 500",
        ] {
            assert!(PipelineConfig::parse(text).is_err(), "配置应被拒绝: {text}");
        }
    }

    #[test]
    fn fixed_beamformer_without_doa_is_allowed() {
        PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "beamformer"
direction_source = "fixed"
algorithm = "delay_sum"
"#,
        )
        .unwrap();
    }

    #[test]
    fn doa_before_beamformer_is_allowed() {
        PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "doa"
enable_viewer = false
[[modules]]
type = "beamformer"
"#,
        )
        .unwrap();
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
enable_viewer = false
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
                start_frame: 0,
                frames: 100,
                algo: &algo,
                mic: &mic,
                reference: &reference,
            })
            .unwrap();
        assert_eq!((algo, mic, reference), expected);
        runtime.finalize().unwrap();
    }

    #[test]
    fn web_viewer_follows_enabled_doa_module() {
        let enabled = PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "doa"
"#,
        )
        .unwrap();
        let disabled = PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "doa"
enabled = false
"#,
        )
        .unwrap();

        let mut enabled_runtime =
            PipelineRuntime::new(enabled, "target/out", "web_enabled").unwrap();
        let mut disabled_runtime =
            PipelineRuntime::new(disabled, "target/out", "web_disabled").unwrap();
        assert!(enabled_runtime.web_server.is_some());
        assert!(disabled_runtime.web_server.is_none());
        enabled_runtime.finalize().unwrap();
        disabled_runtime.finalize().unwrap();
    }

    #[test]
    fn enable_viewer_false_disables_viewer_service() {
        let config = PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "doa"
enable_viewer = false
"#,
        )
        .unwrap();
        let ModuleConfig::Doa {
            enabled,
            enable_viewer,
            ..
        } = &config.modules[0]
        else {
            panic!("expected doa");
        };
        assert!(*enabled);
        assert!(!*enable_viewer);

        let mut runtime = PipelineRuntime::new(config, "target/out", "browser_disabled").unwrap();
        assert!(runtime.web_server.is_none());
        assert!(runtime.web_broadcaster.is_none());
        runtime.finalize().unwrap();
    }
}
