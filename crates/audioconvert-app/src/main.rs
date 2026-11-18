//! AudioConvert desktop app (egui). Add audio files or a folder,
//! pick a target format, press Convert — the bundled ffmpeg turns
//! every file into the target format, tags and cover art included.
//! Conversions run on a worker thread; the list stays responsive.

// On Windows, run as a GUI app: no console window behind the UI.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use audioconvert_core as core;
use core::{
    Channels, CueTrack, OutputMode, ProbeInfo, SampleRate, Settings, SourceFormat, TargetFormat,
    TARGET_FORMATS,
};
use eframe::egui;

/// UI language, following the system: Simplified Chinese when the
/// system language is Chinese, English otherwise.
#[derive(Clone, Copy, PartialEq)]
enum Lang {
    Zh,
    En,
}

fn t(lang: Lang, zh: &'static str, en: &'static str) -> &'static str {
    match lang {
        Lang::Zh => zh,
        Lang::En => en,
    }
}

fn detect_lang() -> Lang {
    // macOS: the system language list is authoritative for GUI apps.
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleLanguages"])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(first) = text.split('"').nth(1) {
                return if first.trim().to_lowercase().starts_with("zh") {
                    Lang::Zh
                } else {
                    Lang::En
                };
            }
        }
        if let Ok(out) = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleLocale"])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_lowercase();
            if !text.is_empty() {
                return if text.starts_with("zh") { Lang::Zh } else { Lang::En };
            }
        }
    }
    // Windows: ask the OS for the user's display language
    // (LANGID primary language 0x04 = Chinese).
    #[cfg(target_os = "windows")]
    {
        unsafe extern "system" {
            fn GetUserDefaultUILanguage() -> u16;
        }
        let langid = unsafe { GetUserDefaultUILanguage() };
        return if langid & 0x3FF == 0x04 {
            Lang::Zh
        } else {
            Lang::En
        };
    }
    for key in ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_lowercase();
            if !v.is_empty() {
                return if v.starts_with("zh") { Lang::Zh } else { Lang::En };
            }
        }
    }
    Lang::En
}

#[derive(Clone, PartialEq)]
enum SkipKind {
    SameFormat,
    Exists,
    Midi,
}

#[derive(Clone, PartialEq)]
enum RowResult {
    Converted { new_size: u64 },
    Skipped(SkipKind),
    Failed(String),
}

#[derive(Clone, PartialEq)]
enum RowState {
    Pending,
    Done(RowResult),
}

struct Row {
    path: PathBuf,
    root: PathBuf,
    rel: String,
    size: u64,
    format: SourceFormat,
    probe: Option<ProbeInfo>,
    probed: bool,
    state: RowState,
    /// Set when this row is one track of a CUE sheet: `path` is
    /// the sheet's audio file, shared by all its track rows.
    cue: Option<CueTrack>,
    /// The sheet this row came from (duplicate detection).
    cue_src: Option<PathBuf>,
}

enum Msg {
    Probe(usize, Option<ProbeInfo>),
    ConvertStart(String),
    Convert(usize, RowResult),
    RunFinished,
}

struct AudioConvertApp {
    lang: Lang,
    show_about: bool,
    wants_quit: bool,
    #[cfg(target_os = "macos")]
    native_menu: Option<NativeMenu>,
    #[cfg(target_os = "windows")]
    titlebar_done: bool,
    rows: Vec<Row>,
    ffmpeg: Option<PathBuf>,
    // Conversion settings (mirrors core::Settings).
    target: TargetFormat,
    bitrate_kbps: u32,
    sample_rate: SampleRate,
    channels: Channels,
    keep_tags: bool,
    custom_out: Option<PathBuf>,
    // Run state.
    running: bool,
    stop: Arc<AtomicBool>,
    done_count: usize,
    total_count: usize,
    run_ok: usize,
    run_skipped: usize,
    run_failed: usize,
    current_file: String,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    summary: String,
}

impl Default for AudioConvertApp {
    fn default() -> Self {
        let lang = detect_lang();
        core::set_english(lang == Lang::En);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = Self {
            lang,
            show_about: false,
            wants_quit: false,
            #[cfg(target_os = "macos")]
            native_menu: None,
            #[cfg(target_os = "windows")]
            titlebar_done: false,
            rows: Vec::new(),
            ffmpeg: core::find_ffmpeg(),
            target: TargetFormat::Mp3,
            bitrate_kbps: 320,
            sample_rate: SampleRate::Keep,
            channels: Channels::Keep,
            keep_tags: true,
            custom_out: None,
            running: false,
            stop: Arc::new(AtomicBool::new(false)),
            done_count: 0,
            total_count: 0,
            run_ok: 0,
            run_skipped: 0,
            run_failed: 0,
            current_file: String::new(),
            tx,
            rx,
            summary: String::new(),
        };
        app.summary = if app.ffmpeg.is_none() {
            t(lang, "没有找到 ffmpeg，转换功能不可用。", "ffmpeg not found — conversion is unavailable.").to_string()
        } else {
            t(lang, "添加文件或添加目录，也可以把文件、文件夹直接拖进来。", "Add files or a folder — or just drag files and folders in.").to_string()
        };
        // Dev hook: preload a folder at startup when asked.
        if let Ok(dir) = std::env::var("AUDIOCONVERT_PRELOAD") {
            let dir = PathBuf::from(dir);
            if dir.is_dir() {
                app.scan_dir(dir);
            }
        }
        // Dev hook: kick off a conversion immediately (modal checks).
        if std::env::var("AUDIOCONVERT_AUTORUN").is_ok() && !app.rows.is_empty() {
            app.start_convert();
        }
        app
    }
}

fn format_bytes(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1} KB", n as f64 / 1_000.0)
    } else {
        format!("{n} B")
    }
}

impl AudioConvertApp {
    fn settings(&self) -> Settings {
        Settings {
            target: self.target,
            bitrate_kbps: self.bitrate_kbps,
            sample_rate: self.sample_rate,
            channels: self.channels,
            keep_tags: self.keep_tags,
            output: match &self.custom_out {
                Some(dir) => OutputMode::Custom(dir.clone()),
                None => OutputMode::SameFolder,
            },
        }
    }

    fn scan_dir(&mut self, dir: PathBuf) {
        let files = core::collect_audio_files(&dir);
        let base = self.rows.len();
        for path in files {
            let rel = path
                .strip_prefix(&dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            self.ingest(path, dir.clone(), rel);
        }
        self.after_rows_added(base);
    }

    fn add_files(&mut self, files: Vec<PathBuf>) {
        let base = self.rows.len();
        for path in files {
            let root = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
            let rel = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.ingest(path, root, rel);
        }
        self.after_rows_added(base);
    }

    /// Add one collected path: a CUE sheet becomes one row per
    /// track, an audio file with a CUE sidecar becomes its
    /// tracks, anything else becomes a plain row. Anything
    /// already listed — plainly, or as a sheet's audio — is
    /// skipped.
    fn ingest(&mut self, path: PathBuf, root: PathBuf, rel: String) {
        let is_cue = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("cue"))
            .unwrap_or(false);
        if is_cue {
            if self.rows.iter().any(|r| r.cue_src.as_deref() == Some(path.as_path())) {
                return;
            }
            self.push_cue_rows(&path, root, &rel);
            return;
        }
        if self.rows.iter().any(|r| r.path == path) {
            return;
        }
        if let Some(sidecar) = core::cue_sidecar(&path) {
            if self.rows.iter().any(|r| r.cue_src.as_deref() == Some(sidecar.as_path())) {
                return;
            }
            self.push_cue_rows(&sidecar, root, &rel);
            return;
        }
        self.push_row(path, root, rel);
    }

