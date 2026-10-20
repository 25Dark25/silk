# Silk

Silky smooth video on Windows. Silk generates in-between frames with an AI
model (RIFE) and adds per-pixel motion blur in the style of ReelSmart Motion
Blur, alone or combined. It was made with gameplay footage in mind.

> **Status:** early, no release yet. The code was written almost entirely by an
> AI assistant (Claude) under the author's direction and has been checked by
> tests and measurements, not by a human code review or a security audit. It
> has been run on one machine only (Windows 11, RTX 4060 Laptop GPU). See
> [Known limits](#known-limits) and [docs/TESTING.md](docs/TESTING.md).

## What it does

Drop a video in the window, pick an effect and press **Process**.

- **Interpolation**: 2×, 3× or 4× the frame rate, with one of three methods:
  - **AI (RIFE)**: RIFE v4.25 running on the GPU through ONNX Runtime and
    DirectML (falls back to the CPU, more than 20× slower).
  - **Pixel Motion**: FFmpeg's `minterpolate` (block-based motion compensation).
  - **Frame Mix**: FFmpeg's `framerate` (blends neighbouring frames).
- **Motion Blur**: blurs every pixel along its own motion during a virtual
  shutter of 45° to 360°. Motion comes from RIFE's optical flow. What moves
  unlike the camera is treated as being in front, so a HUD, a crosshair or a
  followed character stays sharp while the world blurs behind it.
- **Both**: interpolation with RIFE, then motion blur on every output frame.

With the AI method Silk also:

- re-interpolates repeated frames (a game running below the recording frame
  rate) instead of keeping the stutter;
- does not blend across scene cuts;
- keeps the colour matrix, range and tags of the source;
- copies AAC audio untouched (other audio is re-encoded to AAC 192 kb/s).

The interface is in English or Spanish, following the Windows display language.

## Requirements

- Windows 11 x64 (Windows 10 is untested).
- A DirectX 12 GPU for the AI method and motion blur. Without one the CPU is
  used, which is far slower.
- FFmpeg: `ffmpeg.exe` and `ffprobe.exe`, a build that includes `libx264`.
- To build: Rust 1.95 or later with the MSVC toolchain, and internet access on
  the first build (the `ort` crate downloads ONNX Runtime and `DirectML.dll`).

## Quick start

1. Clone the repository:
   ```powershell
   git clone https://github.com/25Dark25/silk
   cd silk
   ```
2. Download a Windows FFmpeg build (for example the "release essentials" build
   from gyan.dev) and copy `ffmpeg.exe` and `ffprobe.exe` to `ffmpeg\bin\`.
3. Build and run:
   ```powershell
   cargo build --release
   .\target\release\silk.exe
   ```

## Configuration

- **Language**: Spanish if the Windows display language is any Spanish
  variant, English otherwise. Set `SILK_LANG=en` or `SILK_LANG=es` to force one.
- **FFmpeg**: searched next to `silk.exe`, in `ffmpeg\` or `ffmpeg\bin\` beside
  it or in up to three parent folders, then on `PATH`. If none is found, the
  window offers a button to locate `ffmpeg.exe`.
- **Model**: `models\rife_v4.25_flow.onnx`, searched the same way.
- **Output**: next to the input, named after it (for example
  `clip_smooth_120fps_blur.mp4`), never overwriting an existing file.
  **Change…** picks another place and an MP4, MKV or MOV container.
- **Blur strength**: the shutter angle. 180° is the natural look of a film
  camera; 360° blurs over the whole frame interval.

## Known limits

- **Windows only.** DirectML exists only on Windows, and the `ort` crate ships
  no DirectML build for other systems.
- **Speed.** On an RTX 4060 Laptop GPU (45 W), 10 s of 1080p60 video take about
  62 s with 2× interpolation, 102 s with both effects and 97 s with motion
  blur alone; one 4K AI frame takes about 0.44 s. Details and conditions are in
  [docs/TESTING.md](docs/TESTING.md).
- **DirectML bug.** DirectML miscomputes `GridSample` on tensors above roughly
  2^26 elements, which broke every interpolated frame at 4K. The bundled model
  splits those operations to avoid it. GPU output has been checked against the
  CPU up to 3840×2160, not above.
- **Pixel Motion** is slow above 1080p (FFmpeg's `minterpolate` uses one core).
- **Colour depth.** Everything is processed as 8-bit 4:2:0. HDR or 10-bit
  sources keep their tags but lose precision.
- **Odd dimensions** are cropped by one pixel (H.264 4:2:0 needs even sizes).
- **Tracks.** Only the first video and the first audio track are kept; the
  output is always H.264. Variable frame rate sources are converted to a
  constant rate.
- **Motion blur** is reconstructed from one frame and its motion, without real
  depth: the "in front" guess can be wrong, the edges of very fast objects can
  show a faint line, motion is assumed straight within a frame, and the trail
  a foreground object spreads over its surroundings is limited to 48 px each
  way.
- **Heuristics.** Scene cuts and repeated frames are detected with thresholds;
  at most three consecutive repeats are treated as dropped frames.
- **Continuous integration has no GPU.** The GPU paths are tested by hand
  ([docs/TESTING.md](docs/TESTING.md)).

## Verifying a release

There is no release yet. Releases will contain the files, a `SHA256SUMS` file
and its detached signature `SHA256SUMS.asc`, made with this key:

```
A7FC EDEA D7E6 C343 7312  284A 442D A687 E0FF 73EE
```

1. Import the key and compare its fingerprint with the one above:
   ```powershell
   curl.exe -sL https://github.com/25Dark25.gpg | gpg --import
   gpg --fingerprint A7FCEDEAD7E6C3437312284A442DA687E0FF73EE
   ```
   This README and `https://github.com/25Dark25.gpg` are both served by
   GitHub. For an independent check, confirm the fingerprint with the author
   through another channel.
2. Verify the signature, then the hashes:
   ```powershell
   gpg --verify SHA256SUMS.asc SHA256SUMS
   Get-Content SHA256SUMS
   Get-FileHash -Algorithm SHA256 <file>
   ```
   Each file's hash must match its line in `SHA256SUMS` (or run
   `sha256sum -c SHA256SUMS` where it is available).

