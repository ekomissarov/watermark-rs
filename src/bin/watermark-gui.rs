//! Desktop window for the watermark tool (egui). The console tool stays as it is.
//!
//! Flow: open an image -> type the label -> "Process" (embed + self-check, the
//! modified blocks are highlighted on the preview) -> "Save…".
//!
//! Build & run:  cargo run --release --features gui --bin watermark-gui

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use eframe::egui::{
    self, pos2, vec2, Align2, Color32, ColorImage, FontId, Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2,
};
use watermark_rs::{
    detect, embed, encode_image, extract_with_cancel, layout_for, load, luma, output_format, parse_seed, BLOCK,
    DEFAULT_SEED, MAX_MESSAGE_BYTES, TILE,
};

/// Largest texture side uploaded to the GPU; bigger images are box-downscaled for the preview only.
const MAX_TEXTURE_SIDE: usize = 4096;
const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 64.0;

// --------------------------------------------------------------------------- //
// Background jobs
// --------------------------------------------------------------------------- //
enum Msg {
    Loaded { path: PathBuf, rgb: Vec<u8>, w: usize, h: usize },
    Processed { message: String, seed: u64, rgb: Vec<u8>, modified: Vec<(usize, usize)>, verified: bool },
    Saved { path: PathBuf, bytes: usize },
    Detected { run: u64, cancelled: bool, found: Result<Option<String>, String> },
    Cancelled,
    Failed(String),
}

// --------------------------------------------------------------------------- //
// State
// --------------------------------------------------------------------------- //
struct Marked {
    message: String,
    seed: u64,
    rgb: Arc<Vec<u8>>,
    tex: TextureHandle,
    modified: Vec<(usize, usize)>, // (block row, block column)
    verified: bool,
}

#[derive(Clone)]
enum Detect {
    Idle,
    Running,
    Cancelled,
    Found(String),
    NotFound,
    Error(String),
}

struct Doc {
    detect: Detect,
    detect_run: u64, // token of the latest search, stale results are ignored
    cancel: Arc<AtomicBool>,
    path: PathBuf,
    rgb: Arc<Vec<u8>>,
    w: usize,
    h: usize,
    tex: TextureHandle,
    marked: Option<Marked>,
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    Original,
    Marked,
}