    /// Expand a CUE sheet into one row per track. A sheet that
    /// fails to parse still appears — as a failed row, with the
    /// reason — instead of vanishing silently.
    fn push_cue_rows(&mut self, cue_path: &Path, root: PathBuf, rel: &str) {
        let sheet = match core::parse_cue(cue_path) {
            Ok(sheet) => sheet,
            Err(why) => {
                let format = core::detect_format(cue_path);
                self.rows.push(Row {
                    size: std::fs::metadata(cue_path).map(|m| m.len()).unwrap_or(0),
                    path: cue_path.to_path_buf(),
                    root,
                    rel: rel.to_string(),
                    format,
                    probe: None,
                    probed: true,
                    state: RowState::Done(RowResult::Failed(why)),
                    cue: None,
                    cue_src: Some(cue_path.to_path_buf()),
                });
                return;
            }
        };
        let prefix = match rel.rfind('/') {
            Some(i) => &rel[..=i],
            None => "",
        };
        for track in sheet.tracks {
            let format = core::detect_format(&track.file);
            self.rows.push(Row {
                size: std::fs::metadata(&track.file).map(|m| m.len()).unwrap_or(0),
                path: track.file.clone(),
                root: root.clone(),
                rel: format!("{prefix}{}", track.file_stem()),
                format,
                probe: None,
                probed: false,
                state: RowState::Pending,
                cue: Some(track),
                cue_src: Some(cue_path.to_path_buf()),
            });
        }
    }

    fn push_row(&mut self, path: PathBuf, root: PathBuf, rel: String) {
        let format = core::detect_format(&path);
        self.rows.push(Row {
            size: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            path,
            root,
            rel,
            format,
            probe: None,
            probed: false,
            state: RowState::Pending,
            cue: None,
            cue_src: None,
        });
    }

    fn after_rows_added(&mut self, _base: usize) {
        if !self.rows.is_empty() {
            self.summary = match self.lang {
                Lang::Zh => format!("共 {} 个文件，选好目标格式后点「开始转换」。", self.rows.len()),
                Lang::En => format!("{} files — pick a target format and press Convert.", self.rows.len()),
            };
        }
        self.start_probes();
    }

    /// Probe the rows that have no stream info yet, on a thread.
    fn start_probes(&mut self) {
        let Some(ffmpeg) = self.ffmpeg.clone() else {
            for row in &mut self.rows {
                row.probed = true;
            }
            return;
        };
        let jobs: Vec<(usize, PathBuf)> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.probed)
            .map(|(i, r)| (i, r.path.clone()))
            .collect();
        if jobs.is_empty() {
            return;
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for (i, path) in jobs {
                let info = core::probe(&ffmpeg, &path);
                if tx.send(Msg::Probe(i, info)).is_err() {
                    return;
                }
            }
        });
    }

    fn start_convert(&mut self) {
        if self.running || self.rows.is_empty() {
            return;
        }
        let Some(ffmpeg) = self.ffmpeg.clone() else { return };
        for row in &mut self.rows {
            row.state = RowState::Pending;
        }
        self.done_count = 0;
        self.total_count = self.rows.len();
        self.run_ok = 0;
        self.run_skipped = 0;
        self.run_failed = 0;
        self.current_file.clear();
        self.running = true;
        self.stop.store(false, Ordering::Relaxed);
        let settings = self.settings();
        let stop = self.stop.clone();
        let tx = self.tx.clone();
        let jobs: Vec<(
            usize,
            PathBuf,
            PathBuf,
            SourceFormat,
            Option<ProbeInfo>,
            Option<CueTrack>,
        )> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                (
                    i,
                    r.path.clone(),
                    r.root.clone(),
                    r.format,
                    r.probe.clone(),
                    r.cue.clone(),
                )
            })
            .collect();
        self.summary = t(self.lang, "正在转换……", "Converting…").to_string();
        std::thread::spawn(move || {
            for (i, src, root, format, probe, cue) in jobs {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let name = match &cue {
                    Some(track) => track.file_stem(),
                    None => src
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| src.to_string_lossy().into_owned()),
                };
                if tx.send(Msg::ConvertStart(name)).is_err() {
                    return;
                }
                let result = convert_one(
                    &ffmpeg,
                    &src,
                    &root,
                    format,
                    probe.as_ref(),
                    &settings,
                    &stop,
                    cue.as_ref(),
                );
                if tx.send(Msg::Convert(i, result)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Msg::RunFinished);
        });
    }

    fn poll_msgs(&mut self, ctx: &egui::Context) {
        let mut any = false;
        loop {
            match self.rx.try_recv() {
                Ok(Msg::Probe(i, info)) => {
                    if let Some(row) = self.rows.get_mut(i) {
                        row.probe = info;
                        row.probed = true;
                    }
                    any = true;
                }
                Ok(Msg::ConvertStart(name)) => {
                    self.current_file = name;
                    any = true;
                }
                Ok(Msg::Convert(i, result)) => {
                    match &result {
                        RowResult::Converted { .. } => self.run_ok += 1,
                        RowResult::Skipped(_) => self.run_skipped += 1,
                        RowResult::Failed(_) => self.run_failed += 1,
                    }
                    if let Some(row) = self.rows.get_mut(i) {
                        row.state = RowState::Done(result);
                    }
                    self.done_count += 1;
                    any = true;
                }
                Ok(Msg::RunFinished) => {
                    self.running = false;
                    self.finish_summary();
                    any = true;
                }
                Err(_) => break,
            }
        }
        if self.running || self.rows.iter().any(|r| !r.probed) {
            ctx.request_repaint();
        } else if any {
            ctx.request_repaint();
        }
    }

    fn clear_list(&mut self) {
        self.rows.clear();
        self.done_count = 0;
        self.total_count = 0;
        self.current_file.clear();
        self.summary = t(
            self.lang,
            "添加文件或添加目录，也可以把文件、文件夹直接拖进来。",
            "Add files or a folder — or just drag files and folders in.",
        )
        .to_string();
    }

    /// Modal progress layer shown while a conversion run is
    /// going (the VideoTag pattern): the footer progress bar
    /// sat awkwardly under the list; this floats above
    /// everything, blocks input to the UI behind it, and
    /// carries the spinner, the bar, the current file, the
    /// tallies and the Stop button. It closes itself when the
    /// run finishes.
    fn convert_progress_modal(&mut self, ctx: &egui::Context) {
        let lang = self.lang;
        let done = self.done_count;
        let total = self.total_count;
        let frac = if total > 0 {
            done as f32 / total as f32
        } else {
            0.0
        };
        let stop = self.stop.clone();
        egui::Modal::new(egui::Id::new("convert_progress")).show(ctx, |ui| {
            ui.set_width(520.0);
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(20, 18))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t(lang, "正在转换…", "Converting…"));
                    });
                    ui.add_space(14.0);
                    progress_bar_ink(ui, frac, &format!("{done} / {total}"));
                    ui.add_space(12.0);
                    if self.current_file.is_empty() {
                        ui.label(" ");
                    } else {
                        ui.add(
                            egui::Label::new(self.current_file.as_str()).truncate(),
                        );
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "{} {} · {} {} · {} {}",
                            t(lang, "成功", "Converted"),
                            self.run_ok,
                            t(lang, "跳过", "skipped"),
                            self.run_skipped,
                            t(lang, "失败", "failed"),
                            self.run_failed
                        ));
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if painted_button(
                                    ui,
                                    t(lang, "停止", "Stop"),
                                    14.0,
                                    egui::vec2(96.0, 32.0),
                                    true,
                                ) {
                                    stop.store(true, Ordering::Relaxed);
                                }
                            },
                        );
                    });
                });
        });
    }

    fn finish_summary(&mut self) {
        let (mut converted, mut skipped, mut failed, mut pending) = (0, 0, 0, 0);
        for row in &self.rows {
            match &row.state {
                RowState::Done(RowResult::Converted { .. }) => converted += 1,
                RowState::Done(RowResult::Skipped(_)) => skipped += 1,
                RowState::Done(RowResult::Failed(_)) => failed += 1,
                RowState::Pending => pending += 1,
            }
        }
        self.summary = match self.lang {
            Lang::Zh => {
                let mut s = format!("完成：转换 {converted} 个，跳过 {skipped} 个，失败 {failed} 个。");
                if pending > 0 {
                    s.push_str(&format!("已停止，剩 {pending} 个未处理。"));
                }
                s
            }
            Lang::En => {
                let mut s = format!("Done: converted {converted}, skipped {skipped}, failed {failed}.");
                if pending > 0 {
                    s.push_str(&format!(" Stopped with {pending} left."));
                }
                s
            }
        };
    }
}

