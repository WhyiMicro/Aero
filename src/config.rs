use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Config {
    pub clip_seconds: u32,
    pub hotkey: String,
    pub fps: u32,
    pub monitor: u32,
    pub encoder: String,
    pub quality: u32,
    pub system_audio: bool,
    pub microphone: bool,
    pub microphone_device: String,
    pub microphone_channel: u32,
    pub microphone_volume: f32,
    pub capture: String,
    pub output_dir: String,
    pub ffmpeg: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            clip_seconds: 30,
            hotkey: "Alt+F10".into(),
            fps: 60,
            monitor: 0,
            encoder: "auto".into(),
            quality: 23,
            system_audio: true,
            microphone: true,
            microphone_device: String::new(),
            microphone_channel: 1,
            microphone_volume: 1.0,
            capture: "auto".into(),
            output_dir: String::new(),
            ffmpeg: "ffmpeg".into(),
        }
    }
}

const TEMPLATE: &str = r#"# Aero settings.
# After editing, right-click the tray icon -> "Reload settings".

# Length of a clip: the last N seconds before you press the hotkey.
clip_seconds = 30

# Hotkey that saves a clip, e.g. "Alt+F10", "Ctrl+Shift+S", "F9".
hotkey = "Alt+F10"

# Capture frame rate.
fps = 60

# Which monitor to record (0 = first monitor, 1 = second, ...).
monitor = 0

# Screen capture: "auto" (Windows Graphics Capture if available), "wgc", or "dda" (Desktop Duplication).
capture = "auto"

# Video encoder: "auto", "h264_nvenc", "hevc_nvenc", "h264_amf", "h264_qsv", "libx264".
encoder = "auto"

# Quality: lower = better looking and bigger files. 18-28 is a sensible range.
quality = 23

# Record desktop/game audio (what you hear). Saved as audio track 1.
system_audio = true

# Record your microphone. Saved as a separate audio track (track 2).
microphone = true

# Which mic to use: part of its name as shown in Windows Sound settings
# (e.g. "Yeti" or "Headset"). Empty = the Windows default microphone.
microphone_device = ""

# Which input of the mic device to record. 1 is right for almost every mic.
# Audio interfaces have several inputs (e.g. Scarlett Solo: 1 = mic, 2 = instrument).
microphone_channel = 1

# Mic volume multiplier: 1.0 = unchanged, 2.0 = twice as loud.
microphone_volume = 1.0

# Folder clips are saved to. Empty = your Videos\Aero folder.
output_dir = ""

# Path to ffmpeg.exe, or just "ffmpeg" if it's on your PATH.
ffmpeg = "ffmpeg"
"#;

impl Config {
    pub fn load_or_create(path: &Path) -> Result<Config> {
        if !path.exists() {
            std::fs::write(path, TEMPLATE).context("writing default config")?;
        }
        let text = std::fs::read_to_string(path).context("reading config")?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("invalid settings in {}", path.display()))?;
        cfg.clip_seconds = cfg.clip_seconds.clamp(5, 600);
        cfg.fps = cfg.fps.clamp(10, 240);
        cfg.microphone_volume = cfg.microphone_volume.clamp(0.0, 10.0);
        Ok(cfg)
    }

    pub fn clips_dir(&self) -> PathBuf {
        if !self.output_dir.trim().is_empty() {
            return PathBuf::from(self.output_dir.trim());
        }
        let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| ".".into());
        home.join("Videos").join("Aero")
    }
}

impl Config {
    /// Writes the settings (as edited in the Aero window) back to disk.
    pub fn save(&self, path: &Path) -> Result<()> {
        let body = toml::to_string(self).context("serialising settings")?;
        let text = format!("# Aero settings. Easiest to change from the Aero window (click the tray icon).\n\n{body}");
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }
}
