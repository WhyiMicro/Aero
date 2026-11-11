//! The Edit tab: trim a clip from the left and right.
//!
//! Frames for the preview and the thumbnail strip come from ffmpeg as raw RGBA over a pipe
//! (no image decoder needed). All ffmpeg work runs on background threads; the UI only
//! uploads finished images as textures.

use crate::recorder::hidden_cmd;
use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Stroke, TextureHandle, Vec2};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};

const PREVIEW_W: usize = 640;
const MIN_LEN: f64 = 0.5; // shortest clip you can trim to, in seconds
const ACCENT: Color32 = Color32::from_rgb(229, 57, 53);

pub enum EditEvent {
    /// A trimmed clip was written (possibly replacing the original).
    Saved(PathBuf),
    Failed(String),
}

struct Info {
    duration: f64,
    width: usize,
    height: usize,
    fps: f64,
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Start,
    End,
    Playhead,
}

struct Export {
    progress: Arc<AtomicU32>, // permille
    child: Arc<Mutex<Option<Child>>>,
    rx: mpsc::Receiver<Result<PathBuf, String>>,
}

/// Messages from worker threads.
enum Work {
    Info(PathBuf, Result<Info, String>),
    Frame(PathBuf, f64, Vec<u8>, [usize; 2]),
    Strip(PathBuf, Vec<u8>, [usize; 2]),
}

pub struct Editor {
    ctx: egui::Context,
    ffmpeg: String,
    path: Option<PathBuf>,
    info: Option<Info>,
    error: Option<String>,
    start: f64,
    end: f64,
    playhead: f64,
    drag: Option<Drag>,
    frame: Option<TextureHandle>,
    frame_t: Option<f64>,
    requested_t: Option<f64>,
    strip: Option<TextureHandle>,
    reencode: bool,
    replace: bool,
    export: Option<Export>,
    frame_tx: mpsc::Sender<(PathBuf, f64, [usize; 2])>,
    tx: mpsc::Sender<Work>,
    rx: mpsc::Receiver<Work>,
    mpv: Option<String>,
}

impl Editor {
    pub fn new(ctx: egui::Context, ffmpeg: String) -> Self {
        let (tx, rx) = mpsc::channel();
        let frame_tx = spawn_frame_worker(ctx.clone(), ffmpeg.clone(), tx.clone());
        let mpv = crate::recorder::resolve_ffmpeg("mpv");
        let mpv = Path::new(&mpv).is_file().then_some(mpv);
        Self {
            ctx,
            ffmpeg,
            path: None,
            info: None,
            error: None,
            start: 0.0,
            end: 0.0,
            playhead: 0.0,
            drag: None,
            frame: None,
            frame_t: None,
            requested_t: None,
            strip: None,
            reencode: false,
            replace: false,
            export: None,
            frame_tx,
            tx,
            rx,
            mpv,
        }
    }

