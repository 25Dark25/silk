#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod i18n;
mod blur;
mod ffmpeg;
mod rife;

use std::path::{Path, PathBuf};
use std::time::Duration;

use eframe::egui::{self, Color32, CornerRadius, RichText, Stroke, vec2};
use ffmpeg::{Event, Job, Method, Request, Tools, VideoInfo};
use i18n::tr;

const APP_NAME: &str = "Silk";
const ACCENT: Color32 = Color32::from_rgb(92, 124, 250);
const OK: Color32 = Color32::from_rgb(80, 200, 120);
const ERR: Color32 = Color32::from_rgb(240, 100, 100);
const VIDEO_EXTS: &[&str] = &["mp4", "mov", "mkv", "avi", "webm", "m4v", "wmv", "flv", "mts", "m2ts", "ts", "mpg", "mpeg", "gif"];

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([560.0, 770.0])
            .with_min_inner_size([460.0, 750.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(SilkApp::new()))
        }),
    )
}

fn setup_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = vec2(10.0, 10.0);
        style.spacing.button_padding = vec2(14.0, 8.0);
        style.visuals.selection.bg_fill = ACCENT;
        style.visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        for w in [
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
            &mut style.visuals.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(8);
        }
        for (text_style, font) in style.text_styles.iter_mut() {
            font.size = match text_style {
                egui::TextStyle::Heading => 26.0,
                egui::TextStyle::Small => 12.0,
                egui::TextStyle::Monospace => 13.0,
                _ => 15.0,
            };
        }
    });
}

struct Input {
    path: PathBuf,
    info: VideoInfo,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Effect {
    Interpolate,
    MotionBlur,
    Both,
}

enum Status {
    Idle,
    Running { fraction: Option<f32>, eta: Option<Duration> },
    Done { output: PathBuf, elapsed: Duration },
    Failed(String),
    Cancelled,
}

struct SilkApp {
    tools: Option<Tools>,
    input: Option<Input>,
    input_error: Option<String>,
    effect: Effect,
    multiplier: u64,
    /// Interpolation method when interpolating alone; motion blur always uses RIFE.
    method: Method,
    /// Motion blur shutter angle in degrees.
    shutter: f32,
    /// Set when the user picked the destination themselves; otherwise it
    /// follows the input name and multiplier.
    custom_output: Option<PathBuf>,
    /// The running encode and the file it writes.
    job: Option<(Job, PathBuf)>,
    status: Status,
    /// Extra information from the last job (GPU fallback, repaired frames…).
    notice: Option<String>,
    /// Whether the RIFE model ships next to the app.
    has_model: bool,
}

impl SilkApp {
    fn new() -> Self {
        let has_model = rife::locate_model().is_some();
        let mut app = Self {
            tools: Tools::locate(),
            input: None,
            input_error: None,
            effect: Effect::Interpolate,
            multiplier: 2,
            method: if has_model { Method::Rife } else { Method::PixelMotion },
            shutter: 180.0,
            custom_output: None,
            job: None,
            status: Status::Idle,
            notice: None,
            has_model,
        };
        // A video dropped onto the executable (or "Open with") arrives as an argument.
        if let Some(path) = std::env::args_os().nth(1) {
            app.load(path.into());
        }
        app
    }

    fn running(&self) -> bool {
        self.job.is_some()
    }

    fn output_multiplier(&self) -> u64 {
        if self.effect == Effect::MotionBlur { 1 } else { self.multiplier }
    }

    fn output_method(&self) -> Method {
        if self.effect == Effect::Interpolate { self.method } else { Method::Rife }
    }

    fn output_shutter(&self) -> Option<f32> {
        (self.effect != Effect::Interpolate).then_some(self.shutter)
    }

    fn load(&mut self, path: PathBuf) {
        self.status = Status::Idle;
        self.custom_output = None;
        self.input = None;
        self.input_error = None;
        let Some(tools) = &self.tools else {
            self.input_error = Some(tr("Locate FFmpeg first.", "Primero hay que localizar FFmpeg.").into());
            return;
        };
        match ffmpeg::probe(tools, &path) {
            Ok(info) => self.input = Some(Input { path, info }),
            Err(e) => self.input_error = Some(trf!("Could not read the video:\n{e}", "No se pudo leer el vídeo:\n{e}")),
        }
    }

