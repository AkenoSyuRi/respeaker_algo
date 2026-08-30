//! ReSpeaker Mic Array v2.0 的常驻本地录音管理服务。

#[cfg(not(windows))]
compile_error!("respeaker_algo 仅支持 Windows ReSpeaker WASAPI 独占录音");

mod app_config;
mod audio;
mod beamformer;
mod controller;
mod doa;
mod drc;
mod events;
mod pipeline;
mod pipeline_worker;
mod recorder;
mod recordings;
mod wasapi;
mod wav;
mod web;

use std::sync::mpsc;

use app_config::load_or_default;
use controller::spawn;
use events::EventBus;

fn main() {
    let (config, config_warning) = load_or_default();
    if let Some(warning) = &config_warning {
        eprintln!("[config] {warning}");
    }

    let events = EventBus::new(256);
    let controller = match spawn(config, config_warning, events.clone()) {
        Ok(controller) => controller,
        Err(error) => {
            eprintln!("启动 AudioController 失败: {error}");
            std::process::exit(1);
        }
    };
    let mut web = match web::start(controller.clone(), events) {
        Ok(web) => web,
        Err(error) => {
            eprintln!("启动 Web 服务失败: {error}");
            let _ = controller.shutdown();
            std::process::exit(1);
        }
    };

    let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
    if let Err(error) = ctrlc::set_handler(move || {
        let _ = shutdown_tx.try_send(());
    }) {
        eprintln!("注册 Ctrl+C 处理器失败: {error}");
        let _ = web.shutdown();
        let _ = controller.shutdown();
        std::process::exit(1);
    }
    let _ = shutdown_rx.recv();

    let controller_error = controller.shutdown().err();
    if let Some(error) = &controller_error {
        eprintln!("关闭 AudioController 失败: {error}");
    }
    if let Err(error) = web.shutdown() {
        eprintln!("关闭 Web 服务失败: {error}");
    }
    if controller_error.is_some() {
        std::process::exit(1);
    }
}
