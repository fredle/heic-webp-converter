//! The window: a queue of images, the output options, and the worker threads
//! that convert them. The footer with the auto-update toggle and version is
//! required on every factory app.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::convert::{Codec, ConvertError, Format, Options};
use crate::license;

const WIDTH: f32 = 680.0;
const HEIGHT: f32 = 560.0;
const APP_NAME: &str = "HEIC & WebP Converter";

pub fn run(args: Vec<String>) -> eframe::Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_inner_size([WIDTH, HEIGHT])
        .with_min_inner_size([520.0, 420.0])
        .with_drag_and_drop(true);
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |_cc| Ok(Box::new(App::new(args)))),
    )
}

#[derive(Clone, Debug, PartialEq)]
enum Status {
    Waiting,
    Converting,
    Done(PathBuf),
    Failed(ConvertError),
}

struct Item {
    path: PathBuf,
    status: Status,
}

type JobResult = (usize, Result<PathBuf, ConvertError>);

pub struct App {
    settings: crate::settings::Settings,
    version: String,
    items: Vec<Item>,
    results: Option<Receiver<JobResult>>,
    in_flight: usize,
    /// Folder of the most recent successful conversion, for "Open folder".
    last_output_dir: Option<PathBuf>,
    licensed_to: Option<String>,
    unlock_open: bool,
    key_entry: String,
    key_message: Option<String>,
}

impl App {
    fn new(args: Vec<String>) -> Self {
        let settings = crate::settings::load();
        let licensed_to = license::verify(&settings.license_key);
        let mut app = Self {
            settings,
            version: crate::update::current_version(),
            items: Vec::new(),
            results: None,
            in_flight: 0,
            last_output_dir: None,
            licensed_to,
            unlock_open: false,
            key_entry: String::new(),
            key_message: None,
        };
        // Files passed on the command line ("Open with", or dropped on the exe).
        let paths: Vec<PathBuf> = args.into_iter().map(PathBuf::from).collect();
        app.add_paths(&paths);
        app
    }

    fn save(&self) {
        crate::settings::save(&self.settings);
    }

    fn busy(&self) -> bool {
        self.in_flight > 0
    }

    fn add_paths(&mut self, paths: &[PathBuf]) {
        let existing: Vec<PathBuf> = self.items.iter().map(|i| i.path.clone()).collect();
        for path in crate::convert::collect_inputs(paths, &existing) {
            self.items.push(Item {
                path,
                status: Status::Waiting,
            });
        }
    }

    fn options(&self) -> Options {
        Options {
            format: self.settings.format,
            quality: self.settings.quality,
            max_edge: self.settings.resize.then_some(self.settings.max_edge),
            out_dir: self.settings.out_dir.clone(),
        }
    }

