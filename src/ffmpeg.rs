//! Everything that talks to FFmpeg: locating the binaries, probing the input,
//! building the interpolation filter and running the encode on a worker thread.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crate::i18n::tr;

#[derive(Clone, Debug)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Tools {
    /// Looks next to the executable (and a few parent folders, which covers
    /// `target/release` during development), then in the folder the user
    /// picked last time, then on `PATH`.
    pub fn locate() -> Option<Self> {
        if let Ok(exe) = std::env::current_exe() {
            for dir in exe.ancestors().skip(1).take(4) {
                for candidate in [dir.to_path_buf(), dir.join("ffmpeg"), dir.join("ffmpeg").join("bin")] {
                    if let Some(tools) = Self::in_dir(&candidate) {
                        return Some(tools);
                    }
                }
            }
        }
        if let Some(tools) = saved_file().and_then(|f| read_saved(&f)).and_then(|dir| Self::in_dir(&dir)) {
            return Some(tools);
        }
        let on_path = Self { ffmpeg: "ffmpeg".into(), ffprobe: "ffprobe".into() };
        (runs(&on_path.ffmpeg) && runs(&on_path.ffprobe)).then_some(on_path)
    }

    pub fn in_dir(dir: &Path) -> Option<Self> {
        let tools = Self {
            ffmpeg: dir.join(exe_name("ffmpeg")),
            ffprobe: dir.join(exe_name("ffprobe")),
        };
        (tools.ffmpeg.is_file() && tools.ffprobe.is_file()).then_some(tools)
    }

    /// Remembers the folder of these tools for the next start (see `locate`).
    pub fn remember(&self) -> std::io::Result<()> {
        match (saved_file(), self.ffmpeg.parent()) {
            (Some(file), Some(dir)) => save(&file, dir),
            _ => Ok(()),
        }
    }
}

/// `%APPDATA%\Silk\ffmpeg-folder.txt`: the folder picked with "Locate ffmpeg.exe…".
fn saved_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("Silk").join("ffmpeg-folder.txt"))
}

fn read_saved(file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let dir = text.trim();
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

fn save(file: &Path, dir: &Path) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file, dir.to_string_lossy().as_bytes())
}

fn exe_name(base: &str) -> String {
    if cfg!(windows) { format!("{base}.exe") } else { base.to_owned() }
}

fn runs(program: &Path) -> bool {
    command(program)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A `Command` that never flashes a console window on Windows.
pub(crate) fn command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

// ---------------------------------------------------------------------------
// Probing

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rational {
    pub num: u64,
    pub den: u64,
}

impl Rational {
    fn parse(s: &str) -> Option<Self> {
        let (n, d) = s.trim().split_once('/').unwrap_or((s.trim(), "1"));
        let (num, den) = (n.parse().ok()?, d.parse().ok()?);
        (num > 0 && den > 0).then_some(Self { num, den })
    }

    pub fn value(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    pub fn times(self, k: u64) -> Self {
        Self { num: self.num * k, den: self.den }
    }
}

#[derive(Clone, Debug)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub fps: Rational,
    /// Seconds; `None` when the container doesn't report it.
    pub duration: Option<f64>,
    /// Codec of the first audio stream, e.g. "aac"; `None` without audio.
    pub audio_codec: Option<String>,
    /// ffprobe's names, e.g. "bt709"/"tv"; `None` when untagged.
    pub color_space: Option<String>,
    pub color_range: Option<String>,
    pub color_primaries: Option<String>,
    pub color_transfer: Option<String>,
}

pub fn probe(tools: &Tools, input: &Path) -> Result<VideoInfo, String> {
    let out = command(&tools.ffprobe)
        .args(["-v", "error", "-show_entries"])
        .arg(
            "stream=codec_type,codec_name,width,height,r_frame_rate,avg_frame_rate,\
             color_space,color_range,color_primaries,color_transfer:format=duration",
        )
        .args(["-of", "compact=p=0"])
        .arg(input)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| trf!("Could not run ffprobe: {e}", "No se pudo ejecutar ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(last_lines(&String::from_utf8_lossy(&out.stderr), 3));
    }

    let mut video = None;
    let mut audio_codec = None;
    let mut duration = None;
    let mut colors = [None, None, None, None];
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let fields: Vec<(&str, &str)> = line.split('|').filter_map(|kv| kv.split_once('=')).collect();
        let get = |key: &str| fields.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
        let tag = |key: &str| get(key).filter(|v| !matches!(*v, "" | "unknown" | "unspecified" | "reserved")).map(str::to_owned);
        match get("codec_type") {
            Some("video") if video.is_none() => {
                video = Some((get("width"), get("height"), get("r_frame_rate"), get("avg_frame_rate")));
                colors = [tag("color_space"), tag("color_range"), tag("color_primaries"), tag("color_transfer")];
            }
            Some("audio") if audio_codec.is_none() => audio_codec = Some(get("codec_name").unwrap_or("").to_owned()),
            Some(_) => {}
            None => duration = duration.or(get("duration").and_then(|d| d.parse::<f64>().ok()).filter(|d| *d > 0.0)),
        }
    }

    let (w, h, r, avg) =
        video.ok_or(tr("The file has no video track.", "El archivo no contiene ninguna pista de vídeo."))?;
    let no_resolution = || tr("Could not read the resolution.", "No se pudo leer la resolución.");
    let width = w.and_then(|v| v.parse().ok()).ok_or_else(no_resolution)?;
    let height = h.and_then(|v| v.parse().ok()).ok_or_else(no_resolution)?;
    // r_frame_rate is the nominal rate; some containers report a timebase-like
    // value there (e.g. 90000), so prefer the average rate when they disagree.
    let fps = match (r.and_then(Rational::parse), avg.and_then(Rational::parse)) {
        (Some(r), Some(a)) if r.value() > a.value() * 1.5 => a,
        (Some(r), _) => r,
        (None, Some(a)) => a,
        (None, None) => return Err(tr("Could not read the frame rate.", "No se pudo leer la velocidad de fotogramas.").into()),
    };
    let [color_space, color_range, color_primaries, color_transfer] = colors;
    Ok(VideoInfo {
        width,
        height,
        fps,
        duration,
        audio_codec,
        color_space,
        color_range,
        color_primaries,
        color_transfer,
    })
}

