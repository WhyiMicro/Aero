//! The Aero window (egui) plus the tray icon and global hotkey.
//!
//! The window can be hidden: closing it only hides it to the tray. While hidden, eframe draws
//! nothing and only calls `logic()` when something wakes it (a hotkey, tray click, a finished
//! save), so the UI costs nothing in the background.

use crate::config::Config;
use crate::recorder::{self, Recorder};
use crate::{audio, info, updater, winutil};
use eframe::egui::{self, Color32, RichText, Vec2};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub struct Paths {
    pub config: PathBuf,
    pub buffer: PathBuf,
    pub ffmpeg_log: PathBuf,
}

/// Things that happen outside the UI (other threads, tray, hotkey) and need handling.
enum Msg {
    Hotkey(u32),
    Menu(MenuId),
    Show,
    Saved(Result<PathBuf, String>),
    /// Result of an update check; `bool` = the user asked for it (vs. the daily check).
    UpdateChecked(Result<Option<updater::Release>, String>, bool),
    UpdateInstalled(Result<PathBuf, String>),
}

enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(updater::Release),
    Installing(updater::Release),
    Failed(String),
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Clips,
    Edit,
    Settings,
}

struct Clip {
    path: PathBuf,
    when: String,
    size_mb: f64,
}

struct TrayItems {
    open: MenuItem,
    save: MenuItem,
    pause: MenuItem,
    folder: MenuItem,
    update: MenuItem,
    quit: MenuItem,
}

pub struct AeroApp {
    paths: Paths,
    cfg: Config,
    draft: Config,
    ffmpeg_exe: String,
    encoder: String,
    rec: Option<Arc<Recorder>>,

    hotkeys: GlobalHotKeyManager,
    hotkey: Option<HotKey>,
    tray: Option<TrayIcon>,
    items: TrayItems,

    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    ctx: egui::Context,