/// One file's verdict, decided and executed on the worker thread.
fn convert_one(
    ffmpeg: &Path,
    src: &Path,
    root: &Path,
    format: SourceFormat,
    probe: Option<&ProbeInfo>,
    settings: &Settings,
    stop: &AtomicBool,
    cue: Option<&CueTrack>,
) -> RowResult {
    // A CUE track is cut from its whole-disc file. It never hits
    // the same-format skip: splitting is the point, even when the
    // target format equals the disc's format.
    if let Some(track) = cue {
        let dst = core::plan_track_output(src, root, settings, track);
        if dst.exists() {
            return RowResult::Skipped(SkipKind::Exists);
        }
        return match core::convert_track(ffmpeg, src, &dst, settings, probe, stop, &track.slice()) {
            Ok(new_size) => RowResult::Converted { new_size },
            Err(core::ConvertError::Stopped) => {
                RowResult::Failed(core::ConvertError::Stopped.to_string())
            }
            Err(e) => RowResult::Failed(e.to_string()),
        };
    }
    if format == SourceFormat::Midi {
        return RowResult::Skipped(SkipKind::Midi);
    }
    let codec = probe.map(|p| p.codec.as_str());
    if settings.target.same_as(format, codec) {
        return RowResult::Skipped(SkipKind::SameFormat);
    }
    let dst = core::plan_output(src, root, settings);
    if dst == src {
        return RowResult::Skipped(SkipKind::SameFormat);
    }
    if dst.exists() {
        return RowResult::Skipped(SkipKind::Exists);
    }
    match core::convert(ffmpeg, src, &dst, settings, probe, stop) {
        Ok(new_size) => RowResult::Converted { new_size },
        Err(core::ConvertError::Stopped) => {
            RowResult::Failed(core::ConvertError::Stopped.to_string())
        }
        Err(e) => RowResult::Failed(e.to_string()),
    }
}

fn status_text(row: &Row, lang: Lang, target: TargetFormat) -> (String, egui::Color32) {
    let green = egui::Color32::from_rgb(46, 160, 90);
    let amber = egui::Color32::from_rgb(200, 140, 40);
    let red = egui::Color32::from_rgb(205, 70, 60);
    match &row.state {
        RowState::Pending => (t(lang, "待处理", "Pending").into(), egui::Color32::GRAY),
        RowState::Done(RowResult::Converted { new_size }) => (
            match lang {
                Lang::Zh => format!("已转换 · {}", format_bytes(*new_size)),
                Lang::En => format!("Converted · {}", format_bytes(*new_size)),
            },
            green,
        ),
        RowState::Done(RowResult::Skipped(SkipKind::SameFormat)) => (
            match lang {
                Lang::Zh => format!("跳过：已是 {}", target.label()),
                Lang::En => format!("Skipped: already {}", target.label()),
            },
            amber,
        ),
        RowState::Done(RowResult::Skipped(SkipKind::Exists)) => {
            (t(lang, "跳过：目标已存在", "Skipped: output exists").into(), amber)
        }
        RowState::Done(RowResult::Skipped(SkipKind::Midi)) => {
            (t(lang, "跳过：MIDI 需合成", "Skipped: MIDI needs a synth").into(), amber)
        }
        RowState::Done(RowResult::Failed(why)) => (
            match lang {
                Lang::Zh => format!("失败：{why}"),
                Lang::En => format!("Failed: {why}"),
            },
            red,
        ),
    }
}

/// A button painted by hand (same fill/stroke as the stock button)
/// whose text is centred on the actual glyphs — stock egui buttons
/// centre the (taller) line box instead, which leaves CJK text
/// riding high.
fn painted_button(
    ui: &mut egui::Ui,
    text: &str,
    font_size: f32,
    min_size: egui::Vec2,
    enabled: bool,
) -> bool {
    let sense = if enabled { egui::Sense::click() } else { egui::Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(min_size, sense);
    let visuals = if enabled {
        *ui.style().interact(&response)
    } else {
        ui.visuals().widgets.inactive
    };
    let painter = ui.painter();
    painter.rect(
        rect,
        egui::CornerRadius::same(5),
        visuals.bg_fill,
        visuals.bg_stroke,
        egui::StrokeKind::Inside,
    );
    let galley = painter.layout_no_wrap(
        text.to_string(),
        egui::FontId::proportional(font_size),
        visuals.text_color(),
    );
    // Center the *ink* (mesh bounds), not the line box.
    let pos = rect.center() - galley.mesh_bounds.center().to_vec2();
    painter.galley(pos, galley, visuals.text_color());
    enabled && response.clicked()
}

/// Height of every control in the settings row.
const SETTINGS_H: f32 = 28.0;

/// A settings-row label, hand-painted so its ink shares the
/// controls' exact centre line (stock labels centre the taller
/// line box, which leaves CJK glyphs riding high).
fn settings_label(ui: &mut egui::Ui, text: &str) {
    let font = egui::FontId::proportional(14.5);
    let color = ui.visuals().text_color();
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let width = galley.size().x;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, SETTINGS_H), egui::Sense::hover());
    let mb = galley.mesh_bounds;
    ui.painter().galley(
        egui::pos2(rect.left() - mb.min.x, rect.center().y - mb.center().y),
        galley,
        color,
    );
}

/// Hand-drawn dropdown for the settings row, after the
/// SmartReName pattern: stock ComboBox centres the line box, not
/// the ink, so CJK text floated in the box; this paints its own
/// text and chevron on the buttons' centre line.
fn settings_dropdown<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    id: &str,
    text: &str,
    width: f32,
    enabled: bool,
    value: &mut T,
    options: &[(T, String)],
) {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, SETTINGS_H),
        if enabled { egui::Sense::click() } else { egui::Sense::hover() },
    );
    let visuals = if enabled {
        *ui.style().interact(&response)
    } else {
        ui.visuals().widgets.inactive
    };
    let painter = ui.painter();
    painter.rect(
        rect,
        egui::CornerRadius::same(7),
        visuals.bg_fill,
        visuals.bg_stroke,
        egui::StrokeKind::Inside,
    );
    let font = egui::FontId::proportional(14.5);
    let color = visuals.text_color();
    let galley = painter.layout_no_wrap(text.to_string(), font.clone(), color);
    painter.galley(
        egui::pos2(
            rect.left() + 12.0 - galley.mesh_bounds.min.x,
            rect.center().y - galley.mesh_bounds.center().y,
        ),
        galley,
        color,
    );
    // Painted chevron at the right end.
    let cx = rect.right() - 15.0;
    let cy = rect.center().y;
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(cx - 5.0, cy - 3.0),
            egui::pos2(cx + 5.0, cy - 3.0),
            egui::pos2(cx, cy + 3.5),
        ],
        color,
        egui::Stroke::NONE,
    ));
    if !enabled {
        return;
    }
    let popup_id = egui::Id::new(id);
    if response.clicked() {
        ui.memory_mut(|m| m.toggle_popup(popup_id));
    }
    egui::popup_below_widget(
        ui,
        popup_id,
        &response,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            ui.set_min_width(width - 16.0);
            // The format list outgrew the window (20 entries), so
            // the popup scrolls past eleven rows instead of
            // running off the bottom of the screen.
            egui::ScrollArea::vertical()
                .max_height(352.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
            for (opt, name) in options {
                let (row, resp) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 32.0),
                    egui::Sense::click(),
                );
                let p = ui.painter();
                if resp.hovered() {
                    p.rect_filled(
                        row,
                        egui::CornerRadius::same(6),
                        ui.visuals().widgets.hovered.bg_fill,
                    );
                }
                let row_color = ui.visuals().text_color();
                if *opt == *value {
                    // Painted check mark, left of the label.
                    let s = egui::Stroke::new(1.8_f32, row_color);
                    let c = egui::pos2(row.left() + 12.0, row.center().y);
                    p.line_segment(
                        [egui::pos2(c.x - 4.5, c.y + 0.5), egui::pos2(c.x - 1.2, c.y + 3.8)],
                        s,
                    );
                    p.line_segment(
                        [egui::pos2(c.x - 1.2, c.y + 3.8), egui::pos2(c.x + 5.0, c.y - 3.8)],
                        s,
                    );
                }
                let g = p.layout_no_wrap(name.clone(), font.clone(), row_color);
                p.galley(
                    egui::pos2(
                        row.left() + 28.0 - g.mesh_bounds.min.x,
                        row.center().y - g.mesh_bounds.center().y,
                    ),
                    g,
                    row_color,
                );
                if resp.clicked() {
                    *value = *opt;
                    ui.memory_mut(|m| m.close_popup());
                }
            }
                });
        },
    );
}