    fn waiting(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.status == Status::Waiting)
            .count()
    }

    fn start(&mut self, ctx: &egui::Context) {
        let allowance = license::run_allowance(self.waiting(), self.licensed_to.is_some());
        let mut jobs = VecDeque::new();
        for (index, item) in self.items.iter_mut().enumerate() {
            if jobs.len() == allowance {
                break;
            }
            if item.status == Status::Waiting {
                item.status = Status::Converting;
                jobs.push_back((index, item.path.clone()));
            }
        }
        if jobs.is_empty() {
            return;
        }
        self.in_flight = jobs.len();
        let (sender, receiver) = channel();
        self.results = Some(receiver);
        spawn_workers(jobs, self.options(), sender, ctx.clone());
    }

    fn collect_results(&mut self) {
        let Some(receiver) = &self.results else {
            return;
        };
        while let Ok((index, result)) = receiver.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            if let Some(item) = self.items.get_mut(index) {
                item.status = match result {
                    Ok(path) => {
                        self.last_output_dir = path.parent().map(Path::to_path_buf);
                        Status::Done(path)
                    }
                    Err(error) => Status::Failed(error),
                };
            }
        }
        if self.in_flight == 0 {
            self.results = None;
        }
    }

    fn retry_failed(&mut self) {
        for item in &mut self.items {
            if matches!(item.status, Status::Failed(_)) {
                item.status = Status::Waiting;
            }
        }
    }

    fn missing_codecs(&self) -> Vec<Codec> {
        let mut codecs = Vec::new();
        for item in &self.items {
            if let Status::Failed(ConvertError::MissingCodec(codec)) = item.status
                && !codecs.contains(&codec)
            {
                codecs.push(codec);
            }
        }
        codecs
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let idle = !self.busy();
            if ui
                .add_enabled(idle, egui::Button::new("Add photos…"))
                .clicked()
                && let Some(files) = rfd::FileDialog::new()
                    .add_filter(
                        "Images",
                        &[
                            "heic", "heif", "hif", "avif", "jxl", "webp", "jpg", "jpeg", "jfif",
                            "png", "bmp", "tif", "tiff", "gif",
                        ],
                    )
                    .add_filter("All files", &["*"])
                    .pick_files()
            {
                self.add_paths(&files);
            }
            if ui
                .add_enabled(idle, egui::Button::new("Add folder…"))
                .clicked()
                && let Some(folder) = rfd::FileDialog::new().pick_folder()
            {
                self.add_paths(&[folder]);
            }
            if ui
                .add_enabled(idle && !self.items.is_empty(), egui::Button::new("Clear"))
                .clicked()
            {
                self.items.clear();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(email) = &self.licensed_to {
                    ui.weak(format!("Unlocked for {email}"));
                } else if license::paid_tier_enabled() {
                    if ui.button("Unlock").clicked() {
                        self.unlock_open = true;
                    }
                    ui.weak(format!("Free: {} photos per run", license::FREE_BATCH));
                }
            });
        });
    }

    fn list(&mut self, ui: &mut egui::Ui) {
        if self.items.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new(
                        "Drop photos or folders here\n\nHEIC · WebP · AVIF · JXL · JPG · PNG · BMP · TIFF · GIF",
                    )
                    .size(16.0)
                    .weak(),
                );
            });
            return;
        }
        let mut remove = None;
        let idle = !self.busy();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (index, item) in self.items.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let name = item
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add_enabled(idle, egui::Button::new("✖").frame(false))
                                .on_hover_text("Remove from the list")
                                .clicked()
                            {
                                remove = Some(index);
                            }
                            let (text, colour) = status_text(&item.status, ui.visuals());
                            ui.colored_label(colour, text);
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.add(egui::Label::new(name).truncate())
                                    .on_hover_text(item.path.display().to_string());
                            });
                        });
                    });
                }
            });
        if let Some(index) = remove {
            self.items.remove(index);
        }
    }

    fn codec_help(&mut self, ui: &mut egui::Ui) {
        let codecs = self.missing_codecs();
        if codecs.is_empty() {
            return;
        }
        ui.group(|ui| {
            for codec in codecs {
                ui.label(codec.explanation());
                ui.horizontal(|ui| {
                    for (label, product) in codec.store_products() {
                        if ui.button(*label).clicked() {
                            open(&format!("ms-windows-store://pdp/?ProductId={product}"));
                        }
                    }
                });
            }
            ui.horizontal(|ui| {
                ui.weak("Install, then");
                if ui
                    .add_enabled(!self.busy(), egui::Button::new("Try again"))
                    .clicked()
                {
                    self.retry_failed();
                }
            });
        });
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let before = (
            self.settings.format,
            self.settings.quality,
            self.settings.resize,
            self.settings.max_edge,
        );
        ui.add_enabled_ui(!self.busy(), |ui| {
            ui.horizontal(|ui| {
                ui.label("Convert to");
                egui::ComboBox::from_id_salt("format")
                    .selected_text(self.settings.format.label())
                    .show_ui(ui, |ui| {
                        for format in Format::ALL {
                            ui.selectable_value(&mut self.settings.format, format, format.label());
                        }
                    });
                if self.settings.format == Format::Jpg {
                    ui.add(egui::Slider::new(&mut self.settings.quality, 40..=100).text("quality"));
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.settings.resize, "Shrink so the longest side is at most");
                ui.add_enabled(
                    self.settings.resize,
                    egui::DragValue::new(&mut self.settings.max_edge)
                        .range(64..=20000)
                        .speed(8)
                        .suffix(" px"),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Save to");
                let same = self.settings.out_dir.is_none();
                if ui.radio(same, "the same folder as the original").clicked() && !same {
                    self.settings.out_dir = None;
                    self.save();
                }
                let label = match &self.settings.out_dir {
                    Some(dir) => dir.display().to_string(),
                    None => "another folder…".to_owned(),
                };
                if ui.radio(!same, label).clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    self.settings.out_dir = Some(dir);
                    self.save();
                }
            });
        });
        let after = (
            self.settings.format,
            self.settings.quality,
            self.settings.resize,
            self.settings.max_edge,
        );
        if before != after {
            self.save();
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let waiting = self.waiting();
            let allowance = license::run_allowance(waiting, self.licensed_to.is_some());
            let label = if self.busy() {
                format!("Converting… {} left", self.in_flight)
            } else if allowance < waiting {
                format!("Convert next {allowance} of {waiting}")
            } else if waiting == 0 {
                "Convert".to_owned()
            } else if waiting == 1 {
                "Convert 1 photo".to_owned()
            } else {
                format!("Convert {waiting} photos")
            };
            let button = egui::Button::new(egui::RichText::new(label).size(16.0))
                .min_size(egui::vec2(220.0, 34.0));
            if ui
                .add_enabled(!self.busy() && waiting > 0, button)
                .clicked()
            {
                self.start(ui.ctx());
            }
            if self.busy() {
                ui.spinner();
            }
            if let Some(dir) = &self.last_output_dir
                && ui.button("Open folder").clicked()
            {
                open(&dir.display().to_string());
            }
            let done = self
                .items
                .iter()
                .filter(|i| matches!(i.status, Status::Done(_)))
                .count();
            if done > 0 {
                ui.weak(format!("{done} converted"));
            }
        });
    }

    fn unlock_window(&mut self, ctx: &egui::Context) {
        if !self.unlock_open {
            return;
        }
        let mut open_flag = true;
        egui::Window::new("Unlock unlimited batches")
            .open(&mut open_flag)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(format!(
                    "Everything works for free, {} photos at a time. A one-off unlock converts any number in one go. No subscription.",
                    license::FREE_BATCH
                ));
                ui.add_space(6.0);
                if ui.button("Buy the unlock…").clicked() {
                    open(license::BUY_URL);
                }
                ui.add_space(10.0);
                ui.label("Already bought it? Paste your licence key:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.key_entry)
                        .hint_text("HWC1.…")
                        .desired_width(f32::INFINITY),
                );
                if ui.button("Activate").clicked() {
                    match license::verify(&self.key_entry) {
                        Some(email) => {
                            self.settings.license_key = self.key_entry.trim().to_owned();
                            self.save();
                            self.key_message = Some(format!("Unlocked for {email}. Thank you!"));
                            self.licensed_to = Some(email);
                        }
                        None => {
                            self.key_message = Some(
                                "That key isn't valid. Copy the whole key, starting with HWC1."
                                    .to_owned(),
                            );
                        }
                    }
                }
                if let Some(message) = &self.key_message {
                    ui.label(message);
                }
            });
        if !open_flag {
            self.unlock_open = false;
            self.key_message = None;
        }
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let mut auto_update = self.settings.auto_update;
            if ui
                .checkbox(&mut auto_update, "Automatically update")
                .on_hover_text("Checks for new versions about once a day and installs them in the background.")
                .changed()
            {
                self.settings.auto_update = auto_update;
                crate::settings::save(&self.settings);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(format!("v{}", self.version));
            });
        });
    }
}