    saving: bool,
    toast: Option<(String, bool, Instant)>,
    tab: Tab,
    clips: Vec<Clip>,
    mics: Option<Vec<(String, u16)>>,
    default_mic: Option<String>,
    capturing_hotkey: bool,
    autostart: bool,
    meters: Vec<(&'static str, f32)>,
    quitting: bool,
    editor: crate::editor::Editor,
    /// Frames left before hiding a window that was started with --tray (see `ui`).
    hide_after_frames: u8,
    update: UpdateState,
    /// Whether the "update available" dialog should be showing.
    update_prompt: bool,
}

impl AeroApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        paths: Paths,
        cfg: Config,
        show_signal: Option<winutil::ShowSignal>,
        start_hidden: bool,
        just_updated: bool,
    ) -> anyhow::Result<Self> {
        let ctx = cc.egui_ctx.clone();
        style(&ctx);
        let (tx, rx) = mpsc::channel();

        // Hotkey / tray / menu events arrive on the UI thread's message loop; forward them
        // and wake eframe so `logic()` runs even while the window is hidden.
        let (t, c) = (tx.clone(), ctx.clone());
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            if e.state() == HotKeyState::Pressed {
                let _ = t.send(Msg::Hotkey(e.id()));
                c.request_repaint();
            }
        }));
        let (t, c) = (tx.clone(), ctx.clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let _ = t.send(Msg::Menu(e.id));
            c.request_repaint();
        }));
        let (t, c) = (tx.clone(), ctx.clone());
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            let left_click = matches!(
                e,
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
            );
            if left_click {
                let _ = t.send(Msg::Show);
                c.request_repaint();
            }
        }));
        // A second launch of Aero signals us to show the window.
        if let Some(sig) = show_signal {
            let (t, c) = (tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                while sig.wait() {
                    let _ = t.send(Msg::Show);
                    c.request_repaint();
                }
            });
        }

        let items = TrayItems {
            open: MenuItem::new("Open Aero", true, None),
            save: MenuItem::new(format!("Save clip\t{}", cfg.hotkey), true, None),
            pause: MenuItem::new("Pause recording", true, None),
            folder: MenuItem::new("Open clips folder", true, None),
            update: MenuItem::new("Check for updates", true, None),
            quit: MenuItem::new("Quit", true, None),
        };
        let menu = Menu::new();
        menu.append_items(&[
            &items.open,
            &items.save,
            &items.pause,
            &items.folder,
            &PredefinedMenuItem::separator(),
            &items.update,
            &items.quit,
        ])?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_icon(tray_icon::Icon::from_rgba(icon_rgba(true), ICON, ICON)?)
            .with_tooltip("Aero")
            .build()?;

        let hotkeys = GlobalHotKeyManager::new()?;
        let ffmpeg_exe = recorder::resolve_ffmpeg(&cfg.ffmpeg);
        let ffmpeg_exe_for_editor = ffmpeg_exe.clone();
        let mut app = Self {
            draft: cfg.clone(),
            encoder: String::new(),
            ffmpeg_exe,
            paths,
            cfg,
            rec: None,
            hotkeys,
            hotkey: None,
            tray: Some(tray),
            items,
            tx,
            rx,
            ctx,
            saving: false,
            toast: None,
            tab: Tab::Clips,
            clips: Vec::new(),
            mics: None,
            default_mic: None,
            capturing_hotkey: false,
            autostart: autostart_enabled(),
            meters: Vec::new(),
            quitting: false,
            editor: crate::editor::Editor::new(cc.egui_ctx.clone(), ffmpeg_exe_for_editor),
            hide_after_frames: if start_hidden { 2 } else { 0 },
            update: UpdateState::Idle,
            update_prompt: false,
        };
        if just_updated {
            app.toast(format!("Updated to Aero {}", updater::current()), true);
        }
        if app.cfg.check_updates && updater::auto_check_due() {
            app.check_for_updates(false);
        }
        if app.autostart && !updater::is_dev_build() {
            // Keep the startup entry current (exe may have moved; older versions lacked --tray).
            // Dev builds (cargo's target folder) never take it over from the installed copy.
            set_autostart(true);
        }
        app.encoder = recorder::pick_encoder(&app.runtime_cfg());
        app.register_hotkey();
        app.set_recording(true);
        app.refresh_clips();
        Ok(app)
    }

    /// The config as the recorder needs it (with the real ffmpeg.exe path).
    fn runtime_cfg(&self) -> Config {
        Config { ffmpeg: self.ffmpeg_exe.clone(), ..self.cfg.clone() }
    }

    fn toast(&mut self, text: impl Into<String>, ok: bool) {
        self.toast = Some((text.into(), ok, Instant::now()));
    }

    // ---------------------------------------------------------------- updates

    /// `manual`: the user clicked "Check for updates" (always prompt, report "up to date").
    fn check_for_updates(&mut self, manual: bool) {
        if matches!(self.update, UpdateState::Checking | UpdateState::Installing(_)) {
            return;
        }
        self.update = UpdateState::Checking;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let r = updater::check().map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::UpdateChecked(r, manual));
            ctx.request_repaint();
        });
    }

    fn install_update(&mut self) {
        let UpdateState::Available(rel) = &self.update else { return };
        if updater::is_dev_build() {
            self.toast("This is a development build: update it with git pull and install.ps1.", false);
            return;
        }
        let rel = rel.clone();
        self.update = UpdateState::Installing(rel.clone());
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let r = updater::install(&rel).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::UpdateInstalled(r));
            ctx.request_repaint();
        });
    }

    /// The "a new version is available" prompt.
    fn update_dialog(&mut self, ctx: &egui::Context) {
        let installing = matches!(self.update, UpdateState::Installing(_));
        let rel = match &self.update {
            UpdateState::Available(r) | UpdateState::Installing(r) if self.update_prompt || installing => r.clone(),
            _ => return,
        };
        let (mut update, mut later, mut skip) = (false, false, false);
        let resp = egui::Modal::new(egui::Id::unique("update-dialog")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.heading(format!("Aero {} is available", rel.version));
            ui.label(RichText::new(format!("You have Aero {}.", updater::current())).weak());
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                ui.label(release_notes_text(&rel.notes));
            });
            if !rel.page_url.is_empty() {
                ui.hyperlink_to("View the release on GitHub", &rel.page_url);
            }
            ui.add_space(8.0);
            if installing {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Downloading and installing... Aero restarts by itself.");
                });
            } else {
                if rel.sha256.is_none() {
                    ui.label(RichText::new("This release has no checksum, so the download can't be verified.").small().weak());
                }
                ui.horizontal(|ui| {
                    let button = egui::Button::new(RichText::new("Update now").strong().color(Color32::WHITE)).fill(Color32::from_rgb(211, 47, 47));
                    update = ui.add(button).on_hover_text("Downloads the new version, then restarts Aero. Your settings and clips are kept.").clicked();
                    later = ui.button("Later").clicked();
                    skip = ui.button("Skip this version").on_hover_text("Don't remind me about this version again").clicked();
                });
            }
        });
        if installing {
            return;
        }
        if update {
            self.install_update();
        } else if skip {
            updater::skip(&rel.tag);
            self.update_prompt = false;
            self.update = UpdateState::Idle;
            self.update_tray_item();
            if let Some(tray) = &self.tray {
                let _ = tray.set_tooltip(Some("Aero"));
            }
        } else if later || resp.should_close() {
            self.update_prompt = false;
        }
    }

    fn update_tray_item(&self) {
        let text = match &self.update {
            UpdateState::Available(r) => format!("Update to Aero {}...", r.version),
            UpdateState::Installing(_) => "Updating...".into(),
            _ => "Check for updates".into(),
        };
        self.items.update.set_text(text);
    }

    // ---------------------------------------------------------------- actions

    fn set_recording(&mut self, on: bool) {
        if let Some(r) = self.rec.take() {
            r.stop();
        }
        if on {
            match Recorder::start(
                self.runtime_cfg(),
                self.encoder.clone(),
                self.paths.buffer.clone(),
                self.paths.ffmpeg_log.clone(),
            ) {
                Ok(r) => self.rec = Some(Arc::new(r)),
                Err(e) => self.toast(format!("Couldn't start recording: {e:#}"), false),
            }
        }
        self.meters.clear();
        let recording = self.rec.is_some();
        self.items.pause.set_text(if recording { "Pause recording" } else { "Resume recording" });
        if let Some(tray) = &self.tray {
            let _ = tray.set_icon(tray_icon::Icon::from_rgba(icon_rgba(recording), ICON, ICON).ok());
            let _ = tray.set_tooltip(Some(if recording {
                format!("Aero - recording. {} saves the last {}s", self.cfg.hotkey, self.cfg.clip_seconds)
            } else {
                "Aero - paused".into()
            }));
        }
    }

    fn save_clip(&mut self) {
        let Some(rec) = self.rec.clone() else {
            winutil::beep(false);
            self.toast("Recording is paused - nothing to save.", false);
            return;
        };
        if self.saving {
            return;
        }
        self.saving = true;
        let (tx, ctx, dir) = (self.tx.clone(), self.ctx.clone(), self.cfg.clips_dir());
        std::thread::spawn(move || {
            let r = rec.save_clip(&dir).map_err(|e| format!("{e:#}"));
            winutil::beep(r.is_ok());
            let _ = tx.send(Msg::Saved(r));
            ctx.request_repaint();
        });
    }

    fn register_hotkey(&mut self) {
        if let Some(old) = self.hotkey.take() {
            let _ = self.hotkeys.unregister(old);
        }
        let spec = self.cfg.hotkey.clone();
        match spec.parse::<HotKey>() {
            Ok(hk) => match self.hotkeys.register(hk) {
                Ok(()) => self.hotkey = Some(hk),
                Err(e) => self.toast(format!("Hotkey {spec} is taken by another app ({e}). Pick another."), false),
            },
            Err(e) => self.toast(format!("Invalid hotkey \"{spec}\": {e}"), false),
        }
        self.items.save.set_text(format!("Save clip\t{spec}"));
    }

    fn apply_settings(&mut self) {
        let new = self.draft.clone();
        if let Err(e) = new.save(&self.paths.config) {
            self.toast(format!("{e:#}"), false);
            return;
        }
        let old = std::mem::replace(&mut self.cfg, new.clone());
        if new.ffmpeg != old.ffmpeg {
            self.ffmpeg_exe = recorder::resolve_ffmpeg(&new.ffmpeg);
        }
        if new.encoder != old.encoder || new.ffmpeg != old.ffmpeg {
            self.encoder = recorder::pick_encoder(&self.runtime_cfg());
        }
        if new.hotkey != old.hotkey {
            self.register_hotkey();
        }
        // Only restart capture (which empties the replay buffer) if capture settings changed.
        let neutral = |c: &Config| Config { hotkey: String::new(), output_dir: String::new(), ..c.clone() };
        if neutral(&new) != neutral(&old) && self.rec.is_some() {
            self.set_recording(true);
        } else {
            self.set_recording(self.rec.is_some()); // refresh tooltip text
        }
        self.refresh_clips();
        info!("settings applied");
        self.toast("Settings saved.", true);
    }

    fn show_window(&mut self) {
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.refresh_clips();
    }

    fn quit(&mut self) {
        self.quitting = true;
        if let Some(r) = self.rec.take() {
            r.stop();
        }
        self.tray = None; // removes the tray icon right away
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn refresh_clips(&mut self) {
        let dir = self.cfg.clips_dir();
        let mut clips: Vec<(SystemTime, Clip)> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("mp4")))
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                let modified = meta.modified().ok()?;
                let when = chrono::DateTime::<chrono::Local>::from(modified).format("%b %-d, %H:%M:%S").to_string();
                Some((modified, Clip { path: e.path(), when, size_mb: meta.len() as f64 / 1e6 }))
            })
            .collect();
        clips.sort_by_key(|c| std::cmp::Reverse(c.0));
        self.clips = clips.into_iter().map(|c| c.1).collect();
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Hotkey(id) => {
                if self.hotkey.is_some_and(|h| h.id() == id) {
                    self.save_clip();
                }
            }
            Msg::Show => self.show_window(),
            Msg::Menu(id) => {
                if id == *self.items.open.id() {
                    self.show_window();
                } else if id == *self.items.save.id() {
                    self.save_clip();
                } else if id == *self.items.pause.id() {
                    self.set_recording(self.rec.is_none());
                } else if id == *self.items.folder.id() {
                    open_folder(&self.cfg.clips_dir());
                } else if id == *self.items.update.id() {
                    self.show_window();
                    if matches!(self.update, UpdateState::Available(_)) {
                        self.update_prompt = true;
                    } else {
                        self.check_for_updates(true);
                    }
                } else if id == *self.items.quit.id() {
                    self.quit();
                }
            }
            Msg::UpdateChecked(r, manual) => {
                self.update = match r {
                    Ok(Some(rel)) => {
                        let skipped = !manual && rel.tag == updater::skipped_tag();
                        info!("update available: {} (skipped: {skipped})", rel.tag);
                        // Only pop the dialog for a manual check, or a version not skipped.
                        // If the window is hidden, it appears when the window is next opened.
                        self.update_prompt = !skipped;
                        if let Some(tray) = &self.tray
                            && !skipped
                        {
                            let _ = tray.set_tooltip(Some(format!("Aero - update available: {}", rel.version)));
                        }
                        UpdateState::Available(rel)
                    }
                    Ok(None) => {
                        if manual {
                            self.toast(format!("You're up to date (Aero {}).", updater::current()), true);
                        }
                        UpdateState::UpToDate
                    }
                    Err(e) => {
                        info!("update check failed: {e}");
                        if manual {
                            self.toast(format!("Couldn't check for updates: {e}"), false);
                            UpdateState::Failed(e)
                        } else {
                            UpdateState::Idle
                        }
                    }
                };
                self.update_tray_item();
            }
            Msg::UpdateInstalled(r) => match r {
                Ok(exe) => match updater::relaunch(&exe, true) {
                    Ok(()) => {
                        info!("restarting into the new version");
                        self.quit();
                    }
                    Err(e) => self.toast(format!("Update installed, but restarting failed ({e:#}). Start Aero again."), false),
                },
                Err(e) => {
                    info!("update failed: {e}");
                    if let UpdateState::Installing(rel) = std::mem::replace(&mut self.update, UpdateState::Idle) {
                        self.update = UpdateState::Available(rel);
                    }
                    self.update_tray_item();
                    self.toast(format!("Update failed: {e}"), false);
                }
            },
            Msg::Saved(r) => {
                self.saving = false;
                match r {
                    Ok(path) => {
                        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                        if let Some(tray) = &self.tray {
                            let _ = tray.set_tooltip(Some(format!("Aero - saved {name}")));
                        }
                        self.toast(format!("Saved {name}"), true);
                        self.refresh_clips();
                    }
                    Err(e) => {
                        info!("save failed: {e}");
                        self.toast(format!("Couldn't save: {e}"), false);
                        // The window may be hidden; make sure the user notices.
                        std::thread::spawn(move || winutil::error_box(&format!("Couldn't save the clip.\n\n{e}")));
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------------- UI

    fn header(&mut self, ui: &mut egui::Ui) {
        let recording = self.rec.is_some();
        ui.horizontal(|ui| {
            let (dot, text) = if recording {
                (Color32::from_rgb(229, 57, 53), format!("Recording - keeping the last {}s", self.cfg.clip_seconds))
            } else {
                (Color32::GRAY, "Paused".to_string())
            };
            let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
            ui.painter().circle_filled(r.center(), 5.5, dot);
            ui.label(RichText::new(text).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(if recording { "Pause" } else { "Resume" }).clicked() {
                    self.set_recording(!recording);
                }
            });
        });

        if let UpdateState::Available(r) = &self.update
            && !self.update_prompt
            && ui.link(RichText::new(format!("Update available: Aero {} - click to see what's new", r.version)).small()).clicked()
        {
            self.update_prompt = true;
        }

        if self.rec.as_ref().is_some_and(|r| r.capture_blocked()) {
            ui.label(
                RichText::new(
                    "Can't see the screen right now - probably a fullscreen game that Windows \
                     can't capture. Clips won't have video until this clears. See README \
                     (\"fullscreen\") for the fix.",
                )
                .small()
                .color(Color32::from_rgb(255, 167, 38)),
            );
        }

        if let Some(rec) = &self.rec {
            for (name, peak) in rec.levels() {
                match self.meters.iter_mut().find(|m| m.0 == name) {
                    Some(m) => m.1 = peak.max(m.1 * 0.8), // fast attack, smooth release
                    None => self.meters.push((name, peak)),
                }
            }
            for (name, level) in &self.meters {
                ui.horizontal(|ui| {
                    ui.add_sized([96.0, 14.0], egui::Label::new(RichText::new(*name).small()));
                    meter(ui, *level);
                });
            }
            if self.meters.is_empty() {
                ui.label(RichText::new("No audio tracks (enable them in Settings)").small().weak());
            }
        }

        ui.add_space(6.0);
        let label = if self.saving { "Saving...".to_string() } else { format!("Save clip   ({})", self.cfg.hotkey) };
        let button = egui::Button::new(RichText::new(label).size(17.0).strong().color(Color32::WHITE))
            .fill(if recording { Color32::from_rgb(211, 47, 47) } else { Color32::from_gray(110) })
            .min_size(Vec2::new(ui.available_width(), 42.0));
        if ui.add_enabled(recording && !self.saving, button).clicked() {
            self.save_clip();
        }
    }

    fn clips_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let n = self.clips.len();
            ui.label(RichText::new(format!("{n} clip{}", if n == 1 { "" } else { "s" })).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Open folder").clicked() {
                    open_folder(&self.cfg.clips_dir());
                }
                if ui.button("Refresh").clicked() {
                    self.refresh_clips();
                }
            });
        });
        ui.add_space(4.0);
        if self.clips.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No clips yet").size(16.0));
                ui.label(RichText::new(format!("Press {} to save the last {} seconds.", self.cfg.hotkey, self.cfg.clip_seconds)).weak());
            });
            return;
        }
        let mut recycle = None;
        let mut edit = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for (i, clip) in self.clips.iter().enumerate() {
                egui::Frame::group(ui.style()).inner_margin(8.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&clip.when).strong());
                            let trimmed = if clip.path.to_string_lossy().contains("_trim") { "  ·  trimmed" } else { "" };
                            ui.label(RichText::new(format!("{:.1} MB{trimmed}", clip.size_mb)).small().weak());
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Delete").on_hover_text("Move to Recycle Bin").clicked() {
                                recycle = Some(i);
                            }
                            if ui.button("Edit").on_hover_text("Trim this clip").clicked() {
                                edit = Some(i);
                            }
                            if ui.button("Show").on_hover_text("Show in folder").clicked() {
                                let _ = std::process::Command::new("explorer.exe")
                                    .raw_arg(format!("/select,\"{}\"", clip.path.display()))
                                    .spawn();
                            }
                            if ui.button(RichText::new("Play").strong()).clicked() {
                                let _ = std::process::Command::new("explorer.exe").arg(&clip.path).spawn();
                            }
                        });
                    });
                });
            }
        });
        if let Some(i) = edit {
            let path = self.clips[i].path.clone();
            self.editor.open(&path);
            self.tab = Tab::Edit;
        }
        if let Some(i) = recycle {
            let path = self.clips[i].path.clone();
            if self.editor.busy() && self.editor.current() == Some(path.as_path()) {
                self.toast("That clip is being trimmed right now.", false);
            } else if winutil::recycle(&path) {
                self.editor.close_if(&path);
                self.toast("Moved to Recycle Bin.", true);
            } else {
                self.toast("Couldn't delete that clip.", false);
            }
            self.refresh_clips();
        }
    }

    fn edit_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Clip");
            let current = self.editor.current().map(|p| p.to_path_buf());
            let shown = current
                .as_ref()
                .and_then(|p| self.clips.iter().find(|c| &c.path == p))
                .map_or_else(|| "Choose a clip...".to_string(), clip_label);
            let mut pick = None;
            ui.add_enabled_ui(!self.editor.busy(), |ui| {
                egui::ComboBox::from_id_salt("editclip").width(ui.available_width()).selected_text(shown).show_ui(ui, |ui| {
                    for c in &self.clips {
                        if ui.selectable_label(current.as_ref() == Some(&c.path), clip_label(c)).clicked() {
                            pick = Some(c.path.clone());
                        }
                    }
                });
            });
            if let Some(p) = pick {
                self.editor.open(&p);
            }
        });
        ui.add_space(6.0);
        let (encoder, quality) = (self.encoder.clone(), self.cfg.quality);
        let event = egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| self.editor.ui(ui, &encoder, quality)).inner;
        match event {
            Some(crate::editor::EditEvent::Saved(p)) => {
                let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                self.toast(format!("Saved {name}"), true);
                self.refresh_clips();
            }
            Some(crate::editor::EditEvent::Failed(e)) => self.toast(format!("Trim failed: {e}"), false),
            None => {}
        }
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        if self.mics.is_none() {
            self.mics = Some(audio::list_mics());
            self.default_mic = audio::default_mic();
        }
        let (mut check_now, mut show_prompt) = (false, false);
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let d = &mut self.draft;

            section(ui, "Clips");
            egui::Grid::new("clips").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Clip length");
                ui.add(egui::Slider::new(&mut d.clip_seconds, 10..=300).suffix(" s"));
                ui.end_row();

                ui.label("Hotkey");
                if self.capturing_hotkey {
                    ui.add(egui::Button::new(RichText::new("Press keys... (Esc cancels)").italics()).selected(true));
                    let mut result = None;
                    ui.input(|i| {
                        for ev in &i.events {
                            if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                                if *key == egui::Key::Escape {
                                    result = Some(None);
                                } else if let Some(s) = hotkey_string(*key, *modifiers) {
                                    result = Some(Some(s));
                                }
                            }
                        }
                    });
                    if let Some(r) = result {
                        if let Some(s) = r {
                            d.hotkey = s;
                        }
                        self.capturing_hotkey = false;
                    }
                } else if ui.button(&d.hotkey).on_hover_text("Click, then press a key combination").clicked() {
                    self.capturing_hotkey = true;
                }
                ui.end_row();

                ui.label("Save to");
                ui.horizontal(|ui| {
                    let shown = if d.output_dir.is_empty() { "Videos\\Aero".to_string() } else { d.output_dir.clone() };
                    ui.add(egui::Label::new(RichText::new(shown).monospace()).truncate());
                    if ui.button("Change...").clicked()
                        && let Some(dir) = rfd::FileDialog::new().set_directory(d.clips_dir()).pick_folder()
                    {
                        d.output_dir = dir.display().to_string();
                    }
                });
                ui.end_row();
            });

            section(ui, "Video");
            egui::Grid::new("video").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Frame rate");
                egui::ComboBox::from_id_salt("fps").selected_text(format!("{} fps", d.fps)).show_ui(ui, |ui| {
                    for f in [30, 60, 120, 144] {
                        ui.selectable_value(&mut d.fps, f, format!("{f} fps"));
                    }
                });
                ui.end_row();

                ui.label("Quality");
                ui.horizontal(|ui| {
                    // Lower number = better, so show the slider "backwards" as a 1-5 scale.
                    let mut level = match d.quality { 0..=19 => 5, 20..=21 => 4, 22..=24 => 3, 25..=27 => 2, _ => 1 };
                    let names = ["", "Low", "Medium", "Good", "High", "Best"];
                    if ui.add(egui::Slider::new(&mut level, 1..=5).show_value(false)).changed() {
                        d.quality = [0, 29, 26, 23, 20, 18][level];
                    }
                    ui.label(names[level]);
                });
                ui.end_row();

                ui.label("Monitor");
                egui::ComboBox::from_id_salt("mon").selected_text(format!("Monitor {}", d.monitor + 1)).show_ui(ui, |ui| {
                    for m in 0..winutil::monitor_count() {
                        ui.selectable_value(&mut d.monitor, m, format!("Monitor {}", m + 1));
                    }
                });
                ui.end_row();

                ui.label("Encoder");
                let encoders = [
                    ("auto", "Automatic"),
                    ("h264_nvenc", "NVIDIA (H.264)"),
                    ("hevc_nvenc", "NVIDIA (HEVC, smaller)"),
                    ("h264_amf", "AMD"),
                    ("h264_qsv", "Intel"),
                    ("libx264", "CPU (slow)"),
                ];
                let current = encoders.iter().find(|e| e.0 == d.encoder).map_or(d.encoder.as_str(), |e| e.1);
                let current = if d.encoder == "auto" { format!("Automatic ({})", self.encoder) } else { current.to_string() };
                egui::ComboBox::from_id_salt("enc").selected_text(current).show_ui(ui, |ui| {
                    for (id, name) in encoders {
                        ui.selectable_value(&mut d.encoder, id.to_string(), name);
                    }
                });
                ui.end_row();

                ui.label("Capture");
                let captures = [
                    ("auto", "Automatic"),
                    ("wgc", "Windows Graphics Capture"),
                    ("dda", "Desktop Duplication (older)"),
                ];
                let current = captures.iter().find(|c| c.0 == d.capture).map_or("Automatic", |c| c.1);
                egui::ComboBox::from_id_salt("cap").selected_text(current).show_ui(ui, |ui| {
                    for (id, name) in captures {
                        ui.selectable_value(&mut d.capture, id.to_string(), name);
                    }
                })
                .response
                .on_hover_text("If a fullscreen game shows up frozen or black in clips, try the other method.");
                ui.end_row();
            });

            section(ui, "Audio");
            ui.checkbox(&mut d.system_audio, "Record game/desktop audio (track 1)");
            ui.checkbox(&mut d.microphone, "Record microphone (track 2)");
            ui.add_enabled_ui(d.microphone, |ui| {
                egui::Grid::new("mic").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    let mics = self.mics.clone().unwrap_or_default();
                    ui.label("Microphone");
                    ui.horizontal(|ui| {
                        let shown = if d.microphone_device.is_empty() { "Windows default".into() } else { d.microphone_device.clone() };
                        egui::ComboBox::from_id_salt("micdev").width(210.0).selected_text(shown).show_ui(ui, |ui| {
                            ui.selectable_value(&mut d.microphone_device, String::new(), "Windows default");
                            for (name, _) in &mics {
                                ui.selectable_value(&mut d.microphone_device, name.clone(), name);
                            }
                        });
                        if ui.button("Rescan").on_hover_text("Look for newly plugged-in mics").clicked() {
                            self.mics = None;
                        }
                    });
                    ui.end_row();

                    let wanted = if d.microphone_device.is_empty() {
                        self.default_mic.clone().unwrap_or_default()
                    } else {
                        d.microphone_device.clone()
                    };
                    let channels = mics
                        .iter()
                        .find(|(n, _)| !wanted.is_empty() && n.contains(&wanted))
                        .map_or(2, |m| m.1);
                    if channels > 1 {
                        ui.label("Input");
                        egui::ComboBox::from_id_salt("micch")
                            .selected_text(format!("Input {}", d.microphone_channel))
                            .show_ui(ui, |ui| {
                                for c in 1..=channels as u32 {
                                    ui.selectable_value(&mut d.microphone_channel, c, format!("Input {c}"));
                                }
                            })
                            .response
                            .on_hover_text("Audio interfaces have several inputs, e.g. a Scarlett's mic can be on input 2.");
                        ui.end_row();
                        ui.label("");
                        ui.label(
                            RichText::new("Not sure? Save, talk, and watch the Microphone meter at the top. If it stays empty, try another input.")
                                .small()
                                .weak(),
                        );
                        ui.end_row();
                    }

                    ui.label("Mic volume");
                    ui.add(egui::Slider::new(&mut d.microphone_volume, 0.0..=4.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
                    ui.end_row();
                });
            });

            section(ui, "General");
            if ui.checkbox(&mut self.autostart, "Start with Windows (in the tray)").changed() {
                set_autostart(self.autostart);
                self.autostart = autostart_enabled();
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.link("Logs").clicked() {
                    open_folder(self.paths.config.parent().unwrap_or(&self.paths.config));
                }
                ui.label(RichText::new("- aero.log and ffmpeg.log").small().weak());
            });

            section(ui, "Updates");
            ui.horizontal(|ui| {
                ui.label(format!("Aero {}", updater::current()));
                let checking = matches!(self.update, UpdateState::Checking | UpdateState::Installing(_));
                if ui.add_enabled(!checking, egui::Button::new("Check for updates")).clicked() {
                    check_now = true;
                }
                match &self.update {
                    UpdateState::Checking => {
                        ui.spinner();
                    }
                    UpdateState::UpToDate => {
                        ui.label(RichText::new("You're up to date").color(Color32::from_rgb(76, 175, 80)));
                    }
                    UpdateState::Available(r) => {
                        if ui.link(format!("Aero {} is available", r.version)).clicked() {
                            show_prompt = true;
                        }
                    }
                    UpdateState::Installing(_) => {
                        ui.label("Updating...");
                    }
                    UpdateState::Failed(e) => {
                        ui.label(RichText::new("Check failed").color(Color32::from_rgb(229, 57, 53))).on_hover_text(e.as_str());
                    }
                    UpdateState::Idle => {}
                }
            });
            ui.checkbox(&mut d.check_updates, "Check for updates automatically (once a day)");
            ui.add_space(8.0);
        });
        if check_now {
            self.check_for_updates(true);
        }
        if show_prompt {
            self.update_prompt = true;
        }
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        let dirty = self.draft != self.cfg;
        ui.horizontal(|ui| {
            if let Some((text, ok, at)) = &self.toast
                && at.elapsed() < Duration::from_secs(6)
            {
                let color = if *ok { Color32::from_rgb(76, 175, 80) } else { Color32::from_rgb(229, 57, 53) };
                ui.add(egui::Label::new(RichText::new(text).color(color)).truncate());
                ui.ctx().request_repaint_after(Duration::from_secs(1));
            } else if dirty {
                // Settings only take effect after Save; make that impossible to miss.
                let text = if self.tab == Tab::Settings { "Unsaved changes" } else { "Unsaved settings changes" };
                ui.label(RichText::new(text).color(Color32::from_rgb(255, 167, 38)).strong());
            }
            if self.tab == Tab::Settings {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let save = egui::Button::new(RichText::new("Save").strong().color(if dirty { Color32::WHITE } else { ui.visuals().weak_text_color() }))
                        .fill(if dirty { Color32::from_rgb(211, 47, 47) } else { ui.visuals().widgets.inactive.weak_bg_fill });
                    if ui.add_enabled(dirty, save).clicked() {
                        self.apply_settings();
                    }
                    if ui.add_enabled(dirty, egui::Button::new("Undo")).clicked() {
                        self.draft = self.cfg.clone();
                    }
                });
            }
        });
    }
}