/// Hand-painted checkbox in the settings-row style: a rounded
/// box with a painted check mark, label centred on its ink —
/// the stock egui checkbox centres on the line box, which
/// rides high with the CJK fallback font.
fn settings_checkbox(ui: &mut egui::Ui, text: &str, enabled: bool, value: &mut bool) {
    let font = egui::FontId::proportional(14.5);
    let measure = ui
        .painter()
        .layout_no_wrap(text.to_string(), font.clone(), egui::Color32::WHITE);
    let text_w = measure.mesh_bounds.max.x - measure.mesh_bounds.min.x;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(18.0 + 7.0 + text_w, SETTINGS_H),
        if enabled { egui::Sense::click() } else { egui::Sense::hover() },
    );
    let visuals = if enabled {
        *ui.style().interact(&response)
    } else {
        ui.visuals().widgets.inactive
    };
    let painter = ui.painter();
    let box_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left(), rect.center().y - 9.0),
        egui::vec2(18.0, 18.0),
    );
    painter.rect(
        box_rect,
        egui::CornerRadius::same(5),
        visuals.bg_fill,
        visuals.bg_stroke,
        egui::StrokeKind::Inside,
    );
    let color = visuals.text_color();
    if *value {
        let s = egui::Stroke::new(1.8_f32, color);
        let c = box_rect.center();
        painter.line_segment(
            [egui::pos2(c.x - 4.5, c.y + 0.5), egui::pos2(c.x - 1.2, c.y + 3.8)],
            s,
        );
        painter.line_segment(
            [egui::pos2(c.x - 1.2, c.y + 3.8), egui::pos2(c.x + 5.0, c.y - 3.8)],
            s,
        );
    }
    let galley = painter.layout_no_wrap(text.to_string(), font, color);
    painter.galley(
        egui::pos2(
            rect.left() + 25.0 - galley.mesh_bounds.min.x,
            rect.center().y - galley.mesh_bounds.center().y,
        ),
        galley,
        color,
    );
    if enabled && response.clicked() {
        *value = !*value;
    }
}

/// Hand-painted progress bar with ink-centred text (the
/// VideoTag helper): stock ProgressBar centres the line box,
/// so CJK text rode high; here the label is centred on its
/// glyph bounds, painted twice — plain over the track, white
/// clipped to the filled part.
fn progress_bar_ink(ui: &mut egui::Ui, frac: f32, text: &str) {
    let height = 20.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    let visuals = ui.visuals();
    let radius = height / 2.0;
    painter.rect_filled(rect, radius, visuals.extreme_bg_color);
    let frac = frac.clamp(0.0, 1.0);
    let mut fill_rect = rect;
    fill_rect.set_width(rect.width() * frac);
    if frac > 0.0 {
        painter.rect_filled(fill_rect, radius, visuals.selection.bg_fill);
    }
    let font = egui::FontId::proportional(12.0);
    let center = rect.center();
    let galley = painter.layout_no_wrap(
        text.to_string(),
        font.clone(),
        visuals.text_color(),
    );
    let pos = center - galley.mesh_bounds.center().to_vec2();
    painter.galley(pos, galley, visuals.text_color());
    if fill_rect.width() > 1.0 {
        let clipped = painter.with_clip_rect(fill_rect);
        let galley =
            clipped.layout_no_wrap(text.to_string(), font, egui::Color32::WHITE);
        let pos = center - galley.mesh_bounds.center().to_vec2();
        clipped.galley(pos, galley, egui::Color32::WHITE);
    }
}

/// Open a URL in the user's default browser (Help menu links).
fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn();
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

impl AudioConvertApp {
    fn pick_dir_dialog(&mut self) {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                self.scan_dir(dir);
            }
        }
    }

    fn pick_files_dialog(&mut self) {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            if let Some(files) = rfd::FileDialog::new().pick_files() {
                self.add_files(files);
            }
        }
    }

    fn pick_out_dir_dialog(&mut self) {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                self.summary = match self.lang {
                    Lang::Zh => format!("保存到：{}", dir.to_string_lossy()),
                    Lang::En => format!("Save to: {}", dir.to_string_lossy()),
                };
                self.custom_out = Some(dir);
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn native_menu_update(&mut self) {
        if self.native_menu.is_none() {
            self.native_menu = build_native_menu(self.lang);
        }
        let (about_id, open_dir_id, open_files_id, convert_id, quit_ids, homepage_id, github_id) = {
            let Some(menu) = &self.native_menu else {
                return;
            };
            (
                menu.about.id().clone(),
                menu.open_dir.id().clone(),
                menu.open_files.id().clone(),
                menu.convert.id().clone(),
                [menu.quit_app.id().clone(), menu.quit_file.id().clone()],
                menu.homepage.id().clone(),
                menu.github.id().clone(),
            )
        };
        while let Ok(event) = muda::MenuEvent::receiver().try_recv() {
            let id = event.id();
            if id == &about_id {
                self.show_about = true;
            } else if id == &homepage_id {
                open_url("https://mrben.net");
            } else if id == &github_id {
                open_url("https://github.com/i-love-osaka/AudioConvert");
            } else if id == &open_dir_id {
                if !self.running {
                    self.pick_dir_dialog();
                }
            } else if id == &open_files_id {
                if !self.running {
                    self.pick_files_dialog();
                }
            } else if id == &convert_id {
                self.start_convert();
            } else if quit_ids.iter().any(|q| q == id) {
                self.wants_quit = true;
            }
        }
    }
}

#[cfg(target_os = "macos")]
struct NativeMenu {
    #[allow(dead_code)]
    attached: bool,
    _menu: muda::Menu,
    about: muda::MenuItem,
    quit_app: muda::MenuItem,
    open_dir: muda::MenuItem,
    open_files: muda::MenuItem,
    quit_file: muda::MenuItem,
    convert: muda::MenuItem,
    homepage: muda::MenuItem,
    github: muda::MenuItem,
}

/// The primary shortcut modifier: Command on macOS, Control on Windows.
#[cfg(target_os = "macos")]
const PRIMARY_MOD: muda::accelerator::Modifiers = muda::accelerator::Modifiers::META;