fn status_text(status: &Status, visuals: &egui::Visuals) -> (String, egui::Color32) {
    match status {
        Status::Waiting => ("Waiting".to_owned(), visuals.weak_text_color()),
        Status::Converting => ("Converting…".to_owned(), visuals.text_color()),
        Status::Done(path) => (
            format!(
                "✔ {}",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            egui::Color32::from_rgb(46, 160, 67),
        ),
        Status::Failed(ConvertError::MissingCodec(_)) => (
            "Needs a Windows extension".to_owned(),
            visuals.warn_fg_color,
        ),
        Status::Failed(ConvertError::Other(message)) => {
            (short_error(message), visuals.error_fg_color)
        }
    }
}

/// One line, short enough to sit beside the file name.
fn short_error(message: &str) -> String {
    let line = message.lines().next().unwrap_or("").trim();
    let text = if line.is_empty() {
        "Couldn't read this file"
    } else {
        line
    };
    let mut short: String = text.chars().take(60).collect();
    if text.chars().count() > 60 {
        short.push('…');
    }
    short
}

fn spawn_workers(
    jobs: VecDeque<(usize, PathBuf)>,
    options: Options,
    sender: Sender<JobResult>,
    ctx: egui::Context,
) {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 8)
        .min(jobs.len());
    let jobs = Arc::new(Mutex::new(jobs));
    for _ in 0..threads {
        let (jobs, options, sender, ctx) =
            (jobs.clone(), options.clone(), sender.clone(), ctx.clone());
        std::thread::spawn(move || {
            loop {
                let next = jobs.lock().ok().and_then(|mut queue| queue.pop_front());
                let Some((index, path)) = next else { break };
                // A decoder panic must not leave the row stuck on "Converting".
                let result = std::panic::catch_unwind(|| crate::convert::convert_file(&path, &options))
                    .unwrap_or_else(|_| {
                        Err(ConvertError::Other("Couldn't read this file".to_owned()))
                    });
                if sender.send((index, result)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
    }
}

/// Opens a folder, web page or Store page with whatever Windows uses for it.
fn open(target: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, w};
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &HSTRING::from(target),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui_root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.collect_results();
        if !self.busy() {
            let dropped: Vec<PathBuf> = ui_root.ctx().input(|input| {
                input
                    .raw
                    .dropped_files
                    .iter()
                    .map(|file| file.path().to_path_buf())
                    .collect()
            });
            if !dropped.is_empty() {
                self.add_paths(&dropped);
            }
        }

        egui::Panel::bottom("footer").show(ui_root, |ui| self.footer(ui));
        egui::Panel::bottom("controls").show(ui_root, |ui| {
            ui.add_space(6.0);
            self.codec_help(ui);
            self.controls(ui);
            ui.add_space(6.0);
        });
        egui::Panel::top("toolbar").show(ui_root, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(ui_root, |ui| self.list(ui));
        let ctx = ui_root.ctx().clone();
        self.unlock_window(&ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_error_is_one_trimmed_line() {
        assert_eq!(short_error("  bad header\nmore"), "bad header");
        assert_eq!(short_error(""), "Couldn't read this file");
        let long = "x".repeat(100);
        assert_eq!(short_error(&long).chars().count(), 61);
    }
}