    fn output_path(&self) -> Option<PathBuf> {
        if let Some((_, p)) = &self.job {
            return Some(p.clone());
        }
        if let Some(p) = &self.custom_output {
            return Some(p.clone());
        }
        let input = self.input.as_ref()?;
        let stem = input.path.file_stem()?.to_string_lossy();
        let fps = fps_label(input.info.fps.value() * self.multiplier as f64);
        let smooth = tr("smooth", "fluido");
        let base = match self.effect {
            Effect::Interpolate => format!("{stem}_{smooth}_{fps}fps"),
            Effect::MotionBlur => format!("{stem}_blur"),
            Effect::Both => format!("{stem}_{smooth}_{fps}fps_blur"),
        };
        let dir = input.path.parent()?;
        let mut candidate = dir.join(format!("{base}.mp4"));
        let mut n = 2;
        while candidate.exists() {
            candidate = dir.join(format!("{base}_{n}.mp4"));
            n += 1;
        }
        Some(candidate)
    }

    fn start(&mut self, ctx: &egui::Context) {
        let (Some(tools), Some(input), Some(output)) = (&self.tools, &self.input, self.output_path()) else {
            return;
        };
        let req = Request {
            input: input.path.clone(),
            output: output.clone(),
            info: input.info.clone(),
            multiplier: self.output_multiplier(),
            method: self.output_method(),
            shutter: self.output_shutter(),
        };
        let ctx = ctx.clone();
        let job = ffmpeg::start(tools, req, move || ctx.request_repaint());
        self.job = Some((job, output));
        self.status = Status::Running { fraction: Some(0.0), eta: None };
        self.notice = None;
    }

    fn poll_job(&mut self) {
        let Some((job, output)) = &self.job else { return };
        let output = output.clone();
        while let Ok(event) = job.events.try_recv() {
            match event {
                Event::Progress { fraction, eta } => self.status = Status::Running { fraction, eta },
                Event::Notice(msg) => self.notice = Some(msg),
                Event::Done { elapsed } => {
                    self.status = Status::Done { output, elapsed };
                    self.job = None;
                    // The next run must not overwrite this result.
                    self.custom_output = None;
                    break;
                }
                Event::Failed(msg) => {
                    self.status = Status::Failed(msg);
                    self.job = None;
                    break;
                }
                Event::Cancelled => {
                    self.status = Status::Cancelled;
                    self.job = None;
                    break;
                }
            }
        }
    }

    fn pick_input(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title(tr("Choose a video", "Elige un vídeo"))
            .add_filter(tr("Video", "Vídeo"), VIDEO_EXTS)
            .add_filter(tr("All files", "Todos los archivos"), &["*"])
            .pick_file()
        {
            self.load(path);
        }
    }

    fn pick_output(&mut self) {
        let Some(current) = self.output_path() else { return };
        let mut dialog = rfd::FileDialog::new()
            .set_title(tr("Save smooth video as", "Guardar vídeo fluido como"))
            .add_filter("MP4", &["mp4"])
            .add_filter("MKV", &["mkv"])
            .add_filter("MOV", &["mov"]);
        if let Some(dir) = current.parent() {
            dialog = dialog.set_directory(dir);
        }
        if let Some(name) = current.file_name() {
            dialog = dialog.set_file_name(name.to_string_lossy());
        }
        if let Some(mut path) = dialog.save_file() {
            if path.extension().is_none() {
                path.set_extension("mp4");
            }
            self.custom_output = Some(path);
        }
    }

    fn pick_ffmpeg(&mut self) {
        if let Some(file) = rfd::FileDialog::new()
            .set_title(tr(
                "Locate ffmpeg.exe (ffprobe.exe must be in the same folder)",
                "Localiza ffmpeg.exe (ffprobe.exe debe estar en la misma carpeta)",
            ))
            .add_filter("ffmpeg", &["exe"])
            .pick_file()
        {
            self.tools = file.parent().and_then(Tools::in_dir);
        }
    }
}

impl eframe::App for SilkApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_job();
        let ctx = ui.ctx().clone();

        let dropped = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
        if let Some(path) = dropped
            && !self.running()
        {
            self.load(path);
        }

        let panel = egui::Frame::central_panel(ui.style()).inner_margin(24);
        egui::CentralPanel::default().frame(panel).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 12.0;
            ui.heading(RichText::new(APP_NAME).strong());
            ui.label(
                RichText::new(tr(
                    "Silky smooth video: new in-between frames and true motion blur.",
                    "Vídeo sedoso: nuevos fotogramas intermedios y motion blur real.",
                ))
                .weak(),
            );
            ui.add_space(4.0);

            self.drop_zone(ui);
            ui.add_enabled_ui(!self.running(), |ui| {
                self.settings(ui);
            });
            ui.add_space(4.0);
            self.actions(ui, &ctx);
            self.status_area(ui);

            ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| self.footer(ui));
        });
    }
}

