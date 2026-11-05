//! Audio capture via cpal, streamed to ffmpeg as one raw f32 PCM pipe.
//!
//! Sources (desktop loopback, microphone) are converted to 48 kHz on capture: game audio to
//! stereo, mic to mono. They are interleaved into one stream ([game L, game R, mic]) and
//! ffmpeg splits that into separate tracks. One pipe on one clock means the tracks stay in sync.
//!
//! Devices deliver nothing while silent (WASAPI loopback with nothing playing), so we can't
//! just forward whatever arrives. A pump thread writes exactly `elapsed * 48000` frames and
//! fills gaps with silence. A small per-source jitter buffer avoids clicks from bursty delivery.

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::io::Write;
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const RATE: u32 = 48_000;
const JITTER: usize = RATE as usize * 40 / 1000; // 40ms
const MAX_BACKLOG: usize = JITTER + RATE as usize / 5; // drop if clock drift builds >200ms

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// What you hear (WASAPI loopback of the default output device). Stereo.
    Desktop,
    /// A microphone, recorded mono from one input channel (0-based). Interfaces like a
    /// Focusrite Scarlett expose extra channels (instrument, loopback) that must not be mixed in.
    Mic { channel: usize },
}

impl Kind {
    pub fn channels(self) -> usize {
        match self {
            Kind::Desktop => 2,
            Kind::Mic { .. } => 1,
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Kind::Desktop => "Game/Desktop",
            Kind::Mic { .. } => "Microphone",
        }
    }
}

pub struct Source {
    pub kind: Kind,
    _stream: cpal::Stream,
    /// 48 kHz frames with `kind.channels()` channels, interleaved.
    queue: Arc<Mutex<VecDeque<f32>>>,
    primed: bool,
    /// Peak sample level (f32 bits) since the UI last read it.
    pub level: Arc<AtomicU32>,
}

impl Source {
    /// `device` = substring of the device name, or empty for the Windows default.
    pub fn open(kind: Kind, device: &str, gain: f32) -> Result<Self> {
        let host = cpal::default_host();
        let dev = if device.trim().is_empty() {
            match kind {
                Kind::Desktop => host.default_output_device(),
                Kind::Mic { .. } => host.default_input_device(),
            }
            .ok_or_else(|| anyhow!("no default {} device", if kind != Kind::Desktop { "microphone" } else { "output" }))?
        } else {
            find_device(&host, kind, device)?
        };
        let name = dev.description().map(|d| d.name().to_string()).unwrap_or_default();
        let supported = match kind {
            Kind::Desktop => dev.default_output_config(),
            Kind::Mic { .. } => dev.default_input_config(),
        }
        .context("querying device format")?;
        let config = supported.config();
        let (in_rate, in_ch) = (config.sample_rate, config.channels as usize);
        // Which input channel feeds a mono track (None = stereo track, uses channels 1+2).
        let pick = match kind {
            Kind::Mic { channel } if channel < in_ch => Some(channel),
            Kind::Mic { channel } => {
                crate::info!("mic channel {} doesn't exist (device has {in_ch}), using 1", channel + 1);
                Some(0)
            }
            Kind::Desktop => None,
        };
        crate::info!("{} source: \"{name}\" {in_rate} Hz, {in_ch} ch, using {}", kind.title(),
            pick.map_or("ch 1+2".to_string(), |c| format!("ch {}", c + 1)));

        let out_ch = kind.channels();
        let cap = RATE as usize * out_ch * 2; // keep at most ~2s queued
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(cap)));
        let level = Arc::new(AtomicU32::new(0));

        macro_rules! build {
            ($t:ty, $conv:expr) => {{
                let q = queue.clone();
                let lvl = level.clone();
                let mut conv = Converter::new(in_rate, in_ch, out_ch, pick, gain);
                dev.build_input_stream::<$t, _, _>(
                    config,
                    move |data: &[$t], _| {
                        let out = conv.process(data.iter().map($conv));
                        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
                        let _ = lvl.try_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                            (peak > f32::from_bits(old)).then_some(peak.to_bits())
                        });
                        if let Ok(mut q) = q.lock() {
                            q.extend(out.iter().copied());
                            let excess = q.len().saturating_sub(cap);
                            q.drain(..excess - excess % out_ch);
                        }
                    },
                    move |e| crate::info!("{} stream error: {e}", kind.title()),
                    None,
                )
            }};
        }
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => build!(f32, |s: &f32| *s),
            cpal::SampleFormat::I16 => build!(i16, |s: &i16| *s as f32 / 32768.0),
            cpal::SampleFormat::I32 => build!(i32, |s: &i32| *s as f32 / 2147483648.0),
            f => return Err(anyhow!("unsupported sample format {f:?}")),
        }
        .context("opening audio stream")?;
        stream.play().context("starting audio stream")?;
        Ok(Self { kind, _stream: stream, queue, primed: false, level })
    }

    /// Appends exactly `need` frames to `out`: captured audio when available, silence otherwise.
    fn fill(&mut self, need: usize, out: &mut Vec<f32>) {
        let ch = self.kind.channels();
        let start = out.len();
        if let Ok(mut q) = self.queue.lock() {
            let avail = q.len() / ch;
            if !self.primed && avail >= JITTER {
                self.primed = true;
            }
            if self.primed {
                if avail > MAX_BACKLOG {
                    q.drain(..(avail - JITTER) * ch);
                }
                let take = need.min(q.len() / ch);
                out.extend(q.drain(..take * ch));
                if take < need {
                    self.primed = false; // ran dry: rebuild the jitter buffer
                }
            }
        }
        out.resize(start + need * ch, 0.0);
    }
}

