# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-10-20

### Added

- Windows desktop app (egui) that makes videos smoother: drop a video, pick an
  effect, press Process.
- Frame interpolation (2×, 3×, 4×) with three methods: RIFE v4.25 on the GPU
  through ONNX Runtime and DirectML with a CPU fallback, FFmpeg `minterpolate`
  (Pixel Motion) and FFmpeg `framerate` (Frame Mix).
- Per-pixel motion blur (shutter 45°–360°) driven by RIFE's optical flow, on
  its own or combined with interpolation.
- Repeated-frame repair, scene-cut detection, preserved colour tags and
  untouched AAC audio for the AI pipeline.
- English and Spanish interface following the Windows display language
  (`SILK_LANG` overrides it).
- Scripts in `tools/model/` that rebuild the bundled model from the vs-mlrt
  RIFE v4.25 export.
- Release package `silk-<version>-windows-x64.zip` with the app, the model,
  DirectML, the Visual C++ runtime and the third-party licenses, built by the
  "Release build" workflow.

[Unreleased]: https://github.com/25Dark25/silk/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/25Dark25/silk/releases/tag/v0.1.0