// ---------------------------------------------------------------------------
// Processing

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// Neural interpolation (RIFE) on the GPU; see `rife.rs`.
    Rife,
    /// Blends neighbouring frames. Fast, but moving objects ghost.
    FrameMix,
    /// Motion-compensated interpolation: estimates per-block motion in both
    /// directions and warps the frames to synthesise real in-between frames.
    PixelMotion,
}

pub struct Request {
    pub input: PathBuf,
    pub output: PathBuf,
    pub info: VideoInfo,
    /// Output frames per source frame; 1 for motion blur alone.
    pub multiplier: u64,
    pub method: Method,
    /// Motion blur shutter angle in degrees (180° is a natural film look).
    /// Always rendered with RIFE's optical flow.
    pub shutter: Option<f32>,
}

fn video_filter(req: &Request) -> String {
    let target = req.info.fps.times(req.multiplier);
    let fps = format!("{}/{}", target.num, target.den);
    let mut chain = Vec::new();
    // yuv420p/H.264 needs even dimensions.
    if req.info.width % 2 == 1 || req.info.height % 2 == 1 {
        chain.push("crop=trunc(iw/2)*2:trunc(ih/2)*2".to_owned());
    }
    // Convert first: interpolating 8-bit 4:2:0 is much faster than 10-bit or RGB.
    chain.push("format=yuv420p".to_owned());
    chain.push(match req.method {
        Method::FrameMix => format!("framerate=fps={fps}"),
        Method::PixelMotion => format!(
            "minterpolate=fps={fps}:mi_mode=mci:mc_mode=aobmc:me_mode=bidir:me=epzs\
             :mb_size=16:search_param=32:vsbmc=1:scd=fdiff:scd_threshold=10"
        ),
        Method::Rife => unreachable!("RIFE frames are generated in rife.rs, not by an FFmpeg filter"),
    });
    chain.join(",")
}

pub(crate) fn strs<const N: usize>(a: [&str; N]) -> impl Iterator<Item = OsString> {
    a.into_iter().map(OsString::from)
}

/// Audio and container options shared by every method.
pub(crate) fn output_args(req: &Request) -> Vec<OsString> {
    let mut args = Vec::new();
    match req.info.audio_codec.as_deref() {
        None => {}
        // Re-encoding AAC to AAC only loses quality.
        Some("aac") => args.extend(strs(["-c:a", "copy"])),
        Some(_) => args.extend(strs(["-c:a", "aac", "-b:a", "192k"])),
    }
    let ext = req.output.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if ext == "mp4" || ext == "mov" {
        args.extend(strs(["-movflags", "+faststart"]));
    }
    args.push(req.output.clone().into());
    args
}

