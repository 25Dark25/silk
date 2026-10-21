# Testing

## Automated tests

`cargo test --release` runs 22 tests:

- **Pure logic** (always run): frame-rate parsing, the `minterpolate` filter
  string, RGB plane conversion, duplicate and scene-cut detection, the output
  timeline (doubling, repeated frames, long static runs, cuts), the motion blur
  (static pixels untouched, smear and light conservation, a fast object over a
  still background, a static overlay over a moving one, motion re-anchoring,
  HUD re-anchoring), language-code parsing, saving and reading the FFmpeg
  folder, and the update check (a newer release is found; the same, an older
  or an unexpected tag and error responses are ignored).
- **Model** (`flow_tracks_motion`): runs RIFE on a synthetic pair and checks the
  meaning of the `flow` output. Uses the GPU, or the CPU when there is none.
- **End to end** (`end_to_end`): encodes a generated clip with every method and
  effect and checks size, frame rate, duration and audio. **Skipped when FFmpeg
  is not found**, which is the case in CI.

Three more tests are `#[ignore]`d: `process_file` (runs the whole pipeline on
any file) and `diagnose_pair` (interpolates one pair on the GPU and on the
CPU) are tools for the manual tests below, and `live_github` needs the network
(it fetches a real "latest release" response over TLS and runs Silk's check).

CI has no GPU and no FFmpeg, so the GPU paths and the end-to-end test are only
covered by running them locally.

## Manual tests

### Setup

Run from the repository root in PowerShell, with FFmpeg in `ffmpeg\bin\`.
The inputs are synthetic so anyone can regenerate them; put them outside the
repository:

```powershell
$m = "$env:TEMP\silk-manual"; New-Item -ItemType Directory -Force $m | Out-Null
$ff = ".\ffmpeg\bin\ffmpeg.exe"
# 10 s of 1080p60: a fast camera pan under a translucent HUD bar.
& $ff -v error -y -f lavfi -i "testsrc2=size=5760x1080:rate=60" -f lavfi -i "sine=frequency=220" -filter_complex "[0:v]crop=1920:1080:x='mod(t*900\,3840)':y=0,drawbox=x=0:y=980:w=1920:h=100:color=black@0.75:t=fill[v]" -map "[v]" -map 1:a -t 10 -pix_fmt yuv420p -colorspace bt709 -color_range tv -c:v libx264 -crf 16 -c:a aac -shortest "$m\pan1080.mp4"
# 6 s of 720p30: a pan with a HUD (text, health bar, crosshair).
& $ff -v error -y -f lavfi -i "testsrc2=size=3840x720:rate=30" -f lavfi -i "sine=frequency=220" -filter_complex "[0:v]crop=1280:720:x='mod(t*600\,2560)':y=0,drawbox=x=0:y=640:w=1280:h=80:color=black@0.75:t=fill,drawbox=x=40:y=660:w=300:h=40:color=red:t=fill,drawbox=x=630:y=350:w=20:h=20:color=white:t=3,drawtext=text='HP 100   AMMO 30/90':x=380:y=668:fontsize=28:fontcolor=white,scale=out_color_matrix=bt709[v]" -map "[v]" -map 1:a -t 6 -pix_fmt yuv420p -colorspace bt709 -color_range tv -c:v libx264 -crf 14 -c:a aac -shortest "$m\hud720.mp4"
# The same at 60 fps: every frame twice, like a 30 fps game recorded at 60.
& $ff -v error -y -i "$m\hud720.mp4" -vf fps=60 -colorspace bt709 -color_range tv -c:v libx264 -crf 14 -c:a copy "$m\dup60.mp4"
# A fast object (40 px per frame) over a still grid.
& $ff -v error -y -f lavfi -i "color=c=0x202020:s=640x360:r=12,drawgrid=w=40:h=40:color=0x606060" -f lavfi -i "color=c=orange:s=60x60:r=12" -filter_complex "[0][1]overlay=x='20+t*480':y=150:shortest=1" -t 1.2 -pix_fmt yuv420p -c:v libx264 -crf 10 "$m\fastbox.mp4"
# Frame pairs for the GPU/CPU comparison.
foreach ($size in "1920x1080", "3840x2160") { foreach ($n in 0, 1) {
  & $ff -v error -y -f lavfi -i "testsrc2=size=${size}:rate=60" -vf "select=eq(n\,$(10 + $n)),format=gbrp" -frames:v 1 -f rawvideo "$m\pair_${size}_$n.raw" } }
```

`process_file` reads `SILK_IN`, `SILK_OUT`, `SILK_X` (multiplier, default 2),
`SILK_METHOD` (`rife`, `pm` or `mix`, default `rife`) and `SILK_BLUR` (shutter
angle; unset for no blur):

```powershell
$env:SILK_IN = "$m\pan1080.mp4"; $env:SILK_OUT = "$m\out.mp4"; $env:SILK_X = "2"
cargo test --release process_file -- --ignored --nocapture
```

### Latest results

Run on 2026-10-19 with the code of the initial import, on Windows 11 IoT
Enterprise LTSC 2024 (10.0.26100), Intel Core i7-13620H, NVIDIA GeForce RTX
4060 Laptop GPU (45 W power limit, driver 617.42), 32 GB RAM, rustc 1.99.1 and
FFmpeg 9.0.2 "essentials" from gyan.dev.

| # | Test | Command | Result |
|---|------|---------|--------|
| 1 | GPU vs CPU, and the 8-bit output vs rounding on the CPU | `diagnose_pair` with `SILK_A`, `SILK_B` = the pair, `SILK_W`, `SILK_H`; then compare the two `.raw` files with FFmpeg's `psnr` | 1080p: GPU 0.107 s, CPU 2.56 s, PSNR 99.9 dB, 0 of 6,220,800 values differ. 4K: GPU 0.436 s, CPU 10.1 s, PSNR 99.2 dB, 0 of 24,883,200 differ. **Pass** |
| 2 | Speed, `pan1080.mp4` | `process_file`: `SILK_X=2`; `SILK_X=2 SILK_BLUR=180`; `SILK_X=1 SILK_BLUR=180` | Interpolation 2×: 62.1 s. Both: 102.3 s. Motion blur only: 96.6 s. GPU at 72–73 °C at the end. |
| 3 | Repeated frames, `dup60.mp4` | `process_file` with `SILK_X=2`; count unique frames with `mpdecimate=hi=64*12:lo=64*5:frac=0.33:max=0` | Source: 360 frames, 181 unique. Output: 720 frames, 718 unique; the app reported 180 repeated frames rebuilt. **Pass** |
| 4 | Colour, `hud720.mp4` | `process_file` with `SILK_X=2`; PSNR of the output's even frames (copies) against the source; `ffprobe` colour tags | PSNR average 44.7 dB, minimum 40.3 dB; tags identical (`bt709`, `tv`). **Pass** |
| 5 | Motion blur, visual | `hud720.mp4` with `SILK_X=2 SILK_BLUR=180`; `fastbox.mp4` with `SILK_X=1 SILK_BLUR=360`; inspect frames 41 and 6 | HUD text, bar and crosshair sharp, background edges blurred along the pan. The fast object leaves translucent trails over the grid; a faint line remains at its original edges (known limit). **Pass** |
| 6 | Interface language | Run `silk.exe` with and without `SILK_LANG=en` | Spanish on a Spanish Windows (LANGID 0x0C0A), English with `SILK_LANG=en`; all texts fit. **Pass** |

### Release smoke test

Run on the release package before publishing it:

1. Extract `silk-<version>-windows-x64.zip` into an empty folder and copy
   `ffmpeg.exe` and `ffprobe.exe` into an `ffmpeg\bin\` folder next to
   `silk.exe`.
2. Make a 3 s clip with that FFmpeg:
   `ffmpeg -f lavfi -i "testsrc2=size=3840x720:rate=30" -f lavfi -i "sine=frequency=220" -filter_complex "[0:v]crop=1280:720:x='mod(t*600\,2560)':y=0[v]" -map "[v]" -map 1:a -t 3 -pix_fmt yuv420p -c:v libx264 -crf 16 -c:a aac -shortest clip.mp4`
3. Run `silk.exe clip.mp4` and press **Process** with the defaults
   (Interpolation, 2×, AI). Then again with **Both** (2×, 180°).
4. Check with `ffprobe` that each output has 60 fps, 180 frames, audio and a
   3 s duration, and with `(Get-Process silk).Modules` that the Visual C++
   DLLs and `DirectML.dll` are loaded from the package folder.

Latest result, 2026-10-20, zip built locally by `tools/release/package.ps1`
from this version (same machine as above): both outputs have 60 fps, 180
frames, audio and 3.000 s, and `vcruntime140`, `vcruntime140_1`, `msvcp140`,
`msvcp140_1` and `DirectML.dll` are loaded from the package folder. **Pass**

### Installer test

Run on the MSI before publishing it, as a standard user (no elevation). For the
upgrade step, build a second MSI from the same files with a higher version,
outside `dist\`:
`wix build -acceptEula wix7 -arch x64 -d Version=0.1.1 -d PackageDir=dist\silk-<version>-windows-x64 installer\silk.wxs -o <folder>\silk-upgrade-test-0.1.1.msi`.

1. `msiexec /i silk-<version>-windows-x64.msi /qn`: exit code 0, the files in
   `%LOCALAPPDATA%\Programs\Silk`, a **Silk** shortcut in the Start menu
   pointing to `silk.exe` there, no shortcut on the user's or the public
   desktop.
2. The [release smoke test](#release-smoke-test) on the installed app, with
   FFmpeg found through `%APPDATA%\Silk\ffmpeg-folder.txt` (what **Locate
   ffmpeg.exe…** saves) instead of a copy next to `silk.exe`.
3. Install the 0.1.1 MSI: the Windows Installer API
   (`WindowsInstaller.Installer`, `ProductsEx`) lists one Silk, now 0.1.1.
4. `msiexec /x` it: no product listed, no install folder, no Start menu
   shortcut and no `HKCU\Software\Silk` left.

Latest result, 2026-10-20, MSI built locally by `package.ps1` from this
version: install exit 0 with 16 files, the Start menu shortcut and no desktop
shortcut; the installed app found FFmpeg through the saved folder, and both
smoke outputs have 60 fps, 180 frames, audio and 3.000 s, with the Visual C++
DLLs and `DirectML.dll` loaded from `%LOCALAPPDATA%\Programs\Silk`; the
upgrade left only Silk 0.1.1; the uninstall left nothing of the above.
**Pass**

### Update banner

The check itself is covered by the tests above. To see the banner, point
`LATEST` in `src/update.rs` at a repository with a release above Silk's version
(for example `https://api.github.com/repos/rust-lang/rust/releases/latest`;
`RELEASE_PAGE` must match it), build, run and revert the change. Last checked
on 2026-10-20: the banner shows the version, and **Download** opens the release
page in the browser. **Pass**