impl SilkApp {
    fn drop_zone(&mut self, ui: &mut egui::Ui) {
        let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        let visuals = ui.visuals().clone();
        let stroke = if hovering { Stroke::new(2.0, ACCENT) } else { visuals.widgets.noninteractive.bg_stroke };
        let fill = if hovering { ACCENT.gamma_multiply(0.12) } else { visuals.faint_bg_color };

        let response = egui::Frame::new()
            .fill(fill)
            .stroke(stroke)
            .corner_radius(12)
            .inner_margin(18)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_min_height(96.0);
                ui.vertical_centered(|ui| match &self.input {
                    Some(input) => {
                        let name = input.path.file_name().unwrap_or_default().to_string_lossy();
                        ui.label(RichText::new(name).strong().size(16.0));
                        let info = &input.info;
                        let mut details = format!("{}×{}  ·  {} fps", info.width, info.height, fps_label(info.fps.value()));
                        if let Some(d) = info.duration {
                            details += &format!("  ·  {}", clock(Duration::from_secs_f64(d)));
                        }
                        ui.label(RichText::new(details).weak());
                        ui.label(
                            RichText::new(tr(
                                "Click or drop another video to change it",
                                "Haz clic o arrastra otro vídeo para cambiarlo",
                            ))
                            .small()
                            .weak(),
                        );
                    }
                    None => {
                        ui.add_space(14.0);
                        ui.label(RichText::new(tr("Drop a video here", "Arrastra un vídeo aquí")).strong().size(17.0));
                        ui.label(RichText::new(tr("or click to browse", "o haz clic para elegirlo")).weak());
                        if let Some(err) = &self.input_error {
                            ui.label(RichText::new(err).color(ERR).small());
                        }
                    }
                });
            })
            .response
            .interact(egui::Sense::click());

        if !self.running() {
            let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.clicked() {
                self.pick_input();
            }
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        let gap = ui.spacing().item_spacing.x;
        let third = (ui.available_width() - 2.0 * gap) / 3.0;

        ui.label(RichText::new(tr("Effect", "Efecto")).strong());
        ui.horizontal(|ui| {
            for (effect, title, subtitle) in [
                (Effect::Interpolate, tr("Interpolation", "Interpolación"), tr("More frames", "Más fotogramas")),
                (Effect::MotionBlur, "Motion Blur", tr("RSMB style", "Estilo RSMB")),
                (Effect::Both, tr("Both", "Ambos"), tr("Smoothest", "Máxima fluidez")),
            ] {
                // Motion blur needs RIFE's optical flow.
                let available = effect == Effect::Interpolate || self.has_model;
                if card(ui, vec2(third, 58.0), self.effect == effect, available, title, Some(subtitle)).clicked() {
                    self.effect = effect;
                }
            }
        });

        if self.effect != Effect::MotionBlur {
            ui.label(RichText::new(tr("FPS multiplier", "Multiplicador de FPS")).strong());
            let source_fps = self.input.as_ref().map(|i| i.info.fps.value());
            ui.horizontal(|ui| {
                for m in [2, 3, 4] {
                    let subtitle = source_fps.map(|fps| format!("{} fps", fps_label(fps * m as f64)));
                    let size = vec2(third, if subtitle.is_some() { 54.0 } else { 40.0 });
                    if card(ui, size, self.multiplier == m, true, &format!("{m}×"), subtitle.as_deref()).clicked() {
                        self.multiplier = m;
                    }
                }
            });
        }

        if self.effect == Effect::Interpolate {
            ui.label(RichText::new(tr("Method", "Método")).strong());
            ui.horizontal(|ui| {
                for (method, title, subtitle, available) in [
                    (Method::Rife, tr("AI (RIFE)", "IA (RIFE)"), tr("Best quality · GPU", "Máxima calidad · GPU"), self.has_model),
                    (Method::PixelMotion, "Pixel Motion", tr("Classic optical flow", "Flujo óptico clásico"), true),
                    (Method::FrameMix, "Frame Mix", tr("Blend · very fast", "Mezcla · muy rápido"), true),
                ] {
                    let response = card(ui, vec2(third, 58.0), self.method == method, available, title, Some(subtitle));
                    if response.clicked() {
                        self.method = method;
                    }
                }
            });
        } else {
            ui.horizontal(|ui| {
                ui.label(RichText::new(tr("Blur strength", "Intensidad del blur")).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(shutter_label(self.shutter)).weak());
                });
            });
            ui.scope(|ui| {
                // Leave room for the value box so the row never widens the panel.
                ui.spacing_mut().slider_width = ui.available_width() - 90.0;
                ui.add(egui::Slider::new(&mut self.shutter, 45.0..=360.0).step_by(15.0).fixed_decimals(0).suffix("°"))
                    .on_hover_text(tr(
                        "Shutter angle: how much of each frame's time the motion is accumulated over. \
                         180° is the natural look of a camera; 360° is the maximum.",
                        "Ángulo de obturación: la fracción de cada fotograma durante la que se acumula el movimiento. \
                         180° es el aspecto natural de una cámara; 360° es el máximo.",
                    ));
            });
        }

        if self.input.is_some()
            && let Some(output) = self.output_path()
        {
            ui.horizontal(|ui| {
                ui.label(RichText::new(tr("Save to:", "Guardar en:")).strong());
                let name = output.file_name().unwrap_or_default().to_string_lossy().into_owned();
                ui.add(egui::Label::new(RichText::new(name).weak()).truncate())
                    .on_hover_text(output.display().to_string());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(tr("Change…", "Cambiar…")).clicked() {
                        self.pick_output();
                    }
                });
            });
        }
    }

    fn actions(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let size = vec2(ui.available_width(), 46.0);
        if let Some((job, _)) = &self.job {
            if card(ui, size, false, true, tr("Cancel", "Cancelar"), None).clicked() {
                job.cancel();
            }
            return;
        }
        let ready = self.tools.is_some() && self.input.is_some();
        if card(ui, size, true, ready, tr("Process", "Procesar"), None).clicked() {
            self.start(ctx);
        }
    }

    fn status_area(&mut self, ui: &mut egui::Ui) {
        match &self.status {
            Status::Idle => {
                let hint = match self.output_method() {
                    Method::Rife if self.effect != Effect::Interpolate => Some(tr(
                        "The blur follows each pixel's motion, computed by the AI on the GPU. \
                         HUDs and other fixed elements stay sharp.",
                        "El blur sigue el movimiento de cada píxel, calculado por la IA en la GPU. \
                         El HUD y los elementos fijos se mantienen nítidos.",
                    )),
                    Method::Rife => Some(tr(
                        "The AI generates the frames on the GPU. Very high quality; 1440p or 4K takes a while.",
                        "La IA genera los fotogramas en la GPU. Calidad muy alta; en 1440p o 4K tarda bastante.",
                    )),
                    Method::PixelMotion
                        if self.input.as_ref().is_some_and(|i| i.info.width * i.info.height > 1920 * 1080) =>
                    {
                        Some(tr(
                            "Pixel Motion is slow above 1080p; be patient.",
                            "Pixel Motion es lento en resoluciones por encima de 1080p; ten paciencia.",
                        ))
                    }
                    _ => None,
                };
                if let Some(hint) = hint {
                    ui.label(RichText::new(hint).small().weak());
                }
            }
            Status::Running { fraction, eta } => {
                let bar = match fraction {
                    Some(f) => {
                        let mut text = format!("{:.0} %", f * 100.0);
                        if let Some(eta) = eta {
                            text += &trf!("  ·  {} left", "  ·  quedan {}", clock(*eta));
                        }
                        egui::ProgressBar::new(*f).text(text)
                    }
                    None => {
                        let text = if self.output_method() == Method::Rife {
                            tr("Preparing the AI…", "Preparando la IA…")
                        } else {
                            tr("Processing…", "Procesando…")
                        };
                        egui::ProgressBar::new(0.0).animate(true).text(text)
                    }
                };
                ui.add(bar.desired_height(24.0).fill(ACCENT));
                self.notice_label(ui);
            }
            Status::Done { output, elapsed } => {
                let output = output.clone();
                ui.add(egui::ProgressBar::new(1.0).desired_height(24.0).fill(OK).text("100 %"));
                ui.horizontal(|ui| {
                    let done = trf!("Done in {}", "Listo en {}", clock(*elapsed));
                    ui.label(RichText::new(done).color(OK).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(tr("Show in folder", "Mostrar en carpeta")).clicked() {
                            reveal(&output);
                        }
                        if ui.button(tr("Open video", "Abrir vídeo")).clicked() {
                            open(&output);
                        }
                    });
                });
                self.notice_label(ui);
            }
            Status::Failed(msg) => {
                ui.label(RichText::new(tr("The video could not be processed", "No se pudo procesar el vídeo")).color(ERR).strong());
                ui.label(RichText::new(msg).small().monospace().color(ERR.gamma_multiply(0.85)));
            }
            Status::Cancelled => {
                ui.label(RichText::new(tr("Cancelled.", "Cancelado.")).weak());
            }
        }
    }

    fn notice_label(&self, ui: &mut egui::Ui) {
        if let Some(notice) = &self.notice {
            ui.label(RichText::new(notice).small().weak());
        }
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        if !self.has_model {
            ui.label(
                RichText::new(tr(
                    "models\\rife_v4.25_flow.onnx was not found next to the app: \
                     the AI and motion blur are unavailable.",
                    "No se encontró models\\rife_v4.25_flow.onnx junto a la aplicación: \
                     la IA y el motion blur no están disponibles.",
                ))
                    .small()
                    .color(ERR),
            );
        }
        match &self.tools {
            Some(tools) => {
                ui.label(RichText::new(format!("FFmpeg: {}", tools.ffmpeg.display())).small().weak());
            }
            None => {
                if ui.button(tr("Locate ffmpeg.exe…", "Localizar ffmpeg.exe…")).clicked() {
                    self.pick_ffmpeg();
                }
                ui.label(
                    RichText::new(tr(
                        "FFmpeg was not found. Copy ffmpeg and ffprobe next to the app \
                         (or into an ffmpeg\\bin folder), add them to PATH, or locate them manually.",
                        "No se encontró FFmpeg. Copia ffmpeg y ffprobe junto a la aplicación \
                         (o en una carpeta ffmpeg\\bin), añádelos al PATH o localízalos manualmente.",
                    ))
                    .color(ERR)
                    .small(),
                );
            }
        }
    }
}