    pub fn current(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn busy(&self) -> bool {
        self.export.is_some()
    }

    /// Loads a clip into the editor (resets the trim range).
    pub fn open(&mut self, path: &Path) {
        if self.busy() {
            return;
        }
        self.path = Some(path.to_path_buf());
        self.info = None;
        self.error = None;
        self.frame = None;
        self.frame_t = None;
        self.requested_t = None;
        self.strip = None;
        self.drag = None;
        let (tx, ctx, ffmpeg, p) = (self.tx.clone(), self.ctx.clone(), self.ffmpeg.clone(), path.to_path_buf());
        std::thread::spawn(move || {
            let info = probe(&ffmpeg, &p);
            let dims = info.as_ref().ok().map(|i| Info { duration: i.duration, width: i.width, height: i.height, fps: i.fps });
            let _ = tx.send(Work::Info(p.clone(), info));
            ctx.request_repaint();
            // The thumbnail strip takes a moment longer; the editor is usable before it arrives.
            if let Some(i) = dims
                && let Some((rgba, size)) = filmstrip(&ffmpeg, &p, &i)
            {
                let _ = tx.send(Work::Strip(p, rgba, size));
                ctx.request_repaint();
            }
        });
    }

    /// Forget the clip if it was deleted elsewhere.
    pub fn close_if(&mut self, path: &Path) {
        if self.path.as_deref() == Some(path) && !self.busy() {
            self.path = None;
            self.info = None;
        }
    }

    fn poll(&mut self) -> Option<EditEvent> {
        while let Ok(w) = self.rx.try_recv() {
            match w {
                Work::Info(p, r) if Some(&p) == self.path.as_ref() => match r {
                    Ok(i) => {
                        self.start = 0.0;
                        self.end = i.duration;
                        self.playhead = 0.0;
                        self.info = Some(i);
                    }
                    Err(e) => self.error = Some(e),
                },
                Work::Frame(p, t, rgba, size) if Some(&p) == self.path.as_ref() => {
                    let img = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                    self.frame = Some(self.ctx.load_texture("edit-frame", img, egui::TextureOptions::LINEAR));
                    self.frame_t = Some(t);
                }
                Work::Strip(p, rgba, size) if Some(&p) == self.path.as_ref() => {
                    let img = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                    self.strip = Some(self.ctx.load_texture("edit-strip", img, egui::TextureOptions::LINEAR));
                }
                _ => {} // stale result for a clip that's no longer open
            }
        }
        if let Some(ex) = &self.export
            && let Ok(r) = ex.rx.try_recv()
        {
            self.export = None;
            return Some(match r {
                Ok(p) => {
                    if self.path.as_ref() == Some(&p) {
                        self.open(&p.clone()); // replaced the original: reload it
                    }
                    EditEvent::Saved(p)
                }
                Err(e) => EditEvent::Failed(e),
            });
        }
        None
    }

    fn request_frame(&mut self) {
        let (Some(path), Some(info)) = (&self.path, &self.info) else { return };
        if self.requested_t.is_some_and(|t| (t - self.playhead).abs() < 0.001) {
            return;
        }
        self.requested_t = Some(self.playhead);
        let h = even((PREVIEW_W as f64 * info.height as f64 / info.width.max(1) as f64) as usize);
        // The very last frame of a file can't be seeked to; stay a hair before the end.
        let t = self.playhead.min((info.duration - 1.5 / info.fps).max(0.0));
        let _ = self.frame_tx.send((path.clone(), t, [PREVIEW_W, h]));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, encoder: &str, quality: u32) -> Option<EditEvent> {
        let event = self.poll();

        let Some(path) = self.path.clone() else {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Pick a clip to trim").size(16.0));
                ui.label(RichText::new("Use the list above, or click Edit next to a clip in the Clips tab.").weak());
            });
            return event;
        };
        if let Some(e) = &self.error {
            ui.colored_label(ACCENT, format!("Couldn't open this clip: {e}"));
            return event;
        }
        let Some(info) = &self.info else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading clip...");
            });
            return event;
        };
        let (duration, fps) = (info.duration, info.fps);
        let aspect = info.height as f32 / info.width.max(1) as f32;

        // ---- preview frame: as large as fits while keeping the controls below it on screen
        let w = ui.available_width();
        let h = (w * aspect).min((ui.available_height() - 215.0).max(110.0));
        let w = h / aspect;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
        let img_rect = Rect::from_center_size(rect.center(), Vec2::new(w, h));
        ui.painter().rect_filled(img_rect, 4.0, Color32::BLACK);
        if let Some(tex) = &self.frame {
            ui.painter().image(tex.id(), img_rect, full_uv(), Color32::WHITE);
        }
        if self.frame_t.is_none_or(|t| (t - self.playhead).abs() > 0.02) {
            ui.painter().text(img_rect.right_top() + Vec2::new(-8.0, 8.0), egui::Align2::RIGHT_TOP, "...", egui::FontId::proportional(14.0), Color32::WHITE);
        }
        ui.add_space(6.0);

        // ---- timeline
        let busy = self.busy();
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 56.0), if busy { Sense::hover() } else { Sense::click_and_drag() });
        let resp = resp.on_hover_text(
            "Drag the red handles to trim. Click anywhere to look at that moment.\n\
             Keys: Left/Right move 0.1 s (Shift: 1 s, Ctrl: 1 frame). I / O set the start / end.",
        );
        let x_of = |t: f64| rect.left() + (t / duration) as f32 * rect.width();
        let t_of = |x: f32| (((x - rect.left()) / rect.width()) as f64 * duration).clamp(0.0, duration);
        // Handles are drawn just inside the selection: the start handle to the right of its
        // edge, the end handle to the left of its edge.
        let start_c = |s: f64| x_of(s) + 5.0;
        let end_c = |e: f64| x_of(e) - 5.0;
        const GRAB: f32 = 12.0;

        if let Some(pos) = resp.hover_pos()
            && ((pos.x - start_c(self.start)).abs() <= GRAB || (pos.x - end_c(self.end)).abs() <= GRAB)
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        // Decide what's being dragged from where the button went down: egui reports the drag
        // only after the pointer has already moved a few pixels.
        if resp.drag_started()
            && let Some(pos) = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos())
        {
            let (ds, de) = ((pos.x - start_c(self.start)).abs(), (pos.x - end_c(self.end)).abs());
            self.drag = Some(if ds <= GRAB && ds <= de {
                Drag::Start
            } else if de <= GRAB {
                Drag::End
            } else {
                Drag::Playhead
            });
        }
        if (resp.dragged() || resp.clicked())
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let t = t_of(pos.x);
            match self.drag.unwrap_or(Drag::Playhead) {
                Drag::Start => {
                    // pointer sits on the handle centre, 5 px inside the edge
                    self.start = t_of(pos.x - 5.0).min(self.end - MIN_LEN).max(0.0);
                    self.playhead = self.start;
                }
                Drag::End => {
                    self.end = t_of(pos.x + 5.0).max(self.start + MIN_LEN).min(duration);
                    self.playhead = self.end;
                }
                Drag::Playhead => self.playhead = t,
            }
        }
        if resp.drag_stopped() {
            self.drag = None;
        }

        let p = ui.painter();
        p.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
        if let Some(strip) = &self.strip {
            p.image(strip.id(), rect, full_uv(), Color32::WHITE);
        }
        let (xs, xe) = (x_of(self.start), x_of(self.end));
        let shade = Color32::from_black_alpha(185);
        p.rect_filled(Rect::from_min_max(rect.min, Pos2::new(xs, rect.bottom())), 0.0, shade);
        p.rect_filled(Rect::from_min_max(Pos2::new(xe, rect.top()), rect.max), 0.0, shade);
        // selection frame + handles
        let stroke = Stroke::new(3.0, ACCENT);
        p.line_segment([Pos2::new(xs, rect.top() + 1.5), Pos2::new(xe, rect.top() + 1.5)], stroke);
        p.line_segment([Pos2::new(xs, rect.bottom() - 1.5), Pos2::new(xe, rect.bottom() - 1.5)], stroke);
        for (x, left) in [(xs, true), (xe, false)] {
            let r = if left { Rect::from_min_max(Pos2::new(x, rect.top()), Pos2::new(x + 10.0, rect.bottom())) } else { Rect::from_min_max(Pos2::new(x - 10.0, rect.top()), Pos2::new(x, rect.bottom())) };
            p.rect_filled(r, 2.0, ACCENT);
            let cx = r.center().x;
            for dx in [-1.5, 1.5] {
                p.line_segment([Pos2::new(cx + dx, r.center().y - 7.0), Pos2::new(cx + dx, r.center().y + 7.0)], Stroke::new(1.0, Color32::WHITE));
            }
        }
        let xp = x_of(self.playhead);
        p.line_segment([Pos2::new(xp, rect.top() - 3.0), Pos2::new(xp, rect.bottom() + 3.0)], Stroke::new(2.0, Color32::WHITE));
        p.circle_filled(Pos2::new(xp, rect.top() - 3.0), 4.0, Color32::WHITE);

        // ---- times + keyboard
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} - {}", fmt_time(self.start), fmt_time(self.end))).monospace());
            ui.label(RichText::new(format!("length {}", fmt_time(self.end - self.start))).monospace().strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("at {}", fmt_time(self.playhead))).monospace().weak());
            });
        });
        if !busy && ui.memory(|m| m.focused().is_none()) {
            let (mut dt, mut set_start, mut set_end) = (0.0, false, false);
            ui.input(|i| {
                let step = if i.modifiers.shift { 1.0 } else if i.modifiers.ctrl { 1.0 / fps } else { 0.1 };
                if i.key_pressed(egui::Key::ArrowLeft) {
                    dt -= step;
                }
                if i.key_pressed(egui::Key::ArrowRight) {
                    dt += step;
                }
                set_start = i.key_pressed(egui::Key::I);
                set_end = i.key_pressed(egui::Key::O);
            });
            self.playhead = (self.playhead + dt).clamp(0.0, duration);
            if set_start {
                self.start = self.playhead.min(self.end - MIN_LEN).max(0.0);
            }
            if set_end {
                self.end = self.playhead.max(self.start + MIN_LEN).min(duration);
            }
        }
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Set start here").on_hover_text("Shortcut: I").clicked() {
                    self.start = self.playhead.min(self.end - MIN_LEN).max(0.0);
                }
                if ui.button("Set end here").on_hover_text("Shortcut: O").clicked() {
                    self.end = self.playhead.max(self.start + MIN_LEN).min(duration);
                }
                if ui.button("Reset").clicked() {
                    self.start = 0.0;
                    self.end = duration;
                }
                if let Some(mpv) = &self.mpv
                    && ui.button("Play selection").on_hover_text("Plays just the trimmed part (in mpv)").clicked()
                {
                    let _ = std::process::Command::new(mpv)
                        .arg(format!("--start={:.3}", self.start))
                        .arg(format!("--end={:.3}", self.end))
                        .arg("--force-window=yes")
                        .arg(&path)
                        .spawn();
                }
            });
        });
        ui.add_space(4.0);

        // ---- export
        if let Some(ex) = &self.export {
            let f = ex.progress.load(Ordering::Relaxed) as f32 / 1000.0;
            ui.horizontal(|ui| {
                ui.add(egui::ProgressBar::new(f).show_percentage().desired_width(ui.available_width() - 80.0));
                if ui.button("Cancel").clicked()
                    && let Some(c) = ex.child.lock().unwrap().as_mut()
                {
                    let _ = c.kill();
                }
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        } else {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.replace, "Replace original").on_hover_text(
                    "Overwrite the clip instead of saving a new ..._trim.mp4. The original goes to the Recycle Bin.",
                );
                ui.checkbox(&mut self.reencode, "Re-encode (slower)").on_hover_text(
                    "Saving is normally instant and lossless, and players start exactly at your handle. \
                     A few apps ignore that and show up to 2 s extra at the start; tick this to re-encode \
                     on your GPU so the file itself starts exactly there.",
                );
            });
            let whole = self.start <= 0.0001 && self.end >= duration - 0.0001;
            let label = if self.replace { "Save trimmed clip (replace original)" } else { "Save trimmed clip" };
            let button = egui::Button::new(RichText::new(label).size(15.0).strong().color(Color32::WHITE))
                .fill(ACCENT)
                .min_size(Vec2::new(ui.available_width(), 36.0));
            if ui.add_enabled(!whole, button).on_disabled_hover_text("Drag a handle first").clicked() {
                self.start_export(encoder, quality);
            }
        }

        self.request_frame();
        event
    }

    fn start_export(&mut self, encoder: &str, quality: u32) {
        let Some(src) = self.path.clone() else { return };
        let (start, len) = (self.start, self.end - self.start);
        let replace = self.replace;
        let out = if replace { src.with_extension("trimming.mp4") } else { free_name(&src) };
        let progress = Arc::new(AtomicU32::new(0));
        let child = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel();

        let mut cmd = hidden_cmd(&self.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostats", "-progress", "pipe:1", "-y"]);
        if self.reencode {
            // With NVENC, decode on the GPU too and keep frames there (about 1.5x faster).
            if encoder.ends_with("_nvenc") {
                cmd.args(["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]);
            } else {
                cmd.args(["-hwaccel", "auto"]);
            }
        }
        cmd.args(["-ss", &format!("{start:.3}"), "-i"]).arg(&src);
        cmd.args(["-t", &format!("{len:.3}"), "-map", "0"]);
        if self.reencode {
            cmd.args(video_args(encoder, quality));
            cmd.args(["-c:a", "copy"]);
        } else {
            // Stream copy: instant and lossless. The cut starts at the keyframe before the
            // handle, and an MP4 edit list makes players skip to the exact frame.
            cmd.args(["-c", "copy"]);
        }
        cmd.args(["-map_metadata", "0", "-movflags", "+faststart"]).arg(&out);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        crate::info!("trimming {} [{start:.2}s +{len:.2}s] reencode={} -> {}", src.display(), self.reencode, out.display());

        let (p, c, ctx) = (progress.clone(), child.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = (|| -> Result<PathBuf, String> {
                let mut ch = cmd.spawn().map_err(|e| format!("couldn't run ffmpeg: {e}"))?;
                let stdout = ch.stdout.take().unwrap();
                let mut stderr = ch.stderr.take().unwrap();
                *c.lock().unwrap() = Some(ch);
                let err_thread = std::thread::spawn(move || {
                    let mut s = String::new();
                    let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
                    s
                });
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.parse::<f64>().ok()) {
                        p.store(((us / 1e6 / len).clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
                        ctx.request_repaint();
                    }
                }
                let status = c.lock().unwrap().take().map(|mut ch| ch.wait());
                let errors = err_thread.join().unwrap_or_default();
                if !matches!(status, Some(Ok(s)) if s.success()) {
                    let _ = std::fs::remove_file(&out);
                    return Err(if errors.trim().is_empty() { "Cancelled.".into() } else { errors.trim().to_string() });
                }
                if replace {
                    // Keep the original's name and date, so the clip stays in its place in the list.
                    let modified = std::fs::metadata(&src).and_then(|m| m.modified()).ok();
                    if !crate::winutil::recycle(&src) {
                        return Err(format!("Trimmed clip saved as {}, but the original couldn't be removed.", out.display()));
                    }
                    std::fs::rename(&out, &src).map_err(|e| format!("renaming the trimmed clip: {e}"))?;
                    if let (Some(t), Ok(f)) = (modified, std::fs::File::options().write(true).open(&src)) {
                        let _ = f.set_modified(t);
                    }
                    Ok(src)
                } else {
                    Ok(out)
                }
            })();
            if let Err(e) = &result {
                crate::info!("trim failed: {e}");
            }
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.export = Some(Export { progress, child, rx });
    }
}

fn video_args(encoder: &str, q: u32) -> Vec<String> {
    let q = q.to_string();
    let mut v: Vec<&str> = vec!["-c:v", encoder];
    if encoder.ends_with("_nvenc") {
        // Frames arrive as CUDA surfaces (see start_export), so no pixel format conversion.
        v.extend(["-preset", "p4", "-rc", "vbr", "-cq", &q, "-b:v", "0"]);
    } else if encoder.ends_with("_amf") {
        v.extend(["-quality", "quality", "-rc", "cqp", "-qp_i", &q, "-qp_p", &q, "-pix_fmt", "yuv420p"]);
    } else if encoder.ends_with("_qsv") {
        v.extend(["-global_quality", &q, "-pix_fmt", "nv12"]);
    } else {
        v.extend(["-preset", "fast", "-crf", &q, "-pix_fmt", "yuv420p"]);
    }
    v.into_iter().map(String::from).collect()
}

/// `Aero_..._trim.mp4`, `Aero_..._trim2.mp4`, ... next to the source.
fn free_name(src: &Path) -> PathBuf {
    let stem = src.file_stem().unwrap_or_default().to_string_lossy();
    let dir = src.parent().unwrap_or(Path::new("."));
    (1..)
        .map(|n| dir.join(if n == 1 { format!("{stem}_trim.mp4") } else { format!("{stem}_trim{n}.mp4") }))
        .find(|p| !p.exists())
        .unwrap()
}

fn full_uv() -> Rect {
    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))
}

fn even(n: usize) -> usize {
    (n.max(2) / 2) * 2
}

pub fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    format!("{}:{:04.1}", (t / 60.0) as u32, t % 60.0)
}

