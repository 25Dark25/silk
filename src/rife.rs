//! AI frame interpolation with RIFE v4.25 running on the GPU through ONNX
//! Runtime + DirectML.
//!
//! One FFmpeg decodes the input to planar RGB on a pipe, this module builds
//! the output timeline and synthesises the in-between frames, and a second
//! FFmpeg encodes them and muxes the original audio. Nothing touches the disk.
//!
//! Two things matter as much as the model for gameplay footage:
//! - Duplicate frames (the game ran slower than the recording) are dropped and
//!   the gap is re-interpolated, otherwise the result stutters.
//! - Scene cuts are not interpolated, otherwise one shot melts into the next.

use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use ort::ep::{self, directml::PerformancePreference};
use ort::environment::Environment;
use ort::session::{HasSelectedOutputs, OutputSelector, RunOptions, Session, SessionOutputs};
use ort::value::TensorRef;

use crate::blur::{self, PairMotion};
use crate::ffmpeg::{self, Event, Request, Tools, VideoInfo, command, last_lines, strs};

/// RIFE v4.25 (vs-mlrt's "v2" ONNX export) with an extra `flow` output: the
/// network's final optical flow, which the motion blur reuses.
const MODEL_FILE: &str = "rife_v4.25_flow.onnx";

/// Longest run of duplicates that is treated as dropped frames. Longer runs
/// are a genuinely static picture (pause menu, loading screen) and are kept.
const MAX_DUPLICATE_RUN: u32 = 3;

/// Looks for the model in `models/` next to the executable or in a parent
/// folder (which covers `target/release` during development).
pub fn locate_model() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors().skip(1).take(4).map(|dir| dir.join("models").join(MODEL_FILE)).find(|p| p.is_file())
}

// ---------------------------------------------------------------------------
// Model

/// RIFE behind compact inputs and outputs: 8-bit frames and a scalar
/// timestep go in, and the frame comes back either as 8-bit `gbrp`, ready
/// for the encoder, or as floats for the motion blur. The conversions happen
/// in the graph with the same arithmetic the CPU used (/255 in, and
/// floor(v·255 + 0.5) out), so only a quarter of the data crosses the bus.
struct Model {
    session: Session,
    frame_only: RunOptions<HasSelectedOutputs>,
    float_only: RunOptions<HasSelectedOutputs>,
    float_and_flow: RunOptions<HasSelectedOutputs>,
    width: usize,
    height: usize,
    /// `[frame A | frame B]`, planar RGB, 8-bit.
    frames: Vec<u8>,
}

/// What RIFE returns for one timestep, as floats.
struct Synthesis {
    /// Planar RGB, 0..1.
    rgb: Vec<f32>,
    /// Planar `[to A x | to A y | to B x | to B y]` in pixels: for each output
    /// pixel, where it sits in frame A and in frame B. Only when requested.
    flow: Option<Vec<f32>>,
}

impl Model {
    fn load(path: &Path, width: usize, height: usize, report: &dyn Fn(Event)) -> Result<Self, String> {
        static ENV: OnceLock<Environment> = OnceLock::new();
        if ENV.get().is_none() {
            let env = ort::init()
                .build()
                .map_err(|e| trf!("Could not start ONNX Runtime: {e}", "No se pudo iniciar ONNX Runtime: {e}"))?;
            let _ = ENV.set(env);
        }
        let env = ENV.get().expect("initialised above");

        let builder = |gpu: bool| -> ort::Result<Session> {
            let mut b = Session::builder(env)?
                .with_memory_pattern(false)?
                .with_parallel_execution(false)?
                .with_dimension_override("batch", 1)?
                .with_dimension_override("height", height as i64)?
                .with_dimension_override("width", width as i64)?;
            if gpu {
                // High performance picks the dedicated GPU on laptops with two.
                b = b.with_execution_providers([ep::DirectML::default()
                    .with_performance_preference(PerformancePreference::HighPerformance)
                    .build()
                    .error_on_failure()])?;
            }
            b.commit_from_file(path)
        };
        let session = match builder(true) {
            Ok(session) => session,
            Err(gpu_err) => {
                report(Event::Notice(trf!(
                    "The GPU could not be used ({gpu_err}). Falling back to the CPU, which is much slower.",
                    "No se pudo usar la GPU ({gpu_err}). Se usará la CPU, que es mucho más lenta."
                )));
                builder(false)
                    .map_err(|e| trf!("Could not load the RIFE model: {e}", "No se pudo cargar el modelo RIFE: {e}"))?
            }
        };
        Self::with_session(session, width, height)
    }

    fn with_session(session: Session, width: usize, height: usize) -> Result<Self, String> {
        let select = |names: &[&str]| -> ort::Result<RunOptions<HasSelectedOutputs>> {
            let selector = names.iter().fold(OutputSelector::no_default(), |s, name| s.with(*name));
            Ok(RunOptions::new()?.with_outputs(selector))
        };
        let err = |e: ort::Error| e.to_string();
        Ok(Self {
            session,
            frame_only: select(&["output_gbrp8"]).map_err(err)?,
            float_only: select(&["output"]).map_err(err)?,
            float_and_flow: select(&["output", "flow"]).map_err(err)?,
            width,
            height,
            frames: vec![0; 6 * width * height],
        })
    }