#[cfg(target_os = "macos")]
fn build_native_menu(lang: Lang) -> Option<NativeMenu> {
    use muda::{Menu, MenuItem, PredefinedMenuItem, Submenu};
    let menu = Menu::new();
    let app_menu = Submenu::new("AudioConvert", true);
    // Custom quit items with our own text: the predefined quit item
    // keeps its English title regardless of the system language.
    let quit_accel = Some(muda::accelerator::Accelerator::new(
        Some(PRIMARY_MOD),
        muda::accelerator::Code::KeyQ,
    ));
    let quit_text = t(lang, "退出 AudioConvert", "Quit AudioConvert");
    let about = MenuItem::new(t(lang, "关于 AudioConvert", "About AudioConvert"), true, None);
    let quit_app = MenuItem::new(quit_text, true, quit_accel);
    app_menu.append(&about).ok()?;
    app_menu.append(&PredefinedMenuItem::separator()).ok()?;
    app_menu.append(&quit_app).ok()?;
    menu.append(&app_menu).ok()?;
    let file_menu = Submenu::new(t(lang, "文件", "File"), true);
    let open_dir = MenuItem::new(
        t(lang, "添加目录…", "Add Folder…"),
        true,
        Some(muda::accelerator::Accelerator::new(
            Some(PRIMARY_MOD),
            muda::accelerator::Code::KeyO,
        )),
    );
    let open_files = MenuItem::new(t(lang, "添加文件…", "Add Files…"), true, None);
    let quit_file = MenuItem::new(quit_text, true, quit_accel);
    file_menu.append(&open_dir).ok()?;
    file_menu.append(&open_files).ok()?;
    file_menu.append(&PredefinedMenuItem::separator()).ok()?;
    file_menu.append(&quit_file).ok()?;
    menu.append(&file_menu).ok()?;
    let action_menu = Submenu::new(t(lang, "转换", "Convert"), true);
    let convert = MenuItem::new(t(lang, "开始转换", "Start Converting"), true, None);
    action_menu.append(&convert).ok()?;
    menu.append(&action_menu).ok()?;
    // Help menu: About, plus links to the author's homepage and
    // the GitHub repository.
    let help_menu = Submenu::new(t(lang, "帮助", "Help"), true);
    let homepage = MenuItem::new(t(lang, "作者主页", "Author's Homepage"), true, None);
    let github = MenuItem::new(t(lang, "在GitHub上查看", "View on GitHub"), true, None);
    help_menu.append(&about).ok()?;
    help_menu.append(&PredefinedMenuItem::separator()).ok()?;
    help_menu.append(&homepage).ok()?;
    help_menu.append(&github).ok()?;
    menu.append(&help_menu).ok()?;
    menu.init_for_nsapp();
    Some(NativeMenu {
        attached: true,
        _menu: menu,
        about,
        quit_app,
        open_dir,
        open_files,
        quit_file,
        convert,
        homepage,
        github,
    })
}

/// Whether Windows is in dark mode for apps (AppsUseLightTheme = 0).
#[cfg(target_os = "windows")]
fn windows_system_dark() -> bool {
    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(
            hkey: isize,
            sub_key: *const u16,
            value: *const u16,
            flags: u32,
            pdw_type: *mut u32,
            pv_data: *mut std::ffi::c_void,
            pcb_data: *mut u32,
        ) -> i32;
    }
    let sub_key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value_name: Vec<u16> = "AppsUseLightTheme"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut data: u32 = 1;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            -2147483647, // HKEY_CURRENT_USER
            sub_key.as_ptr(),
            value_name.as_ptr(),
            0x0000_0018, // RRF_RT_REG_DWORD
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut std::ffi::c_void,
            &mut size,
        )
    };
    status == 0 && data == 0
}

/// Opt the process into the Windows app mode matching the system
/// theme (uxtheme's SetPreferredAppMode, ordinal 135), so menus
/// render dark under Windows dark mode. Must run before any menu.
#[cfg(target_os = "windows")]
fn windows_apply_app_mode() {
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(file_name: *const u16) -> isize;
        fn GetProcAddress(module: isize, proc_name: *const u8) -> usize;
    }
    unsafe {
        let name: Vec<u16> = "uxtheme.dll"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let module = LoadLibraryW(name.as_ptr());
        if module == 0 {
            return;
        }
        // Ordinal 135: SetPreferredAppMode(0=Default, 1=AllowDark).
        let addr = GetProcAddress(module, 135usize as *const u8);
        if addr != 0 {
            let set_mode: unsafe extern "system" fn(i32) -> i32 =
                std::mem::transmute(addr);
            let _ = set_mode(if windows_system_dark() { 1 } else { 0 });
        }
        // Ordinal 136: FlushMenuThemes().
        let addr = GetProcAddress(module, 136usize as *const u8);
        if addr != 0 {
            let flush: unsafe extern "system" fn() = std::mem::transmute(addr);
            flush();
        }
    }
}

/// Match the title bar to the app mode (DWMWA_USE_IMMERSIVE_DARK_MODE).
#[cfg(target_os = "windows")]
fn windows_set_dark_titlebar(hwnd: isize) {
    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: isize,
            attr: u32,
            value: *const std::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    let value: i32 = if windows_system_dark() { 1 } else { 0 };
    unsafe {
        // Attribute 20 on current Windows, 19 on older builds.
        if DwmSetWindowAttribute(
            hwnd,
            20,
            &value as *const i32 as *const std::ffi::c_void,
            4,
        ) != 0
        {
            let _ = DwmSetWindowAttribute(
                hwnd,
                19,
                &value as *const i32 as *const std::ffi::c_void,
                4,
            );
        }
    }
}

/// The native window handle, for the Windows title-bar mode.
#[cfg(target_os = "windows")]
fn window_hwnd(frame: &eframe::Frame) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
        _ => None,
    }
}

/// Paint text with its ink's left edge at `left`, ink vertically
/// centered at `center_y`.
#[cfg(not(target_os = "macos"))]
fn paint_text_ink_left(
    painter: &egui::Painter,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    left: f32,
    center_y: f32,
) {
    let galley = painter.layout_no_wrap(text.to_string(), font, color);
    let mb = galley.mesh_bounds;
    painter.galley(egui::pos2(left - mb.min.x, center_y - mb.center().y), galley, color);
}

/// Same, with the ink's right edge at `right`.
#[cfg(not(target_os = "macos"))]
fn paint_text_ink_right(
    painter: &egui::Painter,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    right: f32,
    center_y: f32,
) {
    let galley = painter.layout_no_wrap(text.to_string(), font, color);
    let mb = galley.mesh_bounds;
    painter.galley(egui::pos2(right - mb.max.x, center_y - mb.center().y), galley, color);
}

/// One row in a drop-down menu: full width, label ink-left at a
/// fixed inset, optional accelerator ink-right, hover highlight,
/// optional check mark.
#[cfg(not(target_os = "macos"))]
fn menu_row(
    ui: &mut egui::Ui,
    label: &str,
    accel: Option<&str>,
    checked: bool,
    enabled: bool,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 30.0),
        if enabled { egui::Sense::click() } else { egui::Sense::hover() },
    );
    let p = ui.painter();
    if enabled && resp.hovered() {
        p.rect_filled(rect, egui::CornerRadius::same(4), menu_hover_color(ui));
    }
    let font = egui::FontId::proportional(13.5);
    let color = if enabled {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    if checked {
        paint_text_ink_left(p, "\u{2713}", font.clone(), color, rect.left() + 4.0, rect.center().y);
    }
    paint_text_ink_left(p, label, font.clone(), color, rect.left() + 17.0, rect.center().y);
    if let Some(a) = accel {
        let accel_color = if enabled && resp.hovered() {
            ui.visuals().text_color().gamma_multiply(0.8)
        } else {
            ui.visuals().weak_text_color()
        };
        paint_text_ink_right(
            p,
            a,
            font,
            accel_color,
            rect.right() - 10.0,
            rect.center().y,
        );
    }
    enabled && resp.clicked()
}

/// The one hover color for menu bar items and drop-down rows alike.
#[cfg(not(target_os = "macos"))]
fn menu_hover_color(ui: &egui::Ui) -> egui::Color32 {
    let bg = ui.visuals().window_fill;
    if ui.visuals().dark_mode {
        egui::Color32::from_rgb(
            bg.r().saturating_add(32),
            bg.g().saturating_add(32),
            bg.b().saturating_add(34),
        )
    } else {
        egui::Color32::from_rgb(
            bg.r().saturating_sub(16),
            bg.g().saturating_sub(16),
            bg.b().saturating_sub(16),
        )
    }
}

/// Separator line color (themed hairline).
#[cfg(not(target_os = "macos"))]
fn menu_sep_color(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        egui::Color32::from_white_alpha(36)
    } else {
        egui::Color32::from_black_alpha(28)
    }
}

