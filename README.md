# Silk

Silky smooth video on Windows. Silk generates in-between frames with an AI
model (RIFE) and adds per-pixel motion blur in the style of ReelSmart Motion
Blur, alone or combined. It was made with gameplay footage in mind.

> **Status:** early; 0.1.0 is the first release. The code was written almost entirely by an
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
- To build from source: Rust 1.95 or later with the MSVC toolchain, and
  internet access on the first build (the `ort` crate downloads ONNX Runtime
  and `DirectML.dll`).

The release packages include everything else, the Visual C++ runtime too.

## Quick start

Each [release](https://github.com/25Dark25/silk/releases) has an installer and
a portable zip. [Verify](#verifying-a-release) the one you download.

- **Installer** (`silk-<version>-windows-x64.msi`): installs Silk for your
  user only, in `%LOCALAPPDATA%\Programs\Silk`, without administrator
  rights, with a Start menu shortcut and no desktop shortcut. A newer
  installer replaces the installed version; uninstall it from Settings > Apps.
- **Portable** (`silk-<version>-windows-x64.zip`): extract it anywhere and run
  `silk.exe`.

Silk also needs FFmpeg. Download a Windows build (for example the "release
essentials" build from gyan.dev). With the portable zip you can copy
`ffmpeg.exe` and `ffprobe.exe` into an `ffmpeg\bin\` folder next to
`silk.exe`; otherwise press **Locate ffmpeg.exe…** in Silk once, and it
remembers the folder.

Neither package is code-signed, so Windows SmartScreen may warn about an
unrecognised app when you open the download; if it does, choose **More info**,
then **Run anyway**.

To build from source instead, clone the repository, put FFmpeg in
`ffmpeg\bin\` at its root, and run:

```powershell
cargo build --release
.\target\release\silk.exe
```

## Configuration

- **Language**: Spanish if the Windows display language is any Spanish
  variant, English otherwise. Set `SILK_LANG=en` or `SILK_LANG=es` to force one.
- **FFmpeg**: searched next to `silk.exe`, in `ffmpeg\` or `ffmpeg\bin\` beside
  it or in up to three parent folders, then in the folder picked with
  **Locate ffmpeg.exe…** (saved in `%APPDATA%\Silk\ffmpeg-folder.txt`), then on
  `PATH`.
- **Updates**: when it starts, Silk asks GitHub
  (`api.github.com/repos/25Dark25/silk/releases/latest`) for the latest
  release, and if it is newer, shows a banner with a link to its page. Nothing
  is downloaded or installed automatically. The request reveals your IP
  address and Silk's version to GitHub; when it fails (offline, for example)
  nothing is shown.
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
- **Bundled Visual C++ runtime.** The copy next to `silk.exe` is not updated by
  Windows Update; it is updated with each Silk release.
- **Not code-signed.** SmartScreen may warn about the downloaded installer or
  zip.
- **Window height.** The window needs about 820 px of height, so it does not
  fit whole on screens with fewer than about 900 px.

## Verifying a release

Each release contains `silk-<version>-windows-x64.msi`,
`silk-<version>-windows-x64.zip`, a `SHA256SUMS` file and its detached
signature `SHA256SUMS.asc`, made with this key:

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
- `tools/release/package.ps1` builds the release zip and MSI from a release
  build. It needs `cargo install cargo-about --locked --features cli`, 7-Zip,
  WiX v7 (`dotnet tool install --global wix --version 7.0.0`) and the Visual
  Studio C++ build tools. The "Release build" workflow runs it on a tag.

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
- **The Visual C++ runtime** (Microsoft), copied into the release package as
  "Distributable Code" under the Visual Studio license terms.
- **FFmpeg** is not included; its license depends on the build you download
  (builds with `libx264` are GPL).
- **egui/eframe** and **rfd** (MIT or Apache-2.0). `cargo deny check licenses`
  lists every dependency's license.

The release packages carry `THIRD-PARTY.txt` and a `licenses` folder with the
license texts and notices of everything they contain. The installer is built
with [WiX Toolset](https://github.com/wixtoolset/wix) v7.

## Security

See [SECURITY.md](SECURITY.md). Please report vulnerabilities privately, not
in public issues.

## License

The code is under the [MIT License](LICENSE).