    fn plane(&self) -> usize {
        self.width * self.height
    }

    /// Loads a decoded `gbrp` frame as frame A (slot 0) or B (slot 1).
    fn set_frame(&mut self, slot: usize, gbrp: &[u8]) {
        let p = self.plane();
        let dst = &mut self.frames[slot * 3 * p..(slot + 1) * 3 * p];
        let (g, rest) = gbrp.split_at(p);
        let (b, r) = rest.split_at(p);
        for (plane, out) in [r, g, b].into_iter().zip(dst.chunks_exact_mut(p)) {
            out.copy_from_slice(plane);
        }
    }

    /// Frame B becomes frame A for the next pair.
    fn advance(&mut self) {
        let p = self.plane();
        self.frames.copy_within(3 * p..6 * p, 0);
    }

    fn run(&mut self, t: f32, options: Selection) -> Result<SessionOutputs<'_>, String> {
        let shape = [1usize, 6, self.height, self.width];
        let frames = TensorRef::from_array_view((shape, &self.frames[..])).map_err(|e| e.to_string())?;
        let t = [t];
        let timestep = TensorRef::from_array_view(([1usize], &t[..])).map_err(|e| e.to_string())?;
        let options = match options {
            Selection::Frame => &self.frame_only,
            Selection::Float => &self.float_only,
            Selection::FloatAndFlow => &self.float_and_flow,
        };
        self.session
            .run_with_options(ort::inputs!["frames" => frames, "timestep" => timestep], options)
            .map_err(|e| trf!("Inference error: {e}", "Error de inferencia: {e}"))
    }

    /// The frame at `t`, as 8-bit `gbrp` for the encoder.
    fn interpolate(&mut self, t: f32) -> Result<Vec<u8>, String> {
        let p = self.plane();
        let outputs = self.run(t, Selection::Frame)?;
        let (_, data) = outputs["output_gbrp8"].try_extract_tensor::<u8>().map_err(|e| e.to_string())?;
        if data.len() < 3 * p {
            return Err(size_error(data.len(), 3 * p));
        }
        Ok(data[..3 * p].to_vec())
    }

    /// The frame at `t` as floats, for the motion blur, optionally with the flow.
    fn interpolate_float(&mut self, t: f32, want_flow: bool) -> Result<Synthesis, String> {
        let (p, width, height) = (self.plane(), self.width, self.height);
        let outputs = self.run(t, if want_flow { Selection::FloatAndFlow } else { Selection::Float })?;
        let (_, data) = outputs["output"].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
        if data.len() < 3 * p {
            return Err(size_error(data.len(), 3 * p));
        }
        let rgb = data[..3 * p].to_vec();
        let flow = if want_flow {
            let (shape, data) = outputs["flow"].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
            Some(crop_flow(shape, data, width, height)?)
        } else {
            None
        };
        Ok(Synthesis { rgb, flow })
    }
}

#[derive(Clone, Copy)]
enum Selection {
    Frame,
    Float,
    FloatAndFlow,
}

fn size_error(got: usize, expected: usize) -> String {
    trf!(
        "The model returned {got} values, expected {expected}.",
        "El modelo devolvió {got} valores, se esperaban {expected}."
    )
}

/// The flow comes at the model's internally padded size; keep the top-left
/// `width`×`height` (the padding is added at the bottom and right).
fn crop_flow(shape: &ort::value::Shape, data: &[f32], width: usize, height: usize) -> Result<Vec<f32>, String> {
    let dims: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
    let [_, 4, ph, pw] = dims[..] else {
        return Err(trf!("Unexpected flow shape: {dims:?}", "Forma de flujo inesperada: {dims:?}"));
    };
    if ph < height || pw < width {
        return Err(trf!(
            "Flow of {pw}×{ph} is smaller than the video ({width}×{height}).",
            "Flujo de {pw}×{ph} menor que el vídeo ({width}×{height})."
        ));
    }
    let mut out = Vec::with_capacity(4 * width * height);
    for c in 0..4 {
        for y in 0..height {
            let row = (c * ph + y) * pw;
            out.extend_from_slice(&data[row..row + width]);
        }
    }
    Ok(out)
}

/// FFmpeg's `gbrp` is planar G, B, R; RIFE wants planar R, G, B in 0..1.
fn gbrp_to_rgb(src: &[u8], dst: &mut [f32]) {
    let lut: [f32; 256] = std::array::from_fn(|i| i as f32 / 255.0);
    let p = src.len() / 3;
    let (g, rest) = src.split_at(p);
    let (b, r) = rest.split_at(p);
    // One thread per plane.
    thread::scope(|scope| {
        for (plane, out) in [r, g, b].into_iter().zip(dst.chunks_exact_mut(p)) {
            scope.spawn(move || {
                for (o, &v) in out.iter_mut().zip(plane) {
                    *o = lut[v as usize];
                }
            });
        }
    });
}