/// Width that fits a menu's rows: the widest label + accelerator,
/// plus the fixed insets the rows paint with.
#[cfg(not(target_os = "macos"))]
fn menu_width(ui: &egui::Ui, items: &[(&str, Option<&str>)]) -> f32 {
    let font = egui::FontId::proportional(13.5);
    let mut widest = 0.0f32;
    for (label, accel) in items {
        let mut w = ui
            .fonts(|f| f.layout_no_wrap((*label).to_string(), font.clone(), egui::Color32::WHITE))
            .size()
            .x;
        if let Some(a) = accel {
            w += 28.0
                + ui.fonts(|f| {
                    f.layout_no_wrap((*a).to_string(), font.clone(), egui::Color32::WHITE)
                })
                .size()
                .x;
        }
        widest = widest.max(w);
    }
    17.0 + widest + 12.0
}

/// A menu separator spanning the popup width.
#[cfg(not(target_os = "macos"))]
fn menu_sep(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 9.0),
        egui::Sense::hover(),
    );
    let y = rect.center().y;
    ui.painter().line_segment(
        [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
        egui::Stroke::new(1.0_f32, menu_sep_color(ui)),
    );
}

/// Flat styling for the menu bar: transparent until hovered/open,
/// like a Windows system menu.
#[cfg(not(target_os = "macos"))]
fn style_menu_bar(ui: &mut egui::Ui) {
    let hover = menu_hover_color(ui);
    let style = ui.style_mut();
    style.spacing.button_padding = egui::vec2(9.0, 3.0);
    style.spacing.item_spacing.x = 4.0;
    style.visuals.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
    style.visuals.widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
    style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(4);
    style.visuals.widgets.hovered.weak_bg_fill = hover;
    style.visuals.widgets.hovered.bg_fill = hover;
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(4);
    style.visuals.widgets.open.weak_bg_fill = hover;
    style.visuals.widgets.open.bg_fill = hover;
    style.visuals.widgets.open.corner_radius = egui::CornerRadius::same(4);
    style.visuals.widgets.active.weak_bg_fill = hover;
    style.visuals.widgets.active.bg_fill = hover;
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(4);
}

impl AudioConvertApp {
    /// Set the Windows title-bar mode once the window exists.
    #[cfg(target_os = "windows")]
    fn apply_titlebar_once(&mut self, frame: &eframe::Frame) {
        if !self.titlebar_done {
            if let Some(hwnd) = window_hwnd(frame) {
                windows_set_dark_titlebar(hwnd);
                self.titlebar_done = true;
            }
        }
    }

    /// In-window menu bar for Windows/Linux, in the Windows system
    /// style. macOS keeps the native menu.
    #[cfg(not(target_os = "macos"))]
    fn menu_bar(&mut self, ctx: &egui::Context) {
        if ctx.memory(|m| m.focused()).is_none() {
            let (open, quit) = ctx.input(|i| {
                (
                    i.modifiers.ctrl && i.key_pressed(egui::Key::O),
                    i.modifiers.ctrl && i.key_pressed(egui::Key::Q),
                )
            });
            if open && !self.running {
                self.pick_dir_dialog();
            }
            if quit {
                self.wants_quit = true;
            }
        }
        let lang = self.lang;
        egui::TopBottomPanel::top("menubar")
            .frame(
                egui::Frame::new()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(egui::Margin::symmetric(4, 4)),
            )
            .show(ctx, |ui| {
            ui.horizontal(|ui| {
                style_menu_bar(ui);
                ui.menu_button(t(lang, "文件", "File"), |ui| {
                    let w = menu_width(ui, &[
                        (t(lang, "添加目录…", "Add Folder…"), Some("Ctrl+O")),
                        (t(lang, "添加文件…", "Add Files…"), None),
                        (t(lang, "退出 AudioConvert", "Quit AudioConvert"), Some("Ctrl+Q")),
                    ]);
                    ui.set_min_width(w);
                    ui.set_max_width(w);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    if menu_row(ui, t(lang, "添加目录…", "Add Folder…"), Some("Ctrl+O"), false, !self.running) {
                        self.pick_dir_dialog();
                        ui.close_menu();
                    }
                    if menu_row(ui, t(lang, "添加文件…", "Add Files…"), None, false, !self.running) {
                        self.pick_files_dialog();
                        ui.close_menu();
                    }
                    menu_sep(ui);
                    if menu_row(ui, t(lang, "退出 AudioConvert", "Quit AudioConvert"), Some("Ctrl+Q"), false, true) {
                        self.wants_quit = true;
                        ui.close_menu();
                    }
                });
                ui.menu_button(t(lang, "转换", "Convert"), |ui| {
                    let w = menu_width(ui, &[
                        (t(lang, "开始转换", "Start Converting"), None),
                    ]);
                    ui.set_min_width(w);
                    ui.set_max_width(w);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    if menu_row(ui, t(lang, "开始转换", "Start Converting"), None, false, !self.running && !self.rows.is_empty()) {
                        self.start_convert();
                        ui.close_menu();
                    }
                });
                ui.menu_button(t(lang, "帮助", "Help"), |ui| {
                    let w = menu_width(ui, &[
                        (t(lang, "关于 AudioConvert", "About AudioConvert"), None),
                        (t(lang, "作者主页", "Author's Homepage"), None),
                        (t(lang, "在GitHub上查看", "View on GitHub"), None),
                    ]);
                    ui.set_min_width(w);
                    ui.set_max_width(w);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    if menu_row(ui, t(lang, "关于 AudioConvert", "About AudioConvert"), None, false, true) {
                        self.show_about = true;
                        ui.close_menu();
                    }
                    menu_sep(ui);
                    if menu_row(ui, t(lang, "作者主页", "Author's Homepage"), None, false, true) {
                        open_url("https://mrben.net");
                        ui.close_menu();
                    }
                    if menu_row(ui, t(lang, "在GitHub上查看", "View on GitHub"), None, false, true) {
                        open_url("https://github.com/i-love-osaka/AudioConvert");
                        ui.close_menu();
                    }
                });
            });
        });
    }
}