/// Duration, size and frame rate, from ffmpeg's stream info.
fn probe(ffmpeg: &str, path: &Path) -> Result<Info, String> {
    let out = hidden_cmd(ffmpeg).args(["-hide_banner", "-i"]).arg(path).output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stderr);
    let duration = text
        .split("Duration: ")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .and_then(|hms| {
            let mut p = hms.trim().split(':').map(|x| x.parse::<f64>().ok());
            Some(p.next()?? * 3600.0 + p.next()?? * 60.0 + p.next()??)
        })
        .ok_or("no duration found")?;
    let video = text.lines().find(|l| l.contains("Video:")).ok_or("no video stream")?;
    let (width, height) = video
        .split([' ', ','])
        .find_map(|tok| {
            let (w, h) = tok.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .ok_or("no video size")?;
    let fps = video
        .split(',')
        .find_map(|part| part.trim().strip_suffix(" fps")?.parse::<f64>().ok())
        .unwrap_or(60.0);
    Ok(Info { duration, width, height, fps })
}

/// One small thumbnail per keyframe (decoding only keyframes is fast), laid side by side.
fn filmstrip(ffmpeg: &str, path: &Path, info: &Info) -> Option<(Vec<u8>, [usize; 2])> {
    let keyframes = (info.duration / 2.0).ceil().max(1.0) as usize;
    let tw = even((8192 / keyframes).min(96));
    let th = even((tw as f64 * info.height as f64 / info.width.max(1) as f64) as usize);
    let out = hidden_cmd(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-skip_frame", "nokey", "-i"])
        .arg(path)
        .args(["-an", "-fps_mode", "passthrough", "-vf", &format!("scale={tw}:{th}"), "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let frame = tw * th * 4;
    let n = out.stdout.len() / frame;
    if n == 0 {
        return None;
    }
    let mut strip = vec![0u8; n * frame];
    for i in 0..n {
        for y in 0..th {
            let src = &out.stdout[i * frame + y * tw * 4..i * frame + (y + 1) * tw * 4];
            let dst = y * n * tw * 4 + i * tw * 4;
            strip[dst..dst + tw * 4].copy_from_slice(src);
        }
    }
    Some((strip, [n * tw, th]))
}

/// Background thread that decodes preview frames. Only the newest request matters, so
/// requests that pile up while one is decoding are skipped.
fn spawn_frame_worker(ctx: egui::Context, ffmpeg: String, tx: mpsc::Sender<Work>) -> mpsc::Sender<(PathBuf, f64, [usize; 2])> {
    let (req_tx, req_rx) = mpsc::channel::<(PathBuf, f64, [usize; 2])>();
    std::thread::spawn(move || {
        while let Ok(mut req) = req_rx.recv() {
            while let Ok(newer) = req_rx.try_recv() {
                req = newer;
            }
            let (path, t, [w, h]) = req;
            let out = hidden_cmd(&ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-ss", &format!("{t:.3}"), "-i"])
                .arg(&path)
                .args(["-frames:v", "1", "-an", "-vf", &format!("scale={w}:{h}"), "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
                .stdin(Stdio::null())
                .output();
            if let Ok(o) = out
                && o.stdout.len() == w * h * 4
            {
                let _ = tx.send(Work::Frame(path, t, o.stdout, [w, h]));
                ctx.request_repaint();
            }
        }
    });
    req_tx
}
