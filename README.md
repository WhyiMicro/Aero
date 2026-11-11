# Aero

A tiny instant-replay clipper for Windows. It sits in the system tray and keeps recording your
screen, desktop audio and microphone into a rolling buffer. Press **Alt+F10** to save the last
30 seconds as an MP4.

Clips have two audio tracks:

| Track | Content                  | Format        |
|-------|--------------------------|---------------|
| 1     | Game/desktop (what you hear) | stereo AAC 160k |
| 2     | Microphone               | mono AAC 96k  |

Video editors (DaVinci Resolve, Premiere, Shotcut…) show them as separate tracks, so you can
adjust or mute your voice on its own. Most players and Discord only play track 1.

- ~5.5 MB executable, ~80 MB RAM (mostly the window's OpenGL context), ~0–3% CPU
  (encoding runs on the GPU via NVENC / AMF / QuickSync)
- The window costs nothing while closed. Closing it hides it to the tray, and nothing is drawn.
- Saving is instant because clips are remuxed, not re-encoded
- Clips go to `Videos\Aero\Aero_<date>_<time>.mp4`

## Requirements

- Windows 10/11
- [ffmpeg](https://ffmpeg.org) on your PATH (`scoop install ffmpeg` or `winget install ffmpeg`)

## Install

```bash
powershell -ExecutionPolicy Bypass -File install.ps1
```

This builds Aero, installs it to `%LOCALAPPDATA%\Programs\Aero\aero.exe`, and adds an **Aero**
shortcut to the Start Menu, so you can search for "Aero". Run it again after changing the
code to update your installed copy; it closes and restarts Aero for you.

To only build: `cargo build --release`, which produces `target\release\aero.exe`.

## Usage

Run `aero.exe` and a small window opens:

- **Top:** recording status, live level meters for game audio and mic, a big **Save clip**
  button, and **Pause**.
- **Clips tab:** your clips, newest first. **Play**, **Edit**, **Show** (in Explorer), or
  **Delete** (moves the clip to the Recycle Bin).
- **Edit tab:** trim a clip from the left and right. Drag the red handles on the thumbnail
  strip and watch the preview frame. Click the strip to look at any moment, then use **Set
  start here** / **Set end here**. Keys: ←/→ move 0.1 s (Shift: 1 s, Ctrl: 1 frame), **I**/**O**
  set the start/end. **Play selection** plays just the trimmed part (if mpv is installed).
  **Save trimmed clip** writes `…_trim.mp4` next to the original, or replaces the original
  (which goes to the Recycle Bin).
  - Saving is **instant and lossless**: the video isn't re-encoded, and players start exactly
    at your handle. A few apps ignore that and show up to 2 s extra at the start; tick
    **Re-encode** for those (it uses the GPU and takes about half the clip's length).
- **Settings tab:** clip length, hotkey (click it, then press your combo), save folder, frame rate,
  quality, monitor, encoder, game audio and mic on/off, mic device, input and volume, and
  **Start with Windows**. Click **Save** to apply.

Closing the window hides Aero to the tray; it keeps recording. **Left-click** the tray dot (or
run `aero.exe` again) to bring the window back. **Right-click** it for Save clip, Pause, Open
clips folder and **Quit**.

You hear a "ding" when a clip is saved, or an error sound if saving failed.

## Settings file (`%APPDATA%\Aero\config.toml`)

The Settings tab writes this file. You can also edit it by hand while Aero is closed.

```toml
clip_seconds = 30        # length of each clip
hotkey = "Alt+F10"       # e.g. "Ctrl+Shift+S", "F9"
fps = 60
monitor = 0              # 0 = first monitor, 1 = second, ...
encoder = "auto"         # auto | h264_nvenc | hevc_nvenc | h264_amf | h264_qsv | libx264
quality = 23             # lower = better quality and bigger files (18-28)
system_audio = true      # record what you hear (track 1)
microphone = true        # record your mic (track 2)
microphone_device = ""   # part of the mic's name, e.g. "Scarlett"; empty = Windows default
microphone_channel = 1   # input on the device (audio interfaces: 1 = mic, 2 = instrument)
microphone_volume = 1.0  # 2.0 = twice as loud
output_dir = ""          # empty = Videos\Aero
ffmpeg = "ffmpeg"        # or a full path to ffmpeg.exe
```

## How it works

```
gfxcapture (WGC) or ddagrab ────────────────┐
                                            ├─> ffmpeg ─> 2-second .ts segments in %TEMP%\aero-buffer
WASAPI loopback ─┐                          │             (older ones are deleted continuously)
                 ├─ 48 kHz, one pipe ───────┘
microphone ──────┘   [game L, game R, mic] → split into 2 tracks

hotkey ─> concat newest segments ─> ffmpeg -c copy ─> Videos\Aero\Aero_….mp4
```

- **Video:** by default the screen is captured with Windows Graphics Capture (ffmpeg's
  `gfxcapture`, ffmpeg 8+). That's the API OBS uses, and it copes with games switching into
  fullscreen. With older ffmpeg, Aero falls back to Desktop Duplication (`ddagrab`). Either way
  the D3D11 frames go straight to NVENC, so they never pass through system RAM. Output is a
  constant frame rate with a keyframe every 2 s.
- **Audio:** each source is converted to 48 kHz as it's captured. Windows sends no loopback
  data while nothing is playing, so Aero writes audio on a wall-clock schedule and fills gaps
  with silence. Both tracks share one pipe and one clock, so they stay in sync with the video
  and with each other. A 40 ms jitter buffer prevents crackling.
- **Robustness:** if ffmpeg exits, or stops producing video for 8 s (screen capture can fail
  silently when a game grabs the display), Aero restarts it within a second. Clips stitch
  together footage from before and after a restart. ffmpeg runs in a Windows job object, so it
  can never outlive Aero.

Logs: `%APPDATA%\Aero\aero.log` and `ffmpeg.log`.

## Known limitations

- Clips are 0–2 s longer than `clip_seconds`, because cuts happen on 2-second segment boundaries.
- A capture restart leaves a short jump in the clip, and footage from before the restart is only
  kept if the resolution didn't change.
- Some exclusive-fullscreen games (especially OpenGL ones such as Minecraft Java) may still show
  up frozen or black. Try, in order:
  1. Switch Settings → Capture to the other method.
  2. NVIDIA Control Panel → Manage 3D settings → **Vulkan/OpenGL present method** →
     **Prefer layered on DXGI Swapchain**.
  3. Run the game in borderless windowed mode.
- Game audio always comes from the Windows default output device.

## Troubleshooting the mic

If the Microphone meter stays empty while you talk, your voice may be on a different input of
your device (audio interfaces like the Focusrite Scarlett have several). To see where your voice
arrives, run this and talk for 15 seconds:

```bash
cargo run --example mictest
```

It prints the level of every input on every mic. Pick the device and input that peak highest in
Settings → Audio.

## License

[MIT](LICENSE)