fn ffmpeg_args(req: &Request) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    args.extend(strs(["-hide_banner", "-nostdin", "-y", "-loglevel", "error", "-nostats", "-progress", "pipe:1"]));
    args.extend(["-i".into(), req.input.clone().into()]);
    args.extend(strs(["-map", "0:v:0", "-map", "0:a:0?"]));
    args.extend(["-vf".into(), video_filter(req).into()]);
    args.extend(strs(["-c:v", "libx264", "-preset", "medium", "-crf", "17", "-pix_fmt", "yuv420p"]));
    args.extend(output_args(req));
    args
}

pub enum Event {
    Progress { fraction: Option<f32>, eta: Option<Duration> },
    /// Something the user should know while the job runs (e.g. a CPU fallback).
    Notice(String),
    Done { elapsed: Duration },
    Failed(String),
    Cancelled,
}

pub struct Job {
    pub events: Receiver<Event>,
    cancel: Arc<AtomicBool>,
}

impl Job {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Starts the encode on a background thread. `notify` is called after every
/// event so the UI can repaint.
pub fn start(tools: &Tools, req: Request, notify: impl Fn() + Send + 'static) -> Job {
    let (tx, events) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let tools = tools.clone();
    let flag = cancel.clone();
    thread::spawn(move || {
        let report = |event: Event| {
            let _ = tx.send(event);
            notify();
        };
        let event = if req.method == Method::Rife || req.shutter.is_some() {
            crate::rife::run(&tools, &req, &flag, &report)
        } else {
            run_filter(&tools.ffmpeg, &req, &flag, &report)
        };
        if !matches!(event, Event::Done { .. }) {
            let _ = std::fs::remove_file(&req.output);
        }
        report(event);
    });
    Job { events, cancel }
}

fn run_filter(ffmpeg: &Path, req: &Request, cancel: &AtomicBool, report: &dyn Fn(Event)) -> Event {
    let mut child = match command(ffmpeg)
        .args(ffmpeg_args(req))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Event::Failed(start_error(&e)),
    };

    // Both pipes are drained on their own threads so ffmpeg never blocks on them.
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let (line_tx, lines) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let stderr_reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let total_us = req.info.duration.map(|d| d * 1_000_000.0);
    let started = Instant::now();
    let mut out_time_us = 0.0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Event::Cancelled;
        }
        match lines.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                if let Some(v) = line.strip_prefix("out_time_us=") {
                    out_time_us = v.trim().parse().unwrap_or(out_time_us);
                } else if line.starts_with("progress=") {
                    let fraction = total_us.map(|t| (out_time_us / t).clamp(0.0, 1.0) as f32);
                    let eta = fraction.filter(|f| *f > 0.01).map(|f| started.elapsed().mul_f32((1.0 - f) / f));
                    report(Event::Progress { fraction, eta });
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let status = child.wait();
    let errors = stderr_reader.join().unwrap_or_default();
    match status {
        Ok(s) if s.success() => Event::Done { elapsed: started.elapsed() },
        Ok(s) => Event::Failed(match last_lines(&errors, 4) {
            msg if msg.is_empty() => trf!("ffmpeg failed ({s}).", "ffmpeg terminó con error ({s})."),
            msg => msg,
        }),
        Err(e) => Event::Failed(trf!("Error waiting for ffmpeg: {e}", "Error esperando a ffmpeg: {e}")),
    }
}

pub(crate) fn start_error(e: &std::io::Error) -> String {
    trf!("Could not start ffmpeg: {e}", "No se pudo iniciar ffmpeg: {e}")
}