fn rgb_to_gbrp(src: &[f32], dst: &mut [u8]) {
    let p = src.len() / 3;
    let (r, rest) = src.split_at(p);
    let (g, b) = rest.split_at(p);
    thread::scope(|scope| {
        for (plane, out) in [g, b, r].into_iter().zip(dst.chunks_exact_mut(p)) {
            scope.spawn(move || {
                for (o, &v) in out.iter_mut().zip(plane) {
                    *o = (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
                }
            });
        }
    });
}

// ---------------------------------------------------------------------------
// Frame analysis (on the green plane, which carries most of the luma)

/// True when no 16×16 block changed visibly: a repeated frame, allowing for
/// compression noise. Judged per block so a small moving cursor still counts.
fn is_duplicate(a: &[u8], b: &[u8], width: usize, height: usize) -> bool {
    const BLOCK: usize = 16;
    const THRESHOLD: u32 = 3; // mean absolute difference per sampled pixel, 0..255
    for by in (0..height.saturating_sub(BLOCK - 1)).step_by(BLOCK) {
        for bx in (0..width.saturating_sub(BLOCK - 1)).step_by(BLOCK) {
            let mut sum = 0u32;
            for y in (by..by + BLOCK).step_by(2) {
                let row = y * width;
                for x in (bx..bx + BLOCK).step_by(2) {
                    sum += a[row + x].abs_diff(b[row + x]) as u32;
                }
            }
            if sum > THRESHOLD * (BLOCK as u32 / 2).pow(2) {
                return false;
            }
        }
    }
    true
}

/// A hard cut: the brightness distribution moves a lot, or the picture
/// changes far more than its overall brightness explains.
///
/// The distribution shift is the earth mover's distance between the two
/// histograms, in levels: a fade or flash that brightens everything by N
/// levels scores N. A bin-by-bin comparison would instead see a flat
/// background fading by a few levels as a completely different picture.
fn is_scene_cut(a: &[u8], b: &[u8], width: usize, height: usize) -> bool {
    let (mut ha, mut hb) = ([0i64; 256], [0i64; 256]);
    let (mut diff, mut n) = (0u64, 0u64);
    for y in (0..height).step_by(4) {
        let row = y * width;
        for x in (0..width).step_by(4) {
            let (va, vb) = (a[row + x], b[row + x]);
            ha[va as usize] += 1;
            hb[vb as usize] += 1;
            diff += va.abs_diff(vb) as u64;
            n += 1;
        }
    }
    let (mut ca, mut cb, mut moved) = (0i64, 0i64, 0u64);
    for (x, y) in ha.iter().zip(&hb) {
        ca += x;
        cb += y;
        moved += ca.abs_diff(cb);
    }
    let shift = moved as f64 / n as f64;
    let mean = diff as f64 / n as f64;
    shift > 40.0 || mean - shift > 50.0
}

// ---------------------------------------------------------------------------
// Colour handling. The YUV -> RGB conversion on the way in and the RGB -> YUV
// conversion on the way out use the same matrix and range, and the output
// carries exactly the tags the source had, so any player shows it the same
// way it shows the original (tagged or not).

struct Colors {
    /// For the `scale` filter, in both directions.
    matrix: &'static str,
    range: &'static str,
    /// A `setparams` filter that gives the output frames the source's tags,
    /// including leaving untagged what was untagged.
    tags: String,
}

impl Colors {
    fn of(info: &VideoInfo) -> Self {
        let matrix = match info.color_space.as_deref() {
            Some("bt709") => "bt709",
            Some("bt2020nc") | Some("bt2020c") => "bt2020",
            Some("fcc") => "fcc",
            Some("smpte240m") => "smpte240m",
            // Tagged BT.601, or untagged: FFmpeg's own default for YUV.
            _ => "bt601",
        };
        let range = if info.color_range.as_deref() == Some("pc") { "pc" } else { "tv" };
        let tag = |v: &Option<String>| v.clone().unwrap_or_else(|| "unknown".into());
        let tags = format!(
            "setparams=colorspace={}:range={}:color_primaries={}:color_trc={}",
            tag(&info.color_space),
            tag(&info.color_range),
            tag(&info.color_primaries),
            tag(&info.color_transfer),
        );
        Self { matrix, range, tags }
    }
}

// ---------------------------------------------------------------------------
// Pipeline

enum OutFrame {
    /// Ready for the encoder: 8-bit `gbrp`.
    Source(Arc<Vec<u8>>),
    Blur(Box<BlurJob>),
}

enum Image {
    Source(Arc<Vec<u8>>),
    Rgb(Vec<f32>),
}

/// A pair's motion, built from RIFE's midpoint flow on a thread of its own,
/// so neither the thread feeding the GPU nor the writer waits for it.
struct LazyPair {
    motion: OnceLock<PairMotion>,
    building: Mutex<Option<thread::JoinHandle<PairMotion>>>,
}

impl LazyPair {
    fn new(flow: Vec<f32>, image: Vec<f32>, width: usize, height: usize) -> Self {
        let building = thread::spawn(move || PairMotion::new(&flow, image, width, height));
        Self { motion: OnceLock::new(), building: Mutex::new(Some(building)) }
    }

    fn get(&self) -> &PairMotion {
        self.motion.get_or_init(|| {
            let handle = self.building.lock().unwrap().take().expect("joined once");
            handle.join().expect("motion thread panicked")
        })
    }
}

/// An output frame to motion-blur on the writer thread, which uses every
/// core while the GPU already works on the next frames.
struct BlurJob {
    image: Image,
    /// Position of this frame within `pair` (0 = its first frame).
    tau: f32,
    pair: Arc<LazyPair>,
    /// Source-frame intervals the pair spans (more than 1 after duplicates).
    span: f32,
    /// For a frame that starts its pair: the pair that ended on it, so the
    /// first half of the exposure follows the real earlier motion.
    previous: Option<(Arc<LazyPair>, f32)>,
    /// Half the exposure, in source-frame intervals.
    half_window: f32,
}

impl BlurJob {
    fn render(self, width: usize, height: usize) -> Vec<f32> {
        let rgb = match self.image {
            Image::Rgb(rgb) => rgb,
            Image::Source(src) => {
                let mut rgb = vec![0f32; src.len()];
                gbrp_to_rgb(&src, &mut rgb);
                rgb
            }
        };
        let ahead = self.pair.get().at(&rgb, self.tau, 1.0 / self.span);
        let behind = match &self.previous {
            Some((pair, span)) => pair.get().at(&rgb, 1.0, 1.0 / span),
            None => ahead.clone(),
        };
        blur::motion_blur(&rgb, &ahead, &behind, width, height, self.half_window)
    }
}

struct Processes {
    decoder: Child,
    encoder: Child,
}

impl Processes {
    fn kill(&mut self) {
        for child in [&mut self.decoder, &mut self.encoder] {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_reader(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut text = String::new();
        let _ = pipe.read_to_string(&mut text);
        text
    })
}

pub fn run(tools: &Tools, req: &Request, cancel: &AtomicBool, report: &dyn Fn(Event)) -> Event {
    let Some(model_path) = locate_model() else {
        return Event::Failed(trf!(
            "The model models\\{MODEL_FILE} was not found next to the app.",
            "No se encontró el modelo models\\{MODEL_FILE} junto a la aplicación."
        ));
    };
    let info = &req.info;
    // H.264 4:2:0 needs even dimensions.
    let (width, height) = ((info.width & !1) as usize, (info.height & !1) as usize);
    let frame_bytes = width * height * 3;
    let multiplier = req.multiplier;
    // A shutter of 360° exposes for the whole output frame interval.
    let half_window = req.shutter.map(|degrees| degrees / 360.0 / multiplier as f32 / 2.0);
    let colors = Colors::of(info);
    let started = Instant::now();

    report(Event::Progress { fraction: None, eta: None });
    let mut model = match Model::load(&model_path, width, height, report) {
        Ok(model) => model,
        Err(e) => return Event::Failed(e),
    };

    // Decoder: constant frame rate (fixes variable-rate recordings), planar RGB.
    let mut vf = Vec::new();
    if info.width % 2 == 1 || info.height % 2 == 1 {
        vf.push("crop=trunc(iw/2)*2:trunc(ih/2)*2".to_owned());
    }
    vf.push(format!("fps={}/{}", info.fps.num, info.fps.den));
    vf.push(format!("scale=in_color_matrix={}:in_range={},format=gbrp", colors.matrix, colors.range));
    let decoder = command(&tools.ffmpeg)
        .args(strs(["-hide_banner", "-nostdin", "-loglevel", "error", "-i"]))
        .arg(&req.input)
        .args(strs(["-map", "0:v:0", "-vf"]))
        .arg(vf.join(","))
        .args(strs(["-f", "rawvideo", "pipe:1"]))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let target = info.fps.times(multiplier);
    let encoder = command(&tools.ffmpeg)
        .args(strs(["-hide_banner", "-nostdin", "-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "gbrp"]))
        .arg("-s")
        .arg(format!("{width}x{height}"))
        .arg("-framerate")
        .arg(format!("{}/{}", target.num, target.den))
        .args(strs(["-i", "pipe:0", "-i"]))
        .arg(&req.input)
        .args(strs(["-map", "0:v:0", "-map", "1:a:0?", "-vf"]))
        .arg(format!(
            "scale=out_color_matrix={}:out_range={},format=yuv420p,{}",
            colors.matrix, colors.range, colors.tags
        ))
        // Plenty of CPU is left while the GPU works, so spend it on compression,
        // unless the motion blur needs it.
        .args(strs(["-c:v", "libx264", "-preset", if half_window.is_some() { "medium" } else { "slow" }]))
        .args(strs(["-crf", "16"]))
        .args(ffmpeg::output_args(req))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();

    let (mut procs, decoder_errors, encoder_errors, frames, stdin) = match (decoder, encoder) {
        (Ok(mut decoder), Ok(mut encoder)) => {
            let stdout = decoder.stdout.take().expect("piped");
            let decoder_errors = spawn_reader(decoder.stderr.take().expect("piped"));
            let encoder_errors = spawn_reader(encoder.stderr.take().expect("piped"));
            let stdin = encoder.stdin.take().expect("piped");
            // Small bounded queues keep the GPU busy while FFmpeg decodes and
            // encodes on other cores, without buffering the whole video.
            let (frame_tx, frames) = mpsc::sync_channel::<Vec<u8>>(4);
            thread::spawn(move || {
                let mut reader = BufReader::with_capacity(1 << 20, stdout);
                loop {
                    let mut frame = vec![0u8; frame_bytes];
                    if reader.read_exact(&mut frame).is_err() || frame_tx.send(frame).is_err() {
                        break;
                    }
                }
            });
            (Processes { decoder, encoder }, decoder_errors, encoder_errors, frames, stdin)
        }
        (Err(e), Ok(mut other)) | (Ok(mut other), Err(e)) => {
            let _ = other.kill();
            return Event::Failed(ffmpeg::start_error(&e));
        }
        (Err(e), Err(_)) => return Event::Failed(ffmpeg::start_error(&e)),
    };

    let (out_tx, out_rx) = mpsc::sync_channel::<OutFrame>(6);
    let writer = thread::spawn(move || -> std::io::Result<()> {
        let mut stdin = stdin;
        let mut bytes = vec![0u8; frame_bytes];
        for frame in out_rx {
            match frame {
                OutFrame::Source(src) => stdin.write_all(&src)?,
                OutFrame::Blur(job) => {
                    rgb_to_gbrp(&job.render(width, height), &mut bytes);
                    stdin.write_all(&bytes)?;
                }
            }
        }
        stdin.flush()
    });

    let expected_out = info.duration.map(|d| (d * info.fps.value()).round().max(1.0) as u64 * multiplier);
    let mut timeline = Timeline::new(multiplier);
    let mut written = 0u64;
    let mut first_output: Option<Instant> = None;
    let mut last_report = Instant::now();

    // Queues one output frame; false once the encoder has gone away (its
    // error is collected below).
    let mut emit = |frame: OutFrame| -> bool {
        if out_tx.send(frame).is_err() {
            return false;
        }
        written += 1;
        let first = *first_output.get_or_insert_with(Instant::now);
        if last_report.elapsed() > Duration::from_millis(150) {
            last_report = Instant::now();
            let fraction = expected_out.map(|n| (written as f32 / n as f32).min(1.0));
            let eta = fraction.filter(|f| *f > 0.005).map(|f| first.elapsed().mul_f32((1.0 - f) / f));
            report(Event::Progress { fraction, eta });
        }
        true
    };

    // Ok(false) means cancelled.
    let outcome = (|| -> Result<bool, String> {
        // The motion of the pair that ended on the current frame A.
        let mut previous: Option<(Arc<LazyPair>, f32)> = None;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let frame = match frames.recv_timeout(Duration::from_millis(200)) {
                Ok(frame) => Arc::new(frame),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            match timeline.push(frame.clone(), width, height) {
                Step::First => model.set_frame(0, &frame),
                Step::Dropped => {}
                Step::Pair { cut, span, jobs } => {
                    if !cut {
                        model.set_frame(1, &frame);
                    }
                    // For motion blur, the pair's motion comes from RIFE at its
                    // midpoint, where the flow is reliable. Across a cut there
                    // is no motion to blur.
                    let mut mid_rgb = None;
                    let pair = match (half_window, cut) {
                        (Some(_), false) => {
                            let mid = model.interpolate_float(0.5, true)?;
                            let flow = mid.flow.expect("requested");
                            let pair = Arc::new(LazyPair::new(flow, mid.rgb.clone(), width, height));
                            mid_rgb = Some(mid.rgb);
                            Some(pair)
                        }
                        _ => None,
                    };
                    for job in jobs {
                        if cancel.load(Ordering::Relaxed) {
                            return Ok(false);
                        }
                        let out = match (job, &pair, half_window) {
                            (Emit::Copy(src), Some(pair), Some(half_window)) => OutFrame::Blur(Box::new(BlurJob {
                                image: Image::Source(src),
                                tau: 0.0,
                                pair: pair.clone(),
                                span: span as f32,
                                previous: previous.clone(),
                                half_window,
                            })),
                            (Emit::Interpolate(t), Some(pair), Some(half_window)) => {
                                let rgb = match mid_rgb.take() {
                                    Some(rgb) if t == 0.5 => rgb,
                                    other => {
                                        mid_rgb = other;
                                        model.interpolate_float(t, false)?.rgb
                                    }
                                };
                                OutFrame::Blur(Box::new(BlurJob {
                                    image: Image::Rgb(rgb),
                                    tau: t,
                                    pair: pair.clone(),
                                    span: span as f32,
                                    previous: None,
                                    half_window,
                                }))
                            }
                            (Emit::Copy(src), ..) => OutFrame::Source(src),
                            (Emit::Interpolate(t), ..) => OutFrame::Source(Arc::new(model.interpolate(t)?)),
                        };
                        if !emit(out) {
                            return Ok(true);
                        }
                    }
                    previous = pair.map(|pair| (pair, span as f32));
                    if cut {
                        model.set_frame(0, &frame);
                    } else {
                        model.advance();
                    }
                }
            }
        }
        for last in timeline.finish() {
            if !emit(OutFrame::Source(last)) {
                break;
            }
        }
        Ok(true)
    })();

    match outcome {
        Ok(true) => {}
        Ok(false) => {
            procs.kill();
            return Event::Cancelled;
        }
        Err(e) => {
            procs.kill();
            return Event::Failed(e);
        }
    }
    drop(out_tx);
    let write_result = writer.join().unwrap_or(Ok(()));
    let decoder_ok = procs.decoder.wait().is_ok_and(|s| s.success());
    let encoder_ok = procs.encoder.wait().is_ok_and(|s| s.success());
    let (dec_err, enc_err) = (decoder_errors.join().unwrap_or_default(), encoder_errors.join().unwrap_or_default());
    if !decoder_ok || timeline.source_frames == 0 {
        let detail = last_lines(&dec_err, 3);
        return Event::Failed(trf!("Could not decode the video.\n{detail}", "No se pudo decodificar el vídeo.\n{detail}"));
    }
    if !encoder_ok || write_result.is_err() {
        let detail = last_lines(&enc_err, 3);
        return Event::Failed(trf!("Could not encode the result.\n{detail}", "No se pudo codificar el resultado.\n{detail}"));
    }
    if timeline.dropped > 0 {
        let dropped = timeline.dropped;
        report(Event::Notice(trf!(
            "Rebuilt {dropped} repeated frames from the original.",
            "Se reconstruyeron {dropped} fotogramas repetidos del original."
        )));
    }
    Event::Done { elapsed: started.elapsed() }
}

#[derive(Debug)]
enum Emit {
    Copy(Arc<Vec<u8>>),
    Interpolate(f32),
}

#[derive(Debug)]
enum Step {
    /// The very first frame: becomes A.
    First,
    /// A repeated frame that will be re-interpolated.
    Dropped,
    /// Output frames due before this frame (B). Afterwards B becomes A.
    /// `span` is how many source intervals separate A and B.
    Pair { cut: bool, span: u64, jobs: Vec<Emit> },
}

/// Maps source frames to the output timeline. Output frame `k` sits at source
/// time `k / multiplier`; it is copied when it lands on a kept frame and
/// interpolated between the surrounding kept frames otherwise.
struct Timeline {
    multiplier: u64,
    /// Last kept frame (A) and its source index.
    kept: Option<(u64, Arc<Vec<u8>>)>,
    duplicate_run: u32,
    source_frames: u64,
    emitted: u64,
    dropped: u64,
}

impl Timeline {
    fn new(multiplier: u64) -> Self {
        Self { multiplier, kept: None, duplicate_run: 0, source_frames: 0, emitted: 0, dropped: 0 }
    }

    fn push(&mut self, frame: Arc<Vec<u8>>, width: usize, height: usize) -> Step {
        let index = self.source_frames;
        self.source_frames += 1;
        let Some((a_index, a)) = self.kept.clone() else {
            self.kept = Some((index, frame));
            return Step::First;
        };
        if self.duplicate_run < MAX_DUPLICATE_RUN && is_duplicate(&a, &frame, width, height) {
            self.duplicate_run += 1;
            self.dropped += 1;
            return Step::Dropped;
        }
        self.duplicate_run = 0;

        let cut = is_scene_cut(&a, &frame, width, height);
        let m = self.multiplier;
        let span = ((index - a_index) * m) as f32;
        let mut jobs = Vec::new();
        while self.emitted < index * m {
            let t = (self.emitted - a_index * m) as f32 / span;
            jobs.push(match () {
                _ if t == 0.0 => Emit::Copy(a.clone()),
                // Across a cut, show whichever shot is nearer in time.
                _ if cut => Emit::Copy(if t < 0.5 { a.clone() } else { frame.clone() }),
                _ => Emit::Interpolate(t),
            });
            self.emitted += 1;
        }
        self.kept = Some((index, frame));
        Step::Pair { cut, span: index - a_index, jobs }
    }

    /// The tail after the last kept frame is a copy of it (anything dropped
    /// there was a duplicate of it anyway).
    fn finish(&mut self) -> Vec<Arc<Vec<u8>>> {
        let Some((_, last)) = self.kept.clone() else { return Vec::new() };
        let total = self.source_frames * self.multiplier;
        let tail = (self.emitted..total).map(|_| last.clone()).collect();
        self.emitted = total;
        tail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_round_trip() {
        let src: Vec<u8> = (0..=255u8).cycle().take(3 * 64).collect();
        let mut rgb = vec![0.0; src.len()];
        let mut back = vec![0u8; src.len()];
        gbrp_to_rgb(&src, &mut rgb);
        rgb_to_gbrp(&rgb, &mut back);
        assert_eq!(src, back);
        // Plane order: gbrp's third plane (R) is RIFE's first.
        assert_eq!(rgb[0], src[128] as f32 / 255.0);
    }

    #[test]
    fn duplicates_ignore_noise_but_not_small_motion() {
        let (w, h) = (64, 64);
        let a = vec![100u8; w * h];
        let mut noisy = a.clone();
        noisy.iter_mut().enumerate().for_each(|(i, v)| *v += (i % 3) as u8);
        assert!(is_duplicate(&a, &noisy, w, h));
        let mut cursor = a.clone();
        for y in 20..30 {
            for x in 20..30 {
                cursor[y * w + x] = 255;
            }
        }
        assert!(!is_duplicate(&a, &cursor, w, h));
    }

    /// A 32×32 frame with a bright square at column `x`; `x` is also stored
    /// in the last byte so tests can tell copies apart.
    fn frame(x: u8) -> Arc<Vec<u8>> {
        let mut f = vec![50u8; 32 * 32 * 3];
        for y in 8..16 {
            f[y * 32 + x as usize..y * 32 + x as usize + 8].fill(200);
        }
        *f.last_mut().unwrap() = x;
        Arc::new(f)
    }

    fn flat(value: u8) -> Arc<Vec<u8>> {
        let mut f = vec![value; 32 * 32 * 3];
        *f.last_mut().unwrap() = value;
        Arc::new(f)
    }

    fn timesteps(steps: &[Step]) -> Vec<String> {
        let mut out = Vec::new();
        for step in steps {
            if let Step::Pair { jobs, .. } = step {
                for job in jobs {
                    out.push(match job {
                        Emit::Copy(f) => format!("copy{}", f[f.len() - 1]),
                        Emit::Interpolate(t) => format!("{t:.2}"),
                    });
                }
            }
        }
        out
    }

    #[test]
    fn timeline_doubles_frames() {
        let mut tl = Timeline::new(2);
        let steps: Vec<Step> = [10, 20, 30].map(|v| tl.push(frame(v), 32, 32)).into();
        assert_eq!(timesteps(&steps), ["copy10", "0.50", "copy20", "0.50"]);
        assert_eq!(tl.finish().len(), 2); // frame 30 and the slot after it
        assert_eq!(tl.emitted, 6);
    }

    #[test]
    fn timeline_reinterpolates_repeated_frames() {
        // A game at 30 fps recorded at 60: every frame appears twice.
        let mut tl = Timeline::new(2);
        let steps: Vec<Step> = [10, 10, 20, 20, 30].map(|v| tl.push(frame(v), 32, 32)).into();
        assert!(matches!(steps[1], Step::Dropped) && matches!(steps[3], Step::Dropped));
        // Between kept frames two source periods apart: quarter steps, no repeats.
        assert_eq!(timesteps(&steps), ["copy10", "0.25", "0.50", "0.75", "copy20", "0.25", "0.50", "0.75"]);
        assert_eq!(tl.dropped, 2);
    }

    #[test]
    fn timeline_keeps_long_static_runs() {
        let mut tl = Timeline::new(2);
        let steps: Vec<Step> = [10; 6].map(|v| tl.push(frame(v), 32, 32)).into();
        let dropped = steps.iter().filter(|s| matches!(s, Step::Dropped)).count();
        assert_eq!(dropped as u32, MAX_DUPLICATE_RUN + 1); // every (MAX+1)th repeat is kept
    }

    #[test]
    fn timeline_does_not_blend_across_cuts() {
        let mut tl = Timeline::new(2);
        let steps: Vec<Step> = [10, 240].map(|v| tl.push(flat(v), 32, 32)).into();
        assert!(matches!(steps[1], Step::Pair { cut: true, .. }));
        assert_eq!(timesteps(&steps), ["copy10", "copy240"]);
    }

    /// Planar RGB: a smooth static background and a textured square at
    /// (40 + dx, 30 + dy).
    fn scene(w: usize, h: usize, dx: usize, dy: usize) -> Vec<f32> {
        let mut rgb = vec![0f32; 3 * w * h];
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32, y as f32);
                let mut v = [0.4 + 0.25 * (fx * 0.31).sin() * (fy * 0.23).cos(), 0.4 + 0.2 * (fx * 0.13 + fy * 0.29).sin(), 0.45];
                let (sx, sy) = (x.wrapping_sub(40 + dx), y.wrapping_sub(30 + dy));
                if sx < 40 && sy < 40 {
                    let (sx, sy) = (sx as f32, sy as f32);
                    v = [0.85, 0.5 + 0.3 * (sx * 0.5).sin() * (sy * 0.4).cos(), 0.2];
                }
                for c in 0..3 {
                    rgb[c * w * h + y * w + x] = v[c];
                }
            }
        }
        rgb
    }

    /// Checks what the extra `flow` output means: per output pixel, the
    /// displacement to its position in A (channels 0-1) and in B (2-3).
    /// Near t = 0 or 1 RIFE ignores the far frame and that half of the flow is
    /// meaningless, which is why the blur only uses the flow at t = 0.5.
    #[test]
    fn flow_tracks_motion() {
        let Some(path) = locate_model() else { return };
        let (w, h) = (160, 96);
        let mut model = Model::load(&path, w, h, &|_| {}).unwrap();
        model.set_frame(0, &to_gbrp(&scene(w, h, 0, 0)));
        model.set_frame(1, &to_gbrp(&scene(w, h, 6, 3)));
        let mut failures = Vec::new();
        for t in [0.25f32, 0.5, 0.75] {
            let flow = model.interpolate_float(t, true).unwrap().flow.unwrap();
            let at = |c: usize, x: f32, y: f32| flow[c * w * h + y as usize * w + x as usize];
            // Centre of the square at time t, and a background point.
            let (x, y) = (60.0 + 6.0 * t, 50.0 + 3.0 * t);
            let square = [at(0, x, y), at(1, x, y), at(2, x, y), at(3, x, y)];
            let background = [at(0, 130.0, 80.0), at(1, 130.0, 80.0), at(2, 130.0, 80.0), at(3, 130.0, 80.0)];
            println!("t={t}: square {square:.2?} background {background:.2?}");
            let expected = [-6.0 * t, -3.0 * t, 6.0 * (1.0 - t), 3.0 * (1.0 - t)];
            for c in 0..4 {
                if (square[c] - expected[c]).abs() > 1.0 || background[c].abs() > 0.5 {
                    failures.push(format!("t={t} c={c}"));
                }
            }
        }
        assert!(failures.is_empty(), "{failures:?}");
    }

    fn to_gbrp(rgb: &[f32]) -> Vec<u8> {
        let mut bytes = vec![0u8; rgb.len()];
        rgb_to_gbrp(rgb, &mut bytes);
        bytes
    }

    /// Diagnostic: interpolates two raw `gbrp` frames on the GPU and on the
    /// CPU, writing both results, and checks that the 8-bit output rounded in
    /// the graph equals rounding the float output on the CPU.
    /// `SILK_A=a.raw SILK_B=b.raw SILK_W=3840 SILK_H=2160 cargo test
    ///  --release diagnose_pair -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn diagnose_pair() {
        let var = |k: &str| std::env::var(k).unwrap();
        let (w, h): (usize, usize) = (var("SILK_W").parse().unwrap(), var("SILK_H").parse().unwrap());
        let (a, b) = (std::fs::read(var("SILK_A")).unwrap(), std::fs::read(var("SILK_B")).unwrap());
        let path = locate_model().unwrap();
        let env = ort::init().build().unwrap();
        for gpu in [true, false] {
            let mut builder = Session::builder(&env).unwrap().with_memory_pattern(false).unwrap();
            if gpu {
                builder = builder
                    .with_execution_providers([ep::DirectML::default()
                        .with_performance_preference(PerformancePreference::HighPerformance)
                        .build()
                        .error_on_failure()])
                    .unwrap();
            }
            let mut model = Model::with_session(builder.commit_from_file(&path).unwrap(), w, h).unwrap();
            model.set_frame(0, &a);
            model.set_frame(1, &b);
            let mut bytes = model.interpolate(0.5).unwrap();
            let t = Instant::now();
            for _ in 0..3 {
                bytes = model.interpolate(0.5).unwrap();
            }
            let elapsed = t.elapsed() / 3;
            let rounded_on_cpu = to_gbrp(&model.interpolate_float(0.5, false).unwrap().rgb);
            let differing = bytes.iter().zip(&rounded_on_cpu).filter(|(x, y)| x != y).count();
            let name = format!("{}.{w}x{h}.{}.raw", var("SILK_A"), if gpu { "gpu" } else { "cpu" });
            std::fs::write(&name, &bytes).unwrap();
            println!("{name} in {elapsed:?}; 8-bit output vs CPU rounding: {differing} of {} values differ", bytes.len());
        }
    }

    #[test]
    fn scene_cut_detection() {
        let (w, h) = (64, 64);
        let dark = vec![20u8; w * h];
        let mut moved = dark.clone();
        moved[..w * 8].fill(200); // a bright object entering the top edge
        assert!(!is_scene_cut(&dark, &moved, w, h));
        assert!(is_scene_cut(&dark, &vec![220u8; w * h], w, h));
        // A flat background fading a few levels is not a cut (a bin-by-bin
        // histogram comparison said it was, freezing a frame mid-fade).
        assert!(!is_scene_cut(&vec![247u8; w * h], &vec![230u8; w * h], w, h));
        // Same brightness distribution, different picture: a cut.
        let checker: Vec<u8> = (0..w * h).map(|i| if (i / 4 + i / w / 4) % 2 == 0 { 30 } else { 220 }).collect();
        let inverted: Vec<u8> = checker.iter().map(|&v| 250 - v).collect();
        assert!(is_scene_cut(&checker, &inverted, w, h));
    }
}