/// A clickable rounded tile with a centred title and optional subtitle; used
/// for the option pickers and the main button so they all look alike.
fn card(ui: &mut egui::Ui, size: egui::Vec2, selected: bool, enabled: bool, title: &str, subtitle: Option<&str>) -> egui::Response {
    let parent_enabled = ui.is_enabled();
    let enabled = enabled && parent_enabled;
    let opacity = if !parent_enabled { 0.55 } else if !enabled { 0.4 } else { 1.0 };
    let (rect, response) = ui.allocate_exact_size(size, if enabled { egui::Sense::click() } else { egui::Sense::hover() });
    let response = if enabled { response.on_hover_cursor(egui::CursorIcon::PointingHand) } else { response };
    if !ui.is_rect_visible(rect) {
        return response;
    }

    let visuals = ui.visuals();
    let hovered = enabled && response.hovered();
    let (fill, stroke, text) = if selected {
        let fill = if hovered { ACCENT.lerp_to_gamma(Color32::WHITE, 0.12) } else { ACCENT };
        (fill, Stroke::NONE, Color32::WHITE)
    } else {
        let stroke_color = if hovered { ACCENT.gamma_multiply(0.8) } else { visuals.widgets.noninteractive.bg_stroke.color };
        (visuals.faint_bg_color, Stroke::new(1.0, stroke_color), visuals.text_color())
    };
    let painter = ui.painter();
    painter.rect(rect, 10, fill.gamma_multiply(opacity), stroke, egui::StrokeKind::Inside);
    let center = rect.center();
    let title_font = egui::FontId::proportional(16.5);
    match subtitle {
        Some(sub) => {
            painter.text(center - vec2(0.0, 9.0), egui::Align2::CENTER_CENTER, title, title_font, text.gamma_multiply(opacity));
            let sub_color = text.gamma_multiply(0.72 * opacity);
            painter.text(center + vec2(0.0, 11.0), egui::Align2::CENTER_CENTER, sub, egui::FontId::proportional(12.5), sub_color);
        }
        None => {
            painter.text(center, egui::Align2::CENTER_CENTER, title, title_font, text.gamma_multiply(opacity));
        }
    }
    response
}

fn shutter_label(degrees: f32) -> &'static str {
    match degrees {
        d if d < 120.0 => tr("Subtle", "Sutil"),
        d if d < 240.0 => tr("Natural (film camera)", "Natural (cámara de cine)"),
        d if d < 330.0 => tr("Strong", "Intenso"),
        _ => tr("Maximum", "Máximo"),
    }
}

fn fps_label(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.005 {
        format!("{}", fps.round() as i64)
    } else {
        format!("{fps:.2}")
    }
}

fn clock(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Opens the file with the system's default player.
fn open(path: &Path) {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program).arg(path).spawn();
}

/// Shows the file selected in the system file manager.
fn reveal(path: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn();
    }
    #[cfg(not(windows))]
    if let Some(dir) = path.parent() {
        open(dir);
    }
}