pub(crate) fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_the_ffmpeg_folder() {
        let dir = std::env::temp_dir().join(format!("silk-saved-{}", std::process::id()));
        let file = dir.join("Silk").join("ffmpeg-folder.txt");
        assert_eq!(read_saved(&file), None, "nothing saved yet");
        let picked = Path::new(r"C:\Program Files\FFmpeg ñ\bin");
        save(&file, picked).unwrap();
        assert_eq!(read_saved(&file).as_deref(), Some(picked));
        std::fs::write(&file, "  \r\n").unwrap();
        assert_eq!(read_saved(&file), None, "a blank file means nothing saved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_rationals() {
        assert_eq!(Rational::parse("30000/1001"), Some(Rational { num: 30000, den: 1001 }));
        assert_eq!(Rational::parse("25"), Some(Rational { num: 25, den: 1 }));
        assert_eq!(Rational::parse("0/0"), None);
        assert_eq!(Rational::parse("N/A"), None);
    }

    #[test]
    fn pixel_motion_filter_uses_mci_aobmc_bidir() {
        let req = Request {
            input: "in.mp4".into(),
            output: "out.mp4".into(),
            info: VideoInfo {
                width: 1921,
                height: 1080,
                fps: Rational { num: 30000, den: 1001 },
                duration: Some(1.0),
                audio_codec: None,
                color_space: None,
                color_range: None,
                color_primaries: None,
                color_transfer: None,
            },
            multiplier: 2,
            method: Method::PixelMotion,
            shutter: None,
        };
        let f = video_filter(&req);
        assert!(f.starts_with("crop="), "{f}");
        assert!(f.contains("minterpolate=fps=60000/1001:mi_mode=mci:mc_mode=aobmc:me_mode=bidir"), "{f}");
    }

    /// Encodes a generated clip with every method (RIFE only when its model is
    /// present). Skipped when FFmpeg isn't found.
    #[test]
    fn end_to_end() {
        let Some(tools) = Tools::locate() else {
            eprintln!("ffmpeg not found; skipping");
            return;
        };
        let dir = std::env::temp_dir().join(format!("silk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.mp4");
        let status = command(&tools.ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=24", "-f", "lavfi"])
            .args(["-i", "sine=frequency=440", "-t", "2", "-vf", "scale=321:181", "-pix_fmt", "yuv444p", "-c:v", "libx264", "-shortest"])
            .arg(&input)
            .status()
            .unwrap();
        assert!(status.success());

        let info = probe(&tools, &input).unwrap();
        assert_eq!((info.width, info.height, info.fps), (321, 181, Rational { num: 24, den: 1 }));
        assert_eq!(info.audio_codec.as_deref(), Some("aac"));

        let mut methods = vec![(Method::PixelMotion, 2, None), (Method::FrameMix, 3, None)];
        if crate::rife::locate_model().is_some() {
            // Interpolation, motion blur alone, and both.
            methods.extend([(Method::Rife, 2, None), (Method::Rife, 1, Some(180.0)), (Method::Rife, 3, Some(270.0))]);
        }
        for (method, multiplier, shutter) in methods {
            let output = dir.join(format!("out_{method:?}_{multiplier}_{shutter:?}.mp4"));
            let req = Request { input: input.clone(), output: output.clone(), info: info.clone(), multiplier, method, shutter };
            let job = start(&tools, req, || {});
            let last = job.events.iter().last().unwrap();
            assert!(matches!(last, Event::Done { .. }), "{method:?} failed: {}", match last {
                Event::Failed(m) => m,
                _ => String::new(),
            });
            let out = probe(&tools, &output).unwrap();
            assert_eq!((out.width, out.height), (320, 180));
            assert_eq!(out.fps.value().round() as u64, 24 * multiplier);
            assert!(out.audio_codec.is_some());
            assert!((out.duration.unwrap() - 2.0).abs() < 0.2, "{:?}", out.duration);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Manual run without the UI:
    /// `SILK_IN=a.mp4 SILK_OUT=b.mp4 [SILK_METHOD=rife|pm|mix] [SILK_X=2]
    ///  [SILK_BLUR=180] cargo test --release process_file -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn process_file() {
        let var = |k: &str| std::env::var(k).ok();
        let tools = Tools::locate().expect("ffmpeg");
        let input = PathBuf::from(var("SILK_IN").expect("SILK_IN"));
        let output = PathBuf::from(var("SILK_OUT").expect("SILK_OUT"));
        let method = match var("SILK_METHOD").as_deref() {
            Some("pm") => Method::PixelMotion,
            Some("mix") => Method::FrameMix,
            _ => Method::Rife,
        };
        let multiplier = var("SILK_X").map_or(2, |x| x.parse().unwrap());
        let shutter = var("SILK_BLUR").map(|d| d.parse().unwrap());
        let info = probe(&tools, &input).unwrap();
        println!("{info:?}");
        let job = start(&tools, Request { input, output, info, multiplier, method, shutter }, || {});
        let mut last_print = Instant::now();
        for event in job.events.iter() {
            match event {
                Event::Progress { fraction, eta } if last_print.elapsed().as_secs() >= 2 => {
                    last_print = Instant::now();
                    println!("progress {fraction:?} eta {eta:?}");
                }
                Event::Progress { .. } => {}
                Event::Notice(n) => println!("notice: {n}"),
                Event::Done { elapsed } => println!("done in {elapsed:?}"),
                Event::Failed(e) => panic!("failed: {e}"),
                Event::Cancelled => panic!("cancelled"),
            }
        }
    }
}