### Not tested

- Windows 10, AMD or Intel GPUs, and machines without a GPU (only the CPU path
  of single frames, in test 1).
- Sources above 3840×2160, HDR or 10-bit, variable frame rate, or with
  non-AAC audio; MKV and MOV output.
- Long videos (the longest run with this code was the 10 s of test 2).
- By hand in the interface: Cancel, Open video, Show in folder, dropping a file
  on the window, the file dialogs and the Locate ffmpeg.exe button.
- The update banner against Silk's own repository (it has no release yet).
- SmartScreen on a real download, a machine without the Visual C++ runtime
  installed, an install by an administrator for all users, and screens shorter
  than about 900 px.

## Model

`models/rife_v4.25_flow.onnx` is built from the vs-mlrt RIFE v4.25 export by
the three scripts in `tools/model/`. To rebuild it in a temporary folder and
check it is the same file (needs Python and 7-Zip), from the repository root:

```powershell
$repo = (Get-Location).Path
$w = "$env:TEMP\silk-model"; New-Item -ItemType Directory -Force $w | Out-Null; Set-Location $w
curl.exe -sLO https://github.com/AmusementClub/vs-mlrt/releases/download/external-models/rife_v4.25.7z
(Get-FileHash rife_v4.25.7z).Hash   # 172FE975C1775134BB87108E4EC6D1A89E861CC5D3BE2AC23BF08AFE5ED626B8
& "C:\Program Files\7-Zip\7z.exe" x -y rife_v4.25.7z | Out-Null   # rife_v2\rife_v4.25.onnx
python -m venv venv; .\venv\Scripts\python -m pip install -q onnx==1.23.2
.\venv\Scripts\python "$repo\tools\model\1_add_flow.py" rife_v2\rife_v4.25.onnx step1.onnx
.\venv\Scripts\python "$repo\tools\model\2_split_gridsample.py" step1.onnx step2.onnx
.\venv\Scripts\python "$repo\tools\model\3_compact_io.py" step2.onnx rife_v4.25_flow.onnx
(Get-FileHash rife_v4.25_flow.onnx).Hash   # 152BAD5267AB11B20C330D09C9F9500B744061B5E99A390728CEF934DD8579D4
Set-Location $repo
```

Last checked on 2026-10-19 with Python 3.15: the result is byte-identical to the
bundled model.