struct WatermarkApp {
    next_id: u64,
    doc: Option<Doc>,
    message: String,
    seed_text: String, // hexadecimal layout seed, editable
    view: View,
    show_regions: bool,
    zoom: f32,
    offset: Vec2, // screen position of the image's top-left corner, relative to the preview rect
    fit_pending: bool,
    busy: Option<String>,
    status: String,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl WatermarkApp {
    fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            next_id: 0,
            doc: None,
            message: String::new(),
            seed_text: format!("{DEFAULT_SEED:X}"),
            view: View::Original,
            show_regions: true,
            zoom: 1.0,
            offset: Vec2::ZERO,
            fit_pending: true,
            busy: None,
            status: "Open an image (PNG or JPEG) or drag a file into the window.".into(),
            tx,
            rx,
        }
    }

    // ----- jobs -------------------------------------------------------------
    fn start_load(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.busy = Some("Loading image…".into());
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        thread::spawn(move || {
            let msg = match load(&path) {
                Ok((rgb, w, h)) => Msg::Loaded { path, rgb, w, h },
                Err(e) => Msg::Failed(format!("Could not open the file: {e}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// The file dialog runs in a worker thread: blocking the UI thread makes the desktop
    /// report "not responding".
    fn start_open(&mut self, ctx: &egui::Context) {
        self.busy = Some("Choose a file…".into());
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        thread::spawn(move || {
            let picked = rfd::FileDialog::new().add_filter("Images", &["png", "jpg", "jpeg"]).pick_file();
            let msg = match picked {
                None => Msg::Cancelled,
                Some(path) => match load(&path) {
                    Ok((rgb, w, h)) => Msg::Loaded { path, rgb, w, h },
                    Err(e) => Msg::Failed(format!("Could not open the file: {e}")),
                },
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Look for an already embedded watermark in the opened file (full search, background
    /// thread, started only on request because it can take long and keeps the CPU busy).
    fn start_detect(&mut self, ctx: &egui::Context) {
        self.next_id += 1;
        let run = self.next_id;
        let Some(doc) = &mut self.doc else { return };
        let seed = match parse_seed(&self.seed_text) {
            Ok(seed) => seed,
            Err(e) => {
                doc.detect = Detect::Error(format!("invalid seed: {e}"));
                return;
            }
        };
        doc.cancel.store(true, Ordering::Relaxed); // stop a previous search of this file, if any
        let cancel = Arc::new(AtomicBool::new(false));
        doc.cancel = cancel.clone();
        doc.detect = Detect::Running;
        doc.detect_run = run;
        let (rgb, w, h) = (doc.rgb.clone(), doc.w, doc.h);
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        thread::spawn(move || {
            let found = extract_with_cancel(&rgb, w, h, seed, cancel.clone()).map_err(|e| e.to_string());
            let cancelled = cancel.load(Ordering::Relaxed);
            let _ = tx.send(Msg::Detected { run, cancelled, found });
            ctx.request_repaint();
        });
    }

    fn start_process(&mut self, ctx: &egui::Context) {
        let Some(doc) = &self.doc else { return };
        let (rgb, w, h, message) = (doc.rgb.clone(), doc.w, doc.h, self.message.clone());
        let seed = match parse_seed(&self.seed_text) {
            Ok(seed) => seed,
            Err(e) => {
                self.status = format!("Invalid seed: {e}");
                return;
            }
        };
        self.busy = Some("Embedding the watermark and verifying it can be read back…".into());
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        thread::spawn(move || {
            let msg = match embed(&rgb, w, h, &message, seed) {
                Ok(emb) => {
                    let found = detect(&luma(&emb.rgb, w, h), true, &layout_for(seed)).0;
                    let verified = found.as_deref() == Some(message.as_str());
                    Msg::Processed { message, seed, rgb: emb.rgb, modified: emb.modified, verified }
                }
                Err(e) => Msg::Failed(format!("Processing failed: {e}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    fn start_save(&mut self, ctx: &egui::Context) {
        let Some(doc) = &self.doc else { return };
        let Some(marked) = &doc.marked else { return };
        let stem = doc.path.file_stem().and_then(|s| s.to_str()).unwrap_or("image").to_string();
        let dir = doc.path.parent().map(|p| p.to_path_buf());
        let (rgb, w, h, message, seed) = (marked.rgb.clone(), doc.w, doc.h, marked.message.clone(), marked.seed);

        self.busy = Some("Choose where to save…".into());
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new()
                .set_file_name(format!("{stem}_wm.png"))
                .add_filter("PNG (lossless, recommended)", &["png"])
                .add_filter("JPEG", &["jpg", "jpeg"]);
            if let Some(dir) = dir {
                dialog = dialog.set_directory(dir);
            }
            let Some(mut path) = dialog.save_file() else {
                let _ = tx.send(Msg::Cancelled);
                ctx.request_repaint();
                return;
            };
            if path.extension().is_none() {
                path.set_extension("png");
            }
            let result: Result<usize, String> = (|| {
                let fmt = output_format(&path).map_err(|e| e.to_string())?;
                let encoded = encode_image(&rgb, w, h, fmt).map_err(|e| e.to_string())?;
                // Verify the exact bytes that will be written.
                let checked = image::load_from_memory(&encoded).map_err(|e| e.to_string())?.to_rgb8();
                let (cw, ch) = (checked.width() as usize, checked.height() as usize);
                let found = detect(&luma(checked.as_raw(), cw, ch), true, &layout_for(seed)).0;
                if found.as_deref() != Some(message.as_str()) {
                    return Err("The watermark cannot be read back from a file in the chosen format (try PNG instead of JPEG). The file was not saved.".into());
                }
                std::fs::write(&path, &encoded).map_err(|e| e.to_string())?;
                Ok(encoded.len())
            })();
            let msg = match result {
                Ok(bytes) => Msg::Saved { path, bytes },
                Err(e) => Msg::Failed(format!("Saving failed: {e}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            if !matches!(msg, Msg::Detected { .. }) {
                self.busy = None;
            }
            match msg {
                Msg::Loaded { path, rgb, w, h } => {
                    let tex = make_texture(ctx, "original", &rgb, w, h);
                    self.status = format!("{}: {w}×{h} px", path.display());
                    if let Some(old) = &self.doc {
                        old.cancel.store(true, Ordering::Relaxed); // do not keep searching in a closed image
                    }
                    self.doc = Some(Doc {
                        detect: Detect::Idle,
                        detect_run: 0,
                        cancel: Arc::new(AtomicBool::new(false)),
                        path,
                        rgb: Arc::new(rgb),
                        w,
                        h,
                        tex,
                        marked: None,
                    });
                    self.view = View::Original;
                    self.fit_pending = true;
                }
                Msg::Processed { message, seed, rgb, modified, verified } => {
                    if let Some(doc) = &mut self.doc {
                        let total = (doc.h / BLOCK) * (doc.w / BLOCK);
                        // Each cell of the 20x20 pattern needs at least one modified block, otherwise
                        // its bit is read as noise. Low coverage is the usual reason for a failed check.
                        let mut cells = vec![false; TILE * TILE];
                        for &(by, bx) in &modified {
                            cells[(by % TILE) * TILE + bx % TILE] = true;
                        }
                        let covered = cells.iter().filter(|&&c| c).count();
                        self.status = format!(
                            "Modified blocks: {} of {} ({:.1}%). Pattern cells covered: {}/{}. {}",
                            modified.len(),
                            total,
                            100.0 * modified.len() as f32 / total.max(1) as f32,
                            covered,
                            TILE * TILE,
                            if verified {
                                "Read-back check passed."
                            } else {
                                "WARNING: the watermark could not be read back (too few textured areas or the image is too small; use a larger image with texture across the whole frame)."
                            }
                        );
                        let tex = make_texture(ctx, "marked", &rgb, doc.w, doc.h);
                        doc.marked = Some(Marked { message, seed, rgb: Arc::new(rgb), tex, modified, verified });
                        self.view = View::Marked;
                        self.show_regions = true;
                    }
                }
                Msg::Saved { path, bytes } => {
                    self.status = format!("Saved: {} ({:.1} MB)", path.display(), bytes as f64 / 1e6);
                }
                Msg::Detected { run, cancelled, found } => {
                    if let Some(doc) = &mut self.doc {
                        if doc.detect_run == run {
                            doc.detect = match found {
                                Ok(Some(text)) => Detect::Found(text),
                                Ok(None) if cancelled => Detect::Cancelled,
                                Ok(None) => Detect::NotFound,
                                Err(e) => Detect::Error(e),
                            };
                        }
                    }
                }
                Msg::Cancelled => self.status = "Cancelled.".into(),
                Msg::Failed(e) => self.status = e,
            }
        }
    }

    // ----- zoom / pan -------------------------------------------------------
    fn zoom_at(&mut self, anchor: Vec2, factor: f32) {
        let new_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let image_point = (anchor - self.offset) / self.zoom;
        self.zoom = new_zoom;
        self.offset = anchor - image_point * new_zoom;
    }

    fn fit(&mut self, viewport: Vec2) {
        let Some(doc) = &self.doc else { return };
        let zoom = (viewport.x / doc.w as f32).min(viewport.y / doc.h as f32) * 0.98;
        self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.offset = (viewport - vec2(doc.w as f32, doc.h as f32) * self.zoom) / 2.0;
    }

    fn actual_size(&mut self, viewport: Vec2) {
        let Some(doc) = &self.doc else { return };
        self.zoom = 1.0;
        self.offset = (viewport - vec2(doc.w as f32, doc.h as f32)) / 2.0;
    }

    // ----- UI ---------------------------------------------------------------
    fn top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let busy = self.busy.is_some();
        let bytes = self.message.len();
        let valid = (1..=MAX_MESSAGE_BYTES).contains(&bytes);
        let seed = parse_seed(&self.seed_text);
        let seed_valid = seed.is_ok();
        let has_doc = self.doc.is_some();
        let can_save = self
            .doc
            .as_ref()
            .and_then(|d| d.marked.as_ref())
            .map_or(false, |m| m.verified && m.message == self.message && seed.as_ref().ok() == Some(&m.seed));

        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(!busy, egui::Button::new("📂 Open…")).clicked() {
                self.start_open(ctx);
            }
            ui.separator();
            ui.label("Watermark:");
            ui.add(egui::TextEdit::singleline(&mut self.message).desired_width(220.0).hint_text("up to 16 bytes of UTF-8"));
            let counter = format!("{bytes}/{MAX_MESSAGE_BYTES} bytes");
            if valid || bytes == 0 {
                ui.weak(counter);
            } else {
                ui.colored_label(Color32::from_rgb(220, 60, 60), counter);
            }
            ui.label("Seed (hex):");
            let seed_color = if seed_valid { ui.visuals().text_color() } else { Color32::from_rgb(220, 60, 60) };
            ui.add(egui::TextEdit::singleline(&mut self.seed_text).desired_width(130.0).text_color(seed_color).hint_text("1-16 hex digits"))
                .on_hover_text("Selects the watermark layout. The same seed is needed to read the watermark. It is a layout parameter, not a secret key.");
            if ui.small_button("Default").on_hover_text("Restore the default seed").clicked() {
                self.seed_text = format!("{DEFAULT_SEED:X}");
            }
            if ui.add_enabled(!busy && has_doc && valid && seed_valid, egui::Button::new("⚙ Process")).clicked() {
                self.start_process(ctx);
            }
            if ui.add_enabled(!busy && can_save, egui::Button::new("💾 Save…")).clicked() {
                self.start_save(ctx);
            }
            if busy {
                ui.spinner();
            }
        });
    }

    fn detect_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(state) = self.doc.as_ref().map(|d| d.detect.clone()) else { return };
        ui.horizontal(|ui| match state {
            Detect::Idle | Detect::Cancelled => {
                if ui.button("🔍 Find watermark in file").clicked() {
                    self.start_detect(ctx);
                }
                if matches!(state, Detect::Cancelled) {
                    ui.weak("Search stopped.");
                } else {
                    ui.weak("Searching a file that has no watermark can take up to a couple of minutes and keeps the CPU busy.");
                }
            }
            Detect::Running => {
                ui.spinner();
                ui.label("Searching for an embedded watermark…");
                if ui.button("Stop").clicked() {
                    if let Some(doc) = &self.doc {
                        doc.cancel.store(true, Ordering::Relaxed);
                    }
                }
            }
            Detect::Found(text) => {
                ui.colored_label(Color32::from_rgb(40, 160, 80), format!("🔍 Watermark found in file: \"{text}\""));
                if ui.small_button("Copy to field").clicked() {
                    self.message = text;
                }
            }
            Detect::NotFound => {
                ui.weak("🔍 No watermark found with the seed used for the search.");
                if ui.small_button("Search again").clicked() {
                    self.start_detect(ctx);
                }
            }
            Detect::Error(e) => {
                ui.colored_label(Color32::from_rgb(220, 60, 60), format!("Watermark search failed: {e}"));
                if ui.small_button("Search again").clicked() {
                    self.start_detect(ctx);
                }
            }
        });
    }

    fn preview_toolbar(&mut self, ui: &mut egui::Ui, viewport: Vec2) {
        let has_marked = self.doc.as_ref().map_or(false, |d| d.marked.is_some());
        ui.horizontal(|ui| {
            if ui.button("➖").on_hover_text("Zoom out").clicked() {
                self.zoom_at(viewport / 2.0, 1.0 / 1.25);
            }
            if ui.button("➕").on_hover_text("Zoom in").clicked() {
                self.zoom_at(viewport / 2.0, 1.25);
            }
            if ui.button("Fit").clicked() {
                self.fit(viewport);
            }
            if ui.button("100%").clicked() {
                self.actual_size(viewport);
            }
            ui.weak(format!("{:.0}%", self.zoom * 100.0));
            ui.separator();
            ui.add_enabled_ui(has_marked, |ui| {
                ui.selectable_value(&mut self.view, View::Original, "Original");
                ui.selectable_value(&mut self.view, View::Marked, "Watermarked");
                ui.checkbox(&mut self.show_regions, "Show embedded regions");
            });
        });
    }

    fn preview(&mut self, ui: &mut egui::Ui) {
        let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        let rect = resp.rect;
        painter.rect_filled(rect, 0.0, Color32::from_gray(24));

        let Some((w, h)) = self.doc.as_ref().map(|d| (d.w, d.h)) else {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Open an image or drag a file into the window",
                FontId::proportional(18.0),
                Color32::GRAY,
            );
            return;
        };

        // Interaction: wheel / pinch zooms around the cursor, drag pans, double click fits.
        if self.fit_pending && rect.width() > 1.0 {
            self.fit(rect.size());
            self.fit_pending = false;
        }
        if resp.dragged() {
            self.offset += resp.drag_delta();
        }
        if resp.double_clicked() {
            self.fit(rect.size());
        }
        if resp.hovered() {
            let (scroll, pinch, hover) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta(), i.pointer.hover_pos()));
            let factor = pinch * (scroll / 240.0).exp();
            if (factor - 1.0).abs() > 1e-4 {
                if let Some(p) = hover {
                    self.zoom_at(p - rect.min, factor);
                }
            }
        }

        // Drawing
        let doc = self.doc.as_ref().expect("checked above");
        let tex = match (&self.view, &doc.marked) {
            (View::Marked, Some(m)) => &m.tex,
            _ => &doc.tex,
        };
        let img_min = rect.min + self.offset;
        let img_rect = Rect::from_min_size(img_min, vec2(w as f32, h as f32) * self.zoom);
        painter.image(tex.id(), img_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);

        if self.show_regions {
            if let Some(m) = &doc.marked {
                let block_px = BLOCK as f32 * self.zoom;
                let red = Color32::from_rgb(255, 0, 0);
                let tint = Color32::from_rgba_unmultiplied(255, 0, 0, if block_px < 3.0 { 150 } else { 45 });
                let stroke = Stroke::new(1.0_f32, red);
                for &(by, bx) in &m.modified {
                    let min = img_min + vec2(bx as f32, by as f32) * block_px;
                    let r = Rect::from_min_size(min, vec2(block_px, block_px));
                    if !r.intersects(rect) {
                        continue;
                    }
                    if block_px < 3.0 {
                        painter.rect_filled(r, 0.0, tint);
                    } else {
                        painter.rect_filled(r, 0.0, tint);
                        let (a, b, c, d) = (r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom());
                        painter.line_segment([a, b], stroke);
                        painter.line_segment([b, c], stroke);
                        painter.line_segment([c, d], stroke);
                        painter.line_segment([d, a], stroke);
                    }
                }
            }
        }
    }
}

impl eframe::App for WatermarkApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);

        // Drag & drop an image file onto the window.
        if self.busy.is_none() {
            let dropped = ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()));
            if let Some(path) = dropped {
                self.start_load(ctx, path);
            }
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui, ctx);
            self.detect_row(ui, ctx);
            ui.add_space(4.0);
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(b) = &self.busy {
                    ui.spinner();
                    ui.label(b);
                } else {
                    ui.label(&self.status);
                }
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            let viewport = ui.available_size() - vec2(0.0, 32.0);
            self.preview_toolbar(ui, viewport);
            self.preview(ui);
        });
    }
}

// --------------------------------------------------------------------------- //
// Helpers
// --------------------------------------------------------------------------- //
fn make_texture(ctx: &egui::Context, name: &str, rgb: &[u8], w: usize, h: usize) -> TextureHandle {
    let f = ((w.max(h) + MAX_TEXTURE_SIDE - 1) / MAX_TEXTURE_SIDE).max(1);
    let image = if f == 1 {
        ColorImage::from_rgb([w, h], rgb)
    } else {
        let (tw, th) = ((w / f).max(1), (h / f).max(1));
        let mut out = Vec::with_capacity(tw * th * 3);
        for ty in 0..th {
            for tx in 0..tw {
                let mut sum = [0u32; 3];
                let mut n = 0u32;
                for y in ty * f..((ty + 1) * f).min(h) {
                    for x in tx * f..((tx + 1) * f).min(w) {
                        let o = (y * w + x) * 3;
                        for c in 0..3 {
                            sum[c] += rgb[o + c] as u32;
                        }
                        n += 1;
                    }
                }
                for c in 0..3 {
                    out.push((sum[c] / n.max(1)) as u8);
                }
            }
        }
        ColorImage::from_rgb([tw, th], &out)
    };
    // Smooth when zoomed out, crisp pixels when zoomed in.
    let options = TextureOptions { magnification: egui::TextureFilter::Nearest, ..TextureOptions::LINEAR };
    ctx.load_texture(name, image, options)
}

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([640.0, 420.0])
            .with_title("Watermark"),
        ..Default::default()
    };
    eframe::run_native("Watermark", options, Box::new(|_cc| Ok(Box::new(WatermarkApp::new()))))
}
