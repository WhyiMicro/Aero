// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod editor;
mod icon;
mod log;
mod recorder;
mod ui;
mod winutil;

use anyhow::{Context, Result, bail};
use config::Config;
use eframe::egui;
use std::path::PathBuf;

fn main() {
    if !winutil::single_instance() {
        // Already running: bring its window to the front instead.
        winutil::ShowSignal::signal_running_instance();
        return;
    }
    if let Err(e) = run() {
        info!("fatal: {e:#}");
        winutil::error_box(&format!("{e:#}"));
    }
}

fn run() -> Result<()> {
    let data = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| ".".into()).join("Aero");
    std::fs::create_dir_all(&data).context("creating %APPDATA%\\Aero")?;
    log::init(&data.join("aero.log"));
    let paths = ui::Paths {
        config: data.join("config.toml"),
        buffer: std::env::temp_dir().join("aero-buffer"),
        ffmpeg_log: data.join("ffmpeg.log"),
    };
    let _ = std::fs::remove_file(&paths.ffmpeg_log);

    let cfg = Config::load_or_create(&paths.config)?;
    check_ffmpeg(&recorder::resolve_ffmpeg(&cfg.ffmpeg))?;

    // Launched by "Start with Windows": go straight to the tray.
    let start_hidden = std::env::args().any(|a| a == "--tray");
    let signal = winutil::ShowSignal::create();
    let icon = egui::IconData { rgba: ui::icon_rgba(true), width: ui::ICON, height: ui::ICON };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Aero")
            .with_inner_size([460.0, 640.0])
            .with_min_inner_size([400.0, 480.0])
            .with_visible(true)
            .with_icon(icon),
        ..Default::default()
    };
    info!("Aero starting (hidden: {start_hidden})");
    eframe::run_native(
        "Aero",
        options,
        Box::new(move |cc| Ok(Box::new(ui::AeroApp::new(cc, paths, cfg, signal, start_hidden)?))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

fn check_ffmpeg(ffmpeg: &str) -> Result<()> {
    let ok = recorder::hidden_cmd(ffmpeg)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        bail!(
            "Aero needs ffmpeg but couldn't run '{ffmpeg}'.\n\nInstall it (e.g. `scoop install ffmpeg` or \
             `winget install ffmpeg`) or set the full path to ffmpeg.exe in\n%APPDATA%\\Aero\\config.toml"
        );
    }
    Ok(())
}
