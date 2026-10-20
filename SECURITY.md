# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through GitHub: open the repository's
**Security** tab and choose **Report a vulnerability**. Please do not open a
public issue.

## Scope

In scope:

- Silk's own code (`src/`), including how it builds FFmpeg command lines from
  file names and paths, and how it handles files dropped on the window.
- The model build scripts in `tools/model/` and the bundled model's
  provenance.
- The release package and how it is built (`tools/release/`), and the CI
  workflows in `.github/`.

Out of scope (please report them upstream):

- Vulnerabilities in FFmpeg, which parses the input videos, ONNX Runtime,
  DirectML, the Visual C++ runtime, GPU drivers, egui or other dependencies.
  Do tell us if Silk uses one of them in an unsafe way, or if a release ships
  a vulnerable version of a bundled DLL.
- Attacks that require replacing files next to `silk.exe` (such as the model
  or the FFmpeg executables) on the user's machine.

## Supported versions

Fixes go into the latest release only (currently 0.1.x).

## Audit status

Silk has not been audited. Its code was written almost entirely by an AI
assistant and has been checked by tests, not by a security review.