impl eframe::App for AeroApp {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // eframe always shows the window after its first frame, so --tray hides it on the second.
        if self.hide_after_frames > 0 {
            self.hide_after_frames -= 1;
            if self.hide_after_frames == 0 {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                ui.ctx().request_repaint();
            }
        }

        // Closing the window just hides it to the tray.
        if ui.ctx().input(|i| i.viewport().close_requested()) && !self.quitting {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.capturing_hotkey = false;
        }

        egui::Panel::top("header").frame(egui::Frame::side_top_panel(ui.style()).inner_margin(12.0)).show(ui, |ui| {
            self.header(ui);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Clips, RichText::new("Clips").size(15.0));
                ui.selectable_value(&mut self.tab, Tab::Edit, RichText::new("Edit").size(15.0));
                ui.selectable_value(&mut self.tab, Tab::Settings, RichText::new("Settings").size(15.0));
            });
        });
        egui::Panel::bottom("footer").frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::symmetric(12, 8))).show(ui, |ui| {
            self.footer(ui);
        });
        egui::CentralPanel::default().frame(egui::Frame::central_panel(ui.style()).inner_margin(12.0)).show(ui, |ui| match self.tab {
            Tab::Clips => self.clips_tab(ui),
            Tab::Edit => self.edit_tab(ui),
            Tab::Settings => self.settings_tab(ui),
        });

        self.update_dialog(ui.ctx());

        if self.rec.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(50)); // level meters
        }
    }
}

