//! The replay buffer.
//!
//! ffmpeg captures the screen (Desktop Duplication via `ddagrab`) and writes 2-second
//! MPEG-TS segments into a temp folder. A supervisor thread deletes segments older than
//! the clip length and restarts ffmpeg if it dies (e.g. after a resolution change).
//! Saving a clip = byte-concatenating the newest segments and remuxing to MP4 (no re-encode,
//! so it's instant).

use crate::audio::{self, Kind, Source};
use crate::config::Config;
use anyhow::{Context, Result, bail};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const SEG_SECS: u32 = 2;
const STALL_TIMEOUT: Duration = Duration::from_secs(8);

/// Which screen capture ffmpeg should use. "wgc" = Windows.Graphics.Capture (ffmpeg's
/// `gfxcapture`, ffmpeg 8+), which copes with games switching display modes. "dda" = Desktop
/// Duplication (`ddagrab`), the older API. "auto" picks WGC when this ffmpeg has it.
pub fn resolve_capture(cfg: &Config) -> &'static str {
    match cfg.capture.as_str() {
        "dda" => "dda",
        "wgc" => "wgc",
        _ => {
            let out = hidden_cmd(&cfg.ffmpeg).args(["-hide_banner", "-h", "filter=gfxcapture"]).output();
            let has_wgc = out.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("Filter gfxcapture"));
            if has_wgc { "wgc" } else { "dda" }
        }
    }
}
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A command that runs without flashing a console window.
pub fn hidden_cmd(program: &str) -> Command {
    let mut c = Command::new(program);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

/// Turns "ffmpeg" into the real ffmpeg.exe path. Package-manager shims (e.g. Scoop's) launch
/// ffmpeg as a child process, and killing the shim would leave the real recorder running.
pub fn resolve_ffmpeg(spec: &str) -> String {
    let p = Path::new(spec);
    let exe = if p.components().count() > 1 {
        Some(p.to_path_buf())
    } else {
        let name = if p.extension().is_some() { spec.to_string() } else { format!("{spec}.exe") };
        std::env::var_os("PATH")
            .and_then(|path| std::env::split_paths(&path).map(|d| d.join(&name)).find(|f| f.is_file()))
    };
    let Some(exe) = exe else { return spec.into() };
    if let Ok(shim) = std::fs::read_to_string(exe.with_extension("shim")) {
        for line in shim.lines() {
            if let Some(v) = line.trim().strip_prefix("path")
                && let Some(v) = v.trim_start().strip_prefix('=')
            {
                let real = PathBuf::from(v.trim().trim_matches('"'));
                if real.is_file() {
                    return real.to_string_lossy().into_owned();
                }
            }
        }
    }
    exe.to_string_lossy().into_owned()
}

/// Picks the first hardware encoder that actually works on this machine.
pub fn pick_encoder(cfg: &Config) -> String {
    if cfg.encoder != "auto" {
        return cfg.encoder.clone();
    }
    for enc in ["h264_nvenc", "h264_amf", "h264_qsv"] {
        let ok = hidden_cmd(&cfg.ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=black:s=640x360"])
            .args(["-frames:v", "1", "-c:v", enc, "-f", "null", "-"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return enc.into();
        }
    }
    "libx264".into()
}

struct Shared {
    cfg: Config,
    encoder: String,
    capture: &'static str,
    buf_dir: PathBuf,
    ffmpeg_log: PathBuf,
    stop: AtomicBool,
    run: AtomicU32,
    /// Held while pruning or reading segments so a save never races a delete.
    seg_lock: Mutex<()>,
    /// Audio tracks of the current run, in stream order: (title, live peak level).
    tracks: Mutex<Vec<(&'static str, Arc<AtomicU32>)>>,
    /// When the watchdog last found capture stalled.
    last_stall: Mutex<Option<Instant>>,
}

pub struct Recorder {
    sh: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Recorder {
    pub fn start(cfg: Config, encoder: String, buf_dir: PathBuf, ffmpeg_log: PathBuf) -> Result<Recorder> {
        let _ = std::fs::remove_dir_all(&buf_dir);
        std::fs::create_dir_all(&buf_dir).context("creating buffer folder")?;
        let capture = resolve_capture(&cfg);
        crate::info!("recorder starting: capture={capture} encoder={encoder} fps={} clip={}s", cfg.fps, cfg.clip_seconds);
        let sh = Arc::new(Shared {
            cfg,
            encoder,
            capture,
            buf_dir,
            ffmpeg_log,
            stop: AtomicBool::new(false),
            run: AtomicU32::new(0),
            seg_lock: Mutex::new(()),
            tracks: Mutex::new(Vec::new()),
            last_stall: Mutex::new(None),
        });
        let s = sh.clone();
        let thread = std::thread::spawn(move || supervise(s));
        Ok(Recorder { sh, thread: Mutex::new(Some(thread)) })
    }

    pub fn stop(&self) {
        self.sh.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
            let _ = std::fs::remove_dir_all(&self.sh.buf_dir);
            crate::info!("recorder stopped");
        }
    }

    /// True if screen capture recently stopped producing video (typically: a fullscreen game
    /// that Windows screen capture can't see).
    pub fn capture_blocked(&self) -> bool {
        self.sh.last_stall.lock().unwrap().is_some_and(|t| t.elapsed() < STALL_TIMEOUT * 2)
    }

    /// Peak level (0..1) of each audio track since the last call, for the UI meters.
    pub fn levels(&self) -> Vec<(&'static str, f32)> {
        self.sh.tracks.lock().unwrap().iter().map(|(t, l)| (*t, f32::from_bits(l.swap(0, Ordering::Relaxed)))).collect()
    }

    /// Writes the last `clip_seconds` to an MP4 and returns its path. Blocking (~1s).
    pub fn save_clip(&self, out_dir: &Path) -> Result<PathBuf> {
        let sh = &self.sh;
        let out_dir = out_dir.to_path_buf();
        std::fs::create_dir_all(&out_dir).context("creating clips folder")?;
        let name = format!("Aero_{}.mp4", chrono::Local::now().format("%Y-%m-%d_%H-%M-%S"));
        let out = out_dir.join(name);

        let _guard = sh.seg_lock.lock().unwrap();
        let segs = list_segments(&sh.buf_dir);
        if segs.is_empty() {
            bail!("Nothing has been recorded yet - give it a couple of seconds.");
        }
        // Full segments covering the clip length, plus the one currently being written.
        let n = sh.cfg.clip_seconds.div_ceil(SEG_SECS) as usize + 1;
        let take = &segs[segs.len().saturating_sub(n)..];

        // Segments of one ffmpeg run are byte-continuous (joined with the concat protocol).
        // Runs (ffmpeg restarts, e.g. after a game went fullscreen) are joined with the concat
        // demuxer, but only runs with the same resolution as the newest one.
        let mut runs: Vec<Vec<&str>> = Vec::new();
        let mut prev_run = None;
        for s in take {
            let len = std::fs::metadata(sh.buf_dir.join(&s.name)).map_or(0, |m| m.len());
            if len == 0 {
                continue; // just created, nothing written yet
            }
            if prev_run != Some(s.run) {
                runs.push(Vec::new());
                prev_run = Some(s.run);
            }
            runs.last_mut().unwrap().push(&s.name);
        }
        // A run that started while capture was blocked (e.g. a fullscreen game) can begin with
        // segments that hold no video. Drop those, and runs with no video at all.
        let mut sizes = Vec::new();
        runs.retain_mut(|run| {
            while let Some(first) = run.first() {
                if let Some(size) = video_size(&sh.cfg.ffmpeg, &sh.buf_dir, first) {
                    sizes.push(size);
                    return true;
                }
                run.remove(0);
            }
            false
        });
        if runs.is_empty() {
            bail!(
                "There's no usable video in the buffer. Screen capture is probably blocked - \
                 fullscreen games like Minecraft need the fix in Settings > Capture (see README)."
            );
        }
        // Keep only the trailing runs with the same resolution as the newest one.
        let newest = sizes.last().unwrap().clone();
        let first_same = sizes.iter().rposition(|s| *s != newest).map_or(0, |i| i + 1);
        runs.drain(..first_same);
        let used: usize = runs.iter().map(|r| r.len()).sum();

        let mut cmd = hidden_cmd(&sh.cfg.ffmpeg);
        cmd.current_dir(&sh.buf_dir).args(["-hide_banner", "-loglevel", "error", "-y"]);
        if runs.len() == 1 {
            cmd.arg("-i").arg(format!("concat:{}", runs[0].join("|")));
        } else {
            let list: String = runs.iter().map(|r| format!("file 'concat:{}'\n", r.join("|"))).collect();
            std::fs::write(sh.buf_dir.join("clip.txt"), list).context("writing concat list")?;
            cmd.args(["-f", "concat", "-safe", "0", "-protocol_whitelist", "file,concat", "-i", "clip.txt"]);
        }
        cmd.args(["-map", "0", "-c", "copy", "-movflags", "+faststart"]);
        // Name the audio tracks so editors show "Game/Desktop" and "Microphone".
        for (i, (title, _)) in sh.tracks.lock().unwrap().iter().enumerate() {
            cmd.arg(format!("-metadata:s:a:{i}")).arg(format!("title={title}"));
            cmd.arg(format!("-metadata:s:a:{i}")).arg(format!("handler_name={title}"));
            cmd.arg(format!("-disposition:a:{i}")).arg(if i == 0 { "default" } else { "0" });
        }
        let result = cmd
            .arg(&out)
            .stdin(Stdio::null())
            .output()
            .context("running ffmpeg")?;
        if !result.status.success() || !out.exists() {
            bail!("ffmpeg failed to save the clip:\n{}", String::from_utf8_lossy(&result.stderr));
        }
        crate::info!("saved {} ({used} segments from {} run(s))", out.display(), runs.len());
        Ok(out)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Segment {
    run: u32,
    idx: u32,
    name: String,
}

/// Segments are named `r<run>_<index>.ts`; returned sorted oldest → newest.
fn list_segments(dir: &Path) -> Vec<Segment> {
    let mut v: Vec<Segment> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let (run, idx) = name.strip_prefix('r')?.strip_suffix(".ts")?.split_once('_')?;
            Some(Segment { run: run.parse().ok()?, idx: idx.parse().ok()?, name })
        })
        .collect();
    v.sort_by_key(|s| (s.run, s.idx));
    v
}

/// "WIDTHxHEIGHT" of a segment's video stream (from ffmpeg's stream info), if readable.
fn video_size(ffmpeg: &str, dir: &Path, segment: &str) -> Option<String> {
    let out = hidden_cmd(ffmpeg).current_dir(dir).args(["-hide_banner", "-i", segment]).output().ok()?;
    let info = String::from_utf8_lossy(&out.stderr);
    let line = info.lines().find(|l| l.contains("Video:"))?;
    line.split([' ', ',']).find_map(|tok| {
        let (w, h) = tok.split_once('x')?;
        (w.parse::<u32>().is_ok() && h.parse::<u32>().is_ok()).then(|| tok.to_string())
    })
}

/// Deletes all but the newest segments (across ffmpeg runs).
fn prune(sh: &Shared) {
    let _guard = sh.seg_lock.lock().unwrap();
    let keep = sh.cfg.clip_seconds.div_ceil(SEG_SECS) as usize + 3;
    let segs = list_segments(&sh.buf_dir);
    for s in &segs[..segs.len().saturating_sub(keep)] {
        let _ = std::fs::remove_file(sh.buf_dir.join(&s.name));
    }
}

/// Newest segment of `run` as (name, size), used to notice when capture stops producing video.
fn newest_segment(sh: &Shared, run: u32) -> Option<(String, u64)> {
    let s = list_segments(&sh.buf_dir).into_iter().rev().find(|s| s.run == run)?;
    let len = std::fs::metadata(sh.buf_dir.join(&s.name)).map(|m| m.len()).unwrap_or(0);
    Some((s.name, len))
}

fn sleep_unless_stopped(sh: &Shared, total: Duration) {
    let step = Duration::from_millis(100);
    let mut t = Duration::ZERO;
    while t < total && !sh.stop.load(Ordering::Relaxed) {
        std::thread::sleep(step);
        t += step;
    }
}

fn supervise(sh: Arc<Shared>) {
    let mut run = 0;
    while !sh.stop.load(Ordering::Relaxed) {
        run += 1;
        sh.run.store(run, Ordering::Relaxed);
        // One job per run: closing it kills ffmpeg, and Windows closes it for us if Aero crashes.
        let job = crate::winutil::KillOnCloseJob::new();
        match spawn(&sh, run, job.as_ref()) {
            Ok((mut child, audio)) => {
                let mut last = None;
                let mut last_change = Instant::now();
                loop {
                    sleep_unless_stopped(&sh, Duration::from_millis(500));
                    if sh.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    prune(&sh);
                    if let Ok(Some(status)) = child.try_wait() {
                        crate::info!("ffmpeg exited ({status}), restarting - see ffmpeg.log");
                        break;
                    }
                    // Watchdog: when a game grabs the screen, screen capture can fail without
                    // ffmpeg exiting (the audio input keeps it alive), so it silently stops
                    // recording. If no new video data appears for a while, restart it.
                    let now = newest_segment(&sh, run);
                    if now != last {
                        last = now;
                        last_change = Instant::now();
                    } else if last_change.elapsed() > STALL_TIMEOUT {
                        crate::info!("capture stalled (no new video for {}s), restarting", STALL_TIMEOUT.as_secs());
                        *sh.last_stall.lock().unwrap() = Some(Instant::now());
                        break;
                    }
                }
                drop(job);
                let _ = child.kill();
                let _ = child.wait();
                if let Some((stop, t)) = audio {
                    stop.store(true, Ordering::Relaxed);
                    let _ = t.join();
                }
            }
            Err(e) => crate::info!("failed to start ffmpeg: {e:#}"),
        }
        sleep_unless_stopped(&sh, Duration::from_secs(1));
    }
}

type AudioPump = (Arc<AtomicBool>, JoinHandle<()>);

fn spawn(sh: &Shared, run: u32, job: Option<&crate::winutil::KillOnCloseJob>) -> Result<(Child, Option<AudioPump>)> {
    let cfg = &sh.cfg;
    let mut wanted = Vec::new();
    if cfg.system_audio {
        wanted.push((Kind::Desktop, "", 1.0));
    }
    if cfg.microphone {
        wanted.push((Kind::Mic { channel: cfg.microphone_channel.max(1) as usize - 1 }, cfg.microphone_device.as_str(), cfg.microphone_volume));
    }
    let sources: Vec<Source> = wanted
        .into_iter()
        .filter_map(|(kind, dev, gain)| match Source::open(kind, dev, gain) {
            Ok(s) => Some(s),
            Err(e) => {
                crate::info!("{} unavailable, skipping that track: {e:#}", kind.title());
                None
            }
        })
        .collect();
    *sh.tracks.lock().unwrap() = sources.iter().map(|s| (s.kind.title(), s.level.clone())).collect();

    let mut cmd = hidden_cmd(&cfg.ffmpeg);
    cmd.current_dir(&sh.buf_dir);
    cmd.args(["-hide_banner", "-loglevel", "warning", "-nostats"]);
    cmd.args(["-f", "lavfi", "-i"]);
    // Both sources hand over GPU (D3D11) frames.
    cmd.arg(match sh.capture {
        "wgc" => format!("gfxcapture=monitor_idx={}:max_framerate={}:capture_cursor=1", cfg.monitor, cfg.fps),
        _ => format!("ddagrab=output_idx={}:framerate={}:draw_mouse=1", cfg.monitor, cfg.fps),
    });
    if !sources.is_empty() {
        let total = audio::total_channels(&sources);
        cmd.args(["-thread_queue_size", "1024", "-f", "f32le", "-ar", &audio::RATE.to_string()]);
        cmd.args(["-ac", &total.to_string(), "-i", "pipe:0"]);
    }
    cmd.args(["-map", "0:v"]);
    if !sources.is_empty() {
        // All sources arrive interleaved in one pipe; split them into one track each.
        let n = sources.len();
        let mut graph = if n > 1 {
            let outs: String = (0..n).map(|i| format!("[s{i}]")).collect();
            format!("[1:a]asplit={n}{outs};")
        } else {
            String::new()
        };
        let mut offset = 0;
        for (i, s) in sources.iter().enumerate() {
            let input = if n > 1 { format!("[s{i}]") } else { "[1:a]".into() };
            let pan = match s.kind {
                Kind::Desktop => format!("pan=stereo|c0=c{}|c1=c{}", offset, offset + 1),
                Kind::Mic { .. } => format!("pan=mono|c0=c{offset}"),
            };
            offset += s.kind.channels();
            graph += &format!("{input}{pan}[a{i}]{}", if i + 1 < n { ";" } else { "" });
        }
        cmd.args(["-filter_complex", &graph]);
        for (i, s) in sources.iter().enumerate() {
            cmd.args(["-map", &format!("[a{i}]")]);
            let rate = if matches!(s.kind, Kind::Mic { .. }) { "96k" } else { "160k" };
            cmd.args([&format!("-c:a:{i}"), "aac", &format!("-b:a:{i}"), rate]);
        }
    }

    let q = cfg.quality.to_string();
    let enc = sh.encoder.as_str();
    if enc.ends_with("_nvenc") {
        // NVENC takes the D3D11 frames from ddagrab directly: zero copies through system RAM.
        cmd.args(["-c:v", enc, "-preset", "p4", "-rc", "vbr", "-cq", &q, "-b:v", "0"]);
    } else if enc.ends_with("_amf") {
        cmd.args(["-vf", "hwdownload,format=bgra,format=nv12"]);
        cmd.args(["-c:v", enc, "-quality", "balanced", "-rc", "cqp", "-qp_i", &q, "-qp_p", &q]);
    } else if enc.ends_with("_qsv") {
        cmd.args(["-vf", "hwdownload,format=bgra,format=nv12"]);
        cmd.args(["-c:v", enc, "-global_quality", &q]);
    } else {
        cmd.args(["-vf", "hwdownload,format=bgra,format=yuv420p"]);
        cmd.args(["-c:v", enc, "-preset", "veryfast", "-crf", &q]);
    }
    // WGC only delivers frames when the screen changes: make the output constant frame rate
    // (editors like that) and force a keyframe every segment so cuts land on time.
    cmd.args(["-fps_mode", "cfr", "-r", &cfg.fps.to_string()]);
    cmd.args(["-g", &(cfg.fps * SEG_SECS).to_string()]);
    cmd.args(["-force_key_frames", &format!("expr:gte(t,n_forced*{SEG_SECS})")]);
    cmd.args(["-f", "segment", "-segment_time", &SEG_SECS.to_string(), "-segment_format", "mpegts"]);
    cmd.arg(format!("r{run}_%06d.ts"));

    let log = std::fs::OpenOptions::new().create(true).append(true).open(&sh.ffmpeg_log)?;
    cmd.stdout(Stdio::null()).stderr(Stdio::from(log));
    cmd.stdin(if sources.is_empty() { Stdio::null() } else { Stdio::piped() });

    let mut child = cmd.spawn().with_context(|| format!("could not run '{}'", cfg.ffmpeg))?;
    if let Some(job) = job {
        job.assign(&child);
    }
    let tracks = sh.tracks.lock().unwrap().iter().map(|t| t.0).collect::<Vec<_>>().join(" + ");
    let pump = match child.stdin.take() {
        Some(stdin) if !sources.is_empty() => {
            let stop = Arc::new(AtomicBool::new(false));
            let t = audio::pump(sources, stdin, stop.clone());
            Some((stop, t))
        }
        _ => None,
    };
    crate::info!("ffmpeg run {run} started (audio: {})", if tracks.is_empty() { "none" } else { &tracks });
    Ok((child, pump))
}