## Development

```powershell
cargo build --release
cargo test --release
cargo clippy --all-targets --release -- -D warnings
```

- Tests that need FFmpeg look for it like the app does and are skipped when it
  is missing. Tests that need the model run on the GPU, or on the CPU when
  there is none.
- Manual tests, the commands to run them and their latest results are in
  [docs/TESTING.md](docs/TESTING.md).
- `tools/model/` rebuilds `models/rife_v4.25_flow.onnx` from the original
  export; the steps are in [docs/TESTING.md](docs/TESTING.md#model).

## Third-party components

- **RIFE** v4.25 weights by hzwer
  ([Practical-RIFE](https://github.com/hzwer/Practical-RIFE), MIT).
- **The ONNX export** of those weights comes from the
  [vs-mlrt](https://github.com/AmusementClub/vs-mlrt) `external-models`
  release. The vs-mlrt repository is GPL-3.0; the model files carry no separate
  license statement. Silk's bundled model is that export with the changes made
  by `tools/model/`.
- **ONNX Runtime** (MIT), through the [`ort`](https://crates.io/crates/ort)
  crate (MIT or Apache-2.0), and **DirectML** (Microsoft, under its own
  license), both downloaded at build time and not stored in this repository.
- **FFmpeg** is not included; its license depends on the build you download
  (builds with `libx264` are GPL).
- **egui/eframe** and **rfd** (MIT or Apache-2.0). `cargo deny check licenses`
  lists every dependency's license.

## Security

See [SECURITY.md](SECURITY.md). Please report vulnerabilities privately, not
in public issues.

## License

The code is under the [MIT License](LICENSE).