// -------------------------------------------------------------------- helpers

fn style(ctx: &egui::Context) {
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 7.0);
        s.spacing.button_padding = Vec2::new(10.0, 5.0);
        s.spacing.slider_width = 190.0;
        for (style, size) in [
            (egui::TextStyle::Body, 14.0),
            (egui::TextStyle::Button, 14.0),
            (egui::TextStyle::Small, 11.5),
            (egui::TextStyle::Heading, 18.0),
        ] {
            if let Some(f) = s.text_styles.get_mut(&style) {
                f.size = size;
            }
        }
    });
}

/// Release notes as plain text: Markdown markers and link targets removed, checksum line
/// dropped, and symbols the default font lacks spelled out.
fn release_notes_text(md: &str) -> String {
    md.lines()
        .filter(|l| !l.contains("SHA-256"))
        .map(|l| {
            let l = l.trim_start_matches('#').trim_start();
            let l = strip_md_links(l).replace("**", "").replace('`', "").replace('←', "Left").replace('→', "Right");
            match l.strip_prefix("- ") {
                Some(rest) => format!("•  {rest}"),
                None => l,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// `[text](url)` -> `text`.
fn strip_md_links(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find("](").map(|i| open + i) else { break };
        let Some(end) = rest[close..].find(')').map(|i| close + i) else { break };
        out.push_str(&rest[..open]);
        out.push_str(&rest[open + 1..close]);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

fn clip_label(c: &Clip) -> String {
    let trimmed = if c.path.to_string_lossy().contains("_trim") { " (trimmed)" } else { "" };
    format!("{}{trimmed}  -  {:.1} MB", c.when, c.size_mb)
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).strong().size(15.0));
    ui.separator();
}

/// A horizontal level meter: green → yellow → red, on a dB scale.
fn meter(ui: &mut egui::Ui, peak: f32) {
    let width = ui.available_width().max(60.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 8.0), egui::Sense::hover());
    let db = 20.0 * peak.max(1e-6).log10();
    let fill = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let p = ui.painter();
    p.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
    let mut bar = rect;
    bar.set_width(rect.width() * fill);
    let color = if db > -3.0 {
        Color32::from_rgb(229, 57, 53)
    } else if db > -12.0 {
        Color32::from_rgb(253, 216, 53)
    } else {
        Color32::from_rgb(76, 175, 80)
    };
    p.rect_filled(bar, 3.0, color);
}

/// Turns a key press into a hotkey string the `global-hotkey` crate understands.
fn hotkey_string(key: egui::Key, m: egui::Modifiers) -> Option<String> {
    let name = key.name();
    let is_fkey = name.len() >= 2 && name.starts_with('F') && name[1..].chars().all(|c| c.is_ascii_digit());
    let is_alnum = name.len() == 1 && name.chars().all(|c| c.is_ascii_alphanumeric());
    if !(is_fkey || is_alnum) {
        return None;
    }
    // Plain letters/digits would hijack normal typing everywhere, so require a modifier.
    if !is_fkey && !(m.ctrl || m.alt || m.shift) {
        return None;
    }
    let mut s = String::new();
    if m.ctrl {
        s += "Ctrl+";
    }
    if m.alt {
        s += "Alt+";
    }
    if m.shift {
        s += "Shift+";
    }
    s += name;
    s.parse::<HotKey>().ok().map(|_| s)
}

fn open_folder(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    let _ = std::process::Command::new("explorer.exe").arg(dir).spawn();
}

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

fn reg(args: &[&str]) -> bool {
    recorder::hidden_cmd("reg.exe")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn autostart_enabled() -> bool {
    reg(&["query", RUN_KEY, "/v", "Aero"])
}

fn set_autostart(on: bool) {
    if on {
        let exe = std::env::current_exe().unwrap_or_default();
        reg(&["add", RUN_KEY, "/v", "Aero", "/t", "REG_SZ", "/d", &format!("\"{}\" --tray", exe.display()), "/f"]);
    } else {
        reg(&["delete", RUN_KEY, "/v", "Aero", "/f"]);
    }
}

pub const ICON: u32 = 32;

/// The 32x32 tray/window icon: red while recording, grey while paused.
pub fn icon_rgba(recording: bool) -> Vec<u8> {
    crate::icon::icon_rgba(ICON as usize, recording)
}