/// Microphones (input devices) as (name, channel count), for the settings UI.
pub fn list_mics() -> Vec<(String, u16)> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else { return Vec::new() };
    devices
        .filter_map(|d| {
            let name = d.description().ok()?.name().to_string();
            let ch = d.default_input_config().map(|c| c.channels()).unwrap_or(1);
            Some((name, ch))
        })
        .collect()
}

fn find_device(host: &cpal::Host, kind: Kind, needle: &str) -> Result<cpal::Device> {
    let devices = match kind {
        Kind::Desktop => host.output_devices().map(|d| d.collect::<Vec<_>>()),
        Kind::Mic { .. } => host.input_devices().map(|d| d.collect::<Vec<_>>()),
    }
    .context("listing audio devices")?;
    let needle_lc = needle.to_lowercase();
    let mut names = Vec::new();
    for d in devices {
        let name = d.description().map(|d| d.name().to_string()).unwrap_or_default();
        if name.to_lowercase().contains(&needle_lc) {
            return Ok(d);
        }
        names.push(name);
    }
    Err(anyhow!("no audio device matching \"{needle}\". Available: {}", names.join(", ")))
}

/// Channel remix + linear resampling to 48 kHz, done incrementally per callback.
struct Converter {
    in_ch: usize,
    out_ch: usize,
    pick: Option<usize>,
    gain: f32,
    step: f64, // input frames per output frame
    pos: f64,  // read position; index 0 = last frame of the previous callback
    prev: Vec<f32>,
    frames: Vec<f32>,
    out: Vec<f32>,
}

impl Converter {
    fn new(in_rate: u32, in_ch: usize, out_ch: usize, pick: Option<usize>, gain: f32) -> Self {
        Self {
            in_ch: in_ch.max(1),
            out_ch,
            pick,
            gain,
            step: in_rate as f64 / RATE as f64,
            pos: 0.0,
            prev: vec![0.0; out_ch],
            frames: Vec::new(),
            out: Vec::new(),
        }
    }

    fn process(&mut self, samples: impl Iterator<Item = f32>) -> &[f32] {
        // 1. remix to out_ch, prefixed by the previous callback's last frame
        self.frames.clear();
        self.frames.extend_from_slice(&self.prev);
        let mut frame = Vec::with_capacity(self.in_ch);
        for s in samples {
            frame.push(s);
            if frame.len() == self.in_ch {
                match (self.pick, self.in_ch) {
                    (Some(c), _) => self.frames.push(frame[c] * self.gain),
                    (None, 1) => self.frames.extend([frame[0] * self.gain; 2]),
                    (None, _) => self.frames.extend([frame[0] * self.gain, frame[1] * self.gain]),
                }
                frame.clear();
            }
        }
        let n = self.frames.len() / self.out_ch; // includes the carried-over frame
        self.out.clear();
        if n < 2 {
            return &self.out;
        }
        // 2. resample (passthrough when rates match: step == 1, pos stays integral)
        let ch = self.out_ch;
        while self.pos < (n - 1) as f64 {
            let i = self.pos as usize;
            let t = (self.pos - i as f64) as f32;
            for c in 0..ch {
                let a = self.frames[i * ch + c];
                let b = self.frames[(i + 1) * ch + c];
                self.out.push(a + (b - a) * t);
            }
            self.pos += self.step;
        }
        self.pos -= (n - 1) as f64;
        self.prev.copy_from_slice(&self.frames[(n - 1) * ch..n * ch]);
        &self.out
    }
}

/// Total channels in the interleaved pipe for these sources.
pub fn total_channels(sources: &[Source]) -> usize {
    sources.iter().map(|s| s.kind.channels()).sum()
}

/// Writes interleaved 48 kHz PCM for all sources into ffmpeg's stdin until `stop` is set
/// or the pipe breaks.
pub fn pump(mut sources: Vec<Source>, mut stdin: ChildStdin, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let total_ch = total_channels(&sources);
        let start = Instant::now();
        let mut written: u64 = 0;
        let mut per_source: Vec<Vec<f32>> = vec![Vec::new(); sources.len()];
        let mut bytes: Vec<u8> = Vec::new();

        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
            let target = (start.elapsed().as_secs_f64() * RATE as f64) as u64;
            let need = (target - written) as usize;
            if need == 0 {
                continue;
            }
            for (src, buf) in sources.iter_mut().zip(per_source.iter_mut()) {
                buf.clear();
                src.fill(need, buf);
            }
            bytes.clear();
            bytes.reserve(need * total_ch * 4);
            for f in 0..need {
                for (src, buf) in sources.iter().zip(per_source.iter()) {
                    let ch = src.kind.channels();
                    for s in &buf[f * ch..(f + 1) * ch] {
                        bytes.extend_from_slice(&s.to_le_bytes());
                    }
                }
            }
            if stdin.write_all(&bytes).is_err() {
                break; // ffmpeg exited
            }
            written += need as u64;
        }
    })
}

/// Name of the Windows default microphone.
pub fn default_mic() -> Option<String> {
    let dev = cpal::default_host().default_input_device()?;
    Some(dev.description().ok()?.name().to_string())
}