impl eframe::App for AudioConvertApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "macos")]
        self.native_menu_update();
        #[cfg(not(target_os = "macos"))]
        self.menu_bar(ctx);
        #[cfg(target_os = "windows")]
        self.apply_titlebar_once(_frame);
        if self.wants_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.poll_msgs(ctx);
        if self.show_about {
            show_about_window(ctx, &mut self.show_about, self.lang);
        }

        // Drag & drop: folders get scanned, files get added.
        if !self.running {
            let dropped: Vec<PathBuf> = ctx.input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .collect()
            });
            if !dropped.is_empty() {
                let mut files = Vec::new();
                for path in dropped {
                    if path.is_dir() {
                        self.scan_dir(path);
                    } else {
                        files.push(path);
                    }
                }
                if !files.is_empty() {
                    self.add_files(files);
                }
            }
        }

        let lang = self.lang;
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 48.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    if let Some(dir) = &self.custom_out {
                        let name = dir
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| dir.to_string_lossy().into_owned());
                        ui.label(
                            egui::RichText::new(match lang {
                                Lang::Zh => format!("保存到 {name}"),
                                Lang::En => format!("Save to {name}"),
                            })
                            .weak(),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let open_w = if lang == Lang::En { 128.0 } else { 108.0 };
                        let save_w = if lang == Lang::En { 110.0 } else { 96.0 };
                        if painted_button(ui, t(lang, "保存到…", "Save to…"), 14.0, egui::vec2(save_w, 32.0), !self.running) {
                            self.pick_out_dir_dialog();
                        }
                        if painted_button(ui, t(lang, "添加目录…", "Add Folder…"), 14.0, egui::vec2(open_w, 32.0), !self.running) {
                            self.pick_dir_dialog();
                        }
                        if painted_button(ui, t(lang, "添加文件…", "Add Files…"), 14.0, egui::vec2(open_w, 32.0), !self.running) {
                            self.pick_files_dialog();
                        }
                        let clear_w = if lang == Lang::En { 104.0 } else { 96.0 };
                        if painted_button(ui, t(lang, "清空列表", "Clear List"), 14.0, egui::vec2(clear_w, 32.0), !self.running && !self.rows.is_empty()) {
                            self.clear_list();
                        }
                    });
                },
            );
        });

        egui::TopBottomPanel::top("settings").show(ctx, |ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 54.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let enabled_all = !self.running;
                    settings_label(ui, t(lang, "目标格式", "Format"));
                    ui.add_space(6.0);
                    let format_options: Vec<(TargetFormat, String)> = TARGET_FORMATS
                        .iter()
                        .map(|f| (*f, f.label().to_string()))
                        .collect();
                    let format_text = self.target.label();
                    settings_dropdown(
                        ui,
                        "dd_target_format",
                        format_text,
                        148.0,
                        enabled_all,
                        &mut self.target,
                        &format_options,
                    );
                    ui.add_space(12.0);
                    settings_label(ui, t(lang, "码率", "Bitrate"));
                    ui.add_space(6.0);
                    let bitrate_options: Vec<(u32, String)> = [128u32, 192, 256, 320]
                        .iter()
                        .map(|k| (*k, format!("{k} kbps")))
                        .collect();
                    let bitrate_text = format!("{} kbps", self.bitrate_kbps);
                    settings_dropdown(
                        ui,
                        "dd_bitrate",
                        &bitrate_text,
                        112.0,
                        enabled_all && self.target.lossy(),
                        &mut self.bitrate_kbps,
                        &bitrate_options,
                    );
                    ui.add_space(12.0);
                    settings_label(ui, t(lang, "采样率", "Sample rate"));
                    ui.add_space(6.0);
                    let rate_options: Vec<(SampleRate, String)> = vec![
                        (SampleRate::Keep, t(lang, "保持原样", "Keep").to_string()),
                        (SampleRate::Hz(44_100), "44.1 kHz".to_string()),
                        (SampleRate::Hz(48_000), "48 kHz".to_string()),
                    ];
                    let rate_text = match self.sample_rate {
                        SampleRate::Keep => t(lang, "保持原样", "Keep"),
                        SampleRate::Hz(44_100) => "44.1 kHz",
                        SampleRate::Hz(48_000) => "48 kHz",
                        SampleRate::Hz(_) => "—",
                    };
                    settings_dropdown(
                        ui,
                        "dd_sample_rate",
                        rate_text,
                        116.0,
                        enabled_all,
                        &mut self.sample_rate,
                        &rate_options,
                    );
                    ui.add_space(12.0);
                    settings_label(ui, t(lang, "声道", "Channels"));
                    ui.add_space(6.0);
                    let channel_options: Vec<(Channels, String)> = vec![
                        (Channels::Keep, t(lang, "保持原样", "Keep").to_string()),
                        (Channels::Stereo, t(lang, "立体声", "Stereo").to_string()),
                        (Channels::Mono, t(lang, "单声道", "Mono").to_string()),
                    ];
                    let channel_text = match self.channels {
                        Channels::Keep => t(lang, "保持原样", "Keep"),
                        Channels::Stereo => t(lang, "立体声", "Stereo"),
                        Channels::Mono => t(lang, "单声道", "Mono"),
                    };
                    settings_dropdown(
                        ui,
                        "dd_channels",
                        channel_text,
                        116.0,
                        enabled_all,
                        &mut self.channels,
                        &channel_options,
                    );
                    ui.add_space(12.0);
                    settings_checkbox(
                        ui,
                        t(lang, "保留标签信息", "Keep tags"),
                        enabled_all,
                        &mut self.keep_tags,
                    );
                },
            );
        });

        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 56.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.add(egui::Label::new(&self.summary).truncate());
                    if !self.running {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let btn_w = if lang == Lang::En { 150.0 } else { 118.0 };
                            let can_run = !self.rows.is_empty() && self.ffmpeg.is_some();
                            if painted_button(ui, t(lang, "开始转换", "Convert"), 15.0, egui::vec2(btn_w, 40.0), can_run) {
                                self.start_convert();
                            }
                        });
                    }
                },
            );
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.rows.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(t(lang, "把音频文件或文件夹拖进来，或者用上面的按钮添加。", "Drag audio files or folders in, or use the buttons above."));
                });
                return;
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                let dark = ui.visuals().dark_mode;
                let zebra = if dark {
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 6)
                } else {
                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 6)
                };
                let name_color = ui.visuals().text_color();
                let dim_color = egui::Color32::from_rgba_unmultiplied(150, 150, 150, 255);
                let font = egui::FontId::proportional(14.0);
                let hover_fill = if dark {
                    egui::Color32::from_rgba_unmultiplied(120, 170, 255, 30)
                } else {
                    egui::Color32::from_rgba_unmultiplied(60, 120, 255, 28)
                };
                for (i, row) in self.rows.iter().enumerate() {
                    let width = ui.available_width();
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(width, 34.0),
                        egui::Sense::hover(),
                    );
                    let painter = ui.painter();
                    if i % 2 == 1 {
                        painter.rect_filled(rect, 4.0, zebra);
                    }
                    if response.hovered() {
                        painter.rect_filled(rect, 4.0, hover_fill);
                    }
                    // Optical centre: +1.5px down — with the CJK
                    // fallback font's metrics, glyph ink otherwise
                    // sits visibly above the row's true centre.
                    let mid_y = rect.center().y + 1.5;
                    // Right-side fixed columns (from the right edge):
                    // status 200, size 90, bitrate 88, duration 64,
                    // format 76.
                    let status_rect = egui::Rect::from_min_max(
                        egui::pos2(rect.right() - 208.0, rect.top()),
                        egui::pos2(rect.right() - 8.0, rect.bottom()),
                    );
                    let size_rect = egui::Rect::from_min_max(
                        egui::pos2(status_rect.left() - 96.0, rect.top()),
                        egui::pos2(status_rect.left() - 6.0, rect.bottom()),
                    );
                    let bitrate_rect = egui::Rect::from_min_max(
                        egui::pos2(size_rect.left() - 94.0, rect.top()),
                        egui::pos2(size_rect.left() - 6.0, rect.bottom()),
                    );
                    let duration_rect = egui::Rect::from_min_max(
                        egui::pos2(bitrate_rect.left() - 70.0, rect.top()),
                        egui::pos2(bitrate_rect.left() - 6.0, rect.bottom()),
                    );
                    let format_rect = egui::Rect::from_min_max(
                        egui::pos2(duration_rect.left() - 82.0, rect.top()),
                        egui::pos2(duration_rect.left() - 6.0, rect.bottom()),
                    );
                    let name_rect = egui::Rect::from_min_max(
                        egui::pos2(rect.left() + 8.0, rect.top()),
                        egui::pos2(format_rect.left() - 10.0, rect.bottom()),
                    );
                    // Filename, truncated with an ellipsis when too long.
                    let mut job = egui::text::LayoutJob::simple(
                        row.rel.clone(),
                        font.clone(),
                        name_color,
                        name_rect.width(),
                    );
                    job.wrap.max_rows = 1;
                    job.wrap.overflow_character = Some('\u{2026}');
                    let galley = painter.layout_job(job);
                    painter.galley(
                        egui::pos2(name_rect.left() - galley.mesh_bounds.min.x, mid_y - galley.mesh_bounds.center().y),
                        galley,
                        name_color,
                    );
                    painter.text(
                        egui::pos2(format_rect.right(), mid_y),
                        egui::Align2::RIGHT_CENTER,
                        row.format.name(),
                        font.clone(),
                        dim_color,
                    );
                    let duration_text = if let Some(track) = &row.cue {
                        // A track row shows its own length, not
                        // the whole disc's: from the sheet, or
                        // the disc length minus the track's start
                        // for a file's last track.
                        match track.duration().or_else(|| {
                            row.probe
                                .as_ref()
                                .map(|p| (p.duration_secs - track.start).max(0.0))
                        }) {
                            Some(d) => core::format_duration(d),
                            None => "—".to_string(),
                        }
                    } else {
                        match (&row.probe, row.probed) {
                            (Some(p), _) => core::format_duration(p.duration_secs),
                            (None, false) => "…".to_string(),
                            (None, true) => "—".to_string(),
                        }
                    };
                    painter.text(
                        egui::pos2(duration_rect.right(), mid_y),
                        egui::Align2::RIGHT_CENTER,
                        duration_text,
                        font.clone(),
                        dim_color,
                    );
                    let bitrate_text = match &row.probe {
                        Some(p) => match p.bitrate_kbps {
                            Some(k) => format!("{k} kbps"),
                            None => "—".to_string(),
                        },
                        None => "—".to_string(),
                    };
                    painter.text(
                        egui::pos2(bitrate_rect.right(), mid_y),
                        egui::Align2::RIGHT_CENTER,
                        bitrate_text,
                        font.clone(),
                        dim_color,
                    );
                    painter.text(
                        egui::pos2(size_rect.right(), mid_y),
                        egui::Align2::RIGHT_CENTER,
                        format_bytes(row.size),
                        font.clone(),
                        dim_color,
                    );
                    let (text, color) = status_text(row, lang, self.target);
                    painter.text(
                        egui::pos2(status_rect.right(), mid_y),
                        egui::Align2::RIGHT_CENTER,
                        text,
                        font.clone(),
                        color,
                    );
                }
            });
        });

        if self.running {
            self.convert_progress_modal(ctx);
        }
    }
}

/// Load a CJK system font so Chinese labels render properly.
fn setup_fonts(ctx: &egui::Context) {
    let candidates = [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "C:\\Windows\\Fonts\\msyh.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert("cjk".to_owned(), egui::FontData::from_owned(bytes).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                if let Some(list) = fonts.families.get_mut(&family) {
                    list.push("cjk".to_owned());
                }
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
}

fn main() -> eframe::Result<()> {
    // Windows: opt into the system app mode before any menu is
    // created, so menus render in the system theme.
    #[cfg(target_os = "windows")]
    windows_apply_app_mode();

    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).ok();
    // Windows shows the app name and version in the title bar;
    // macOS keeps the title empty (traffic lights only).
    let title = if cfg!(target_os = "windows") {
        format!("AudioConvert {}", env!("CARGO_PKG_VERSION"))
    } else {
        String::new()
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([920.0, 600.0])
            .with_min_inner_size([620.0, 420.0])
            .with_icon(icon.unwrap_or_default())
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "AudioConvert",
        options,
        Box::new(|cc| {
            setup_fonts(&cc.egui_ctx);
            Ok(Box::new(AudioConvertApp::default()))
        }),
    )
}

/// The About window: frameless, custom header with a hand-drawn ×,
/// content optically centered.
fn show_about_window(ctx: &egui::Context, show_about: &mut bool, lang: Lang) {
    let mut open = *show_about;
    let mut close_requested = false;
    egui::Window::new(t(lang, "关于 AudioConvert", "About AudioConvert"))
        .id(egui::Id::new("about_window"))
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .title_bar(false)
        .fixed_size(egui::Vec2::new(480.0, 500.0))
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.allocate_ui_with_layout(
                egui::Vec2::new(ui.available_width(), 40.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.label(egui::RichText::new(t(lang, "关于 AudioConvert", "About AudioConvert")).size(14.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (button_rect, response) = ui
                            .allocate_exact_size(egui::Vec2::splat(30.0), egui::Sense::click());
                        if response.hovered() {
                            ui.painter().rect_filled(
                                button_rect,
                                4.0,
                                ui.visuals().widgets.hovered.bg_fill,
                            );
                        }
                        let center = button_rect.center();
                        let half = 5.0;
                        let stroke = egui::Stroke::new(1.5_f32, ui.visuals().text_color());
                        ui.painter().line_segment(
                            [center + egui::Vec2::splat(-half), center + egui::Vec2::splat(half)],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [
                                center + egui::Vec2::new(-half, half),
                                center + egui::Vec2::new(half, -half),
                            ],
                            stroke,
                        );
                        if response.clicked() {
                            close_requested = true;
                        }
                    });
                },
            );
            // Fixed hairline, not ui.separator() — inside a top-down
            // layout the stock separator eats all remaining space
            // and pushes the content to the bottom.
            let (line_rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), 9.0),
                egui::Sense::hover(),
            );
            let y = line_rect.center().y;
            ui.painter().line_segment(
                [egui::pos2(line_rect.left(), y), egui::pos2(line_rect.right(), y)],
                egui::Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color),
            );
            let body_size = ui.available_size();
            ui.allocate_ui_with_layout(
                body_size,
                egui::Layout::top_down(egui::Align::Center).with_main_align(egui::Align::Center),
                |ui| {
                    ui.add_space(10.0);
                    about_icon(ui);
                    ui.add_space(20.0);
                    ui.label(egui::RichText::new("AudioConvert").size(22.0));
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(match lang {
                                Lang::Zh => format!("版本 {}", env!("CARGO_PKG_VERSION")),
                                Lang::En => format!("Version {}", env!("CARGO_PKG_VERSION")),
                            }).size(16.0),
                    );
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new(t(lang, "任何音频格式，转成任何音频格式。", "Any audio format in, any audio format out.")).size(13.5).weak(),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            t(lang, "20 种输出格式，标签与封面可保留可清空\n由随包 ffmpeg 解码与编码", "20 output formats — tags and cover art kept or stripped, your choice\nPowered by the bundled ffmpeg"),
                        )
                        .size(12.5)
                        .weak(),
                    );
                    ui.add_space(24.0);
                    ui.label(egui::RichText::new("By Mr.Ben").size(16.0));
                    ui.add_space(2.0);
                    ui.hyperlink_to(
                        egui::RichText::new("https://mrben.net").size(16.0),
                        "https://mrben.net",
                    );
                    ui.add_space(10.0);
                },
            );
        });
    if close_requested {
        open = false;
    }
    *show_about = open;
}

/// Miniature of the app icon, painted for the About window: blue
/// rounded square with two white arrows trading places.
fn about_icon(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(88.0, 88.0), egui::Sense::hover());
    // Inset to match the app icon's grid margins (the Dock icon's
    // body occupies the middle ~80% of its tile).
    let rect = rect.shrink(9.0);
    let p = ui.painter();
    p.rect_filled(
        rect,
        egui::CornerRadius::same(20),
        egui::Color32::from_rgb(30, 120, 200),
    );
    let c = rect.center();
    let white = egui::Color32::WHITE;
    // Two arc arrows circling a quaver (the app icon's design):
    // the top arc's head points down-right, the bottom's up-left.
    let r = 27.4_f32;
    let arc_stroke = egui::Stroke::new(3.2_f32, white);
    for (a0, a1) in [(200.0_f32, 340.0_f32), (20.0_f32, 160.0_f32)] {
        let pts: Vec<egui::Pos2> = (0..=24)
            .map(|i| {
                let a = (a0 + (a1 - a0) * (i as f32 / 24.0)).to_radians();
                c + egui::vec2(r * a.cos(), r * a.sin())
            })
            .collect();
        p.add(egui::Shape::line(pts, arc_stroke));
        let a = a1.to_radians();
        let pos = c + egui::vec2(r * a.cos(), r * a.sin());
        let dir = egui::vec2(-a.sin(), a.cos());
        let perp = egui::vec2(dir.y, -dir.x);
        p.add(egui::Shape::convex_polygon(
            vec![
                pos + dir * 5.2,
                pos - dir * 1.5 + perp * 2.8,
                pos - dir * 1.5 - perp * 2.8,
            ],
            white,
            egui::Stroke::NONE,
        ));
    }
    // The quaver: ellipse head, stem and flag.
    let head_c = c + egui::vec2(-2.0, 2.0);
    p.add(egui::Shape::ellipse_filled(
        head_c,
        egui::vec2(7.5, 5.6),
        white,
    ));
    let stem_x = head_c.x + 6.3;
    let stem_top = head_c.y - 20.0;
    p.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(stem_x - 1.4, stem_top),
            egui::pos2(stem_x + 1.4, head_c.y - 0.5),
        ),
        0.0,
        white,
    );
    p.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(stem_x + 1.4, stem_top),
            egui::pos2(stem_x + 11.0, stem_top + 4.0),
            egui::pos2(stem_x + 9.6, stem_top + 11.5),
            egui::pos2(stem_x + 1.4, stem_top + 9.0),
        ],
        white,
        egui::Stroke::NONE,
    ));
}
