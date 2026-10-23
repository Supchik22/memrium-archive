//! Memrium Archive: look into an archive, take files out of it, or pack files into one. On Chebrium; the work is done by
//! `memrium_de::archive` on a thread of its own, so the window stays alive and the job can be stopped.
//!
//!   memrium_archive <archive>              browse it
//!   memrium_archive --extract <archive>    take everything out beside it (into a folder named like the archive)
//!   memrium_archive --compress <files...>  pack these (a dialog asks for the kind and the name)

use chebrium::app::{self, App, Config, Host};
use chebrium::draw::Align;
use chebrium::input::{Key, KeyMods};
use chebrium::ui::{inside, Ui};
use memrium_de::archive::{self, Entry, Format, Progress, Summary, CREATE};
use memrium_de::fsops::{self, human_size};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{channel, Receiver},
    Arc,
};
use std::time::{Duration, Instant};

const PAD: f32 = 24.0;
/// the header (icon, name, facts)
const HEAD_H: f32 = 84.0;
/// the column heads of the list
const COLS_H: f32 = 34.0;
const ROW_H: f32 = 32.0;
const BAR_H: f32 = 92.0;
const SCROLL_KEY: u64 = 90_000;

enum Mode {
    /// nothing was asked for
    Empty,
    Browse,
    Compress,
}

/// What a job reports.
enum Msg {
    Progress(Progress),
    Done(Result<Summary, String>),
}

struct Job {
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    /// what the whole job is expected to come to (bytes), to show how far it is
    total: u64,
    progress: Progress,
    what: &'static str,
}

/// One row of the folder being looked at.
#[derive(Clone)]
struct Row {
    /// the name shown
    name: String,
    /// the full name inside the archive
    full: String,
    is_dir: bool,
    size: u64,
    modified: Option<u64>,
}

struct ArchiveApp {
    mode: Mode,
    archive: PathBuf,
    format: Option<Format>,
    entries: Vec<Entry>,
    error: String,
    /// the folder inside the archive that is shown ("" is the top)
    cwd: String,
    rows: Vec<Row>,
    sel: Vec<String>,
    scroll: f32,
    /// where the files go (Browse) / the folder the new archive is made in (Compress)
    dest: String,
    // compress
    sources: Vec<PathBuf>,
    kind: usize,
    name: String,
    job: Option<Job>,
    /// the result of the last job: a message, and the folder or file to show
    note: String,
    shown: Option<PathBuf>,
    waker: Option<app::Waker>,
    /// start extracting everything as soon as the window is up (`--extract`)
    auto: bool,
    dirty: bool,
    /// the window is to close (the job is done and the "Done" button was pressed)
    want_quit: bool,
}

/// The archive's name without its ending (`photos.tar.gz` -> `photos`).
fn stem_of(path: &Path) -> String {
    let n = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    match Format::detect(path) {
        Some(f) => {
            let ext = format!(".{}", f.extension());
            if n.to_lowercase().ends_with(&ext) {
                n[..n.len() - ext.len()].to_string()
            } else {
                n.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(n)
            }
        }
        None => n,
    }
}

impl ArchiveApp {
    fn new(args: Vec<String>) -> ArchiveApp {
        let mut a = ArchiveApp { mode: Mode::Empty, archive: PathBuf::new(), format: None, entries: Vec::new(), error: String::new(), cwd: String::new(), rows: Vec::new(), sel: Vec::new(), scroll: 0.0, dest: String::new(), sources: Vec::new(), kind: 0, name: String::new(), job: None, note: String::new(), shown: None, waker: None, auto: false, dirty: true, want_quit: false };
        let mut it = args.into_iter().peekable();
        match it.peek().map(|s| s.as_str()) {
            Some("--compress") => {
                it.next();
                a.sources = it.map(PathBuf::from).filter(|p| p.exists()).collect();
                if !a.sources.is_empty() {
                    a.mode = Mode::Compress;
                    a.name = archive::default_name(&a.sources);
                    a.dest = a.sources[0].parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| fsops::home().to_string_lossy().to_string());
                }
            }
            Some("--extract") => {
                it.next();
                if let Some(p) = it.next() {
                    a.auto = true;
                    a.open(PathBuf::from(p));
                }
            }
            Some(_) => {
                if let Some(p) = it.next() {
                    a.open(PathBuf::from(p));
                }
            }
            None => {}
        }
        a
    }

    fn open(&mut self, path: PathBuf) {
        self.mode = Mode::Browse;
        self.format = Format::detect(&path);
        match archive::list(&path) {
            Ok(e) => {
                self.entries = e;
                self.error.clear();
            }
            Err(e) => {
                self.entries.clear();
                self.error = e;
            }
        }
        // by default the files go beside the archive, into a folder of their own
        let parent = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        self.dest = fsops::unique_path(&parent, &stem_of(&path)).to_string_lossy().to_string();
        self.archive = path;
        self.dirty = true;
    }

    fn rebuild(&mut self) {
        let prefix = if self.cwd.is_empty() { String::new() } else { format!("{}/", self.cwd) };
        let mut map: BTreeMap<String, Row> = BTreeMap::new();
        for e in &self.entries {
            let Some(rest) = e.path.strip_prefix(&prefix) else { continue };
            if rest.is_empty() {
                continue;
            }
            match rest.split_once('/') {
                Some((dir, _)) => {
                    map.entry(dir.to_string()).or_insert(Row { name: dir.to_string(), full: format!("{prefix}{dir}"), is_dir: true, size: 0, modified: None });
                }
                None => {
                    map.insert(rest.to_string(), Row { name: rest.to_string(), full: e.path.clone(), is_dir: e.is_dir, size: e.size, modified: e.modified });
                }
            }
        }
        let mut rows: Vec<Row> = map.into_values().collect();
        rows.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        self.rows = rows;
        self.dirty = false;
    }

    /// Start a job on a thread. `work` gets a callback to report with (it returns false when the job is to stop).
    fn start(&mut self, what: &'static str, total: u64, work: impl FnOnce(&mut dyn FnMut(&Progress) -> bool) -> Result<Summary, String> + Send + 'static) {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (c2, waker) = (cancel.clone(), self.waker.clone());
        std::thread::Builder::new()
            .name("archive-job".into())
            .spawn(move || {
                let mut last = Instant::now() - Duration::from_secs(1);
                let result = work(&mut |p: &Progress| {
                    // reports are thinned out: a window cannot use more than a few a second
                    if last.elapsed() >= Duration::from_millis(60) {
                        last = Instant::now();
                        let _ = tx.send(Msg::Progress(p.clone()));
                        if let Some(w) = &waker {
                            w.wake();
                        }
                    }
                    !c2.load(Ordering::Relaxed)
                });
                let _ = tx.send(Msg::Done(result));
                if let Some(w) = &waker {
                    w.wake();
                }
            })
            .ok();
        self.note.clear();
        self.shown = None;
        self.job = Some(Job { rx, cancel, total, progress: Progress::default(), what });
    }

    fn extract(&mut self, only: Vec<String>) {
        if self.job.is_some() {
            return;
        }
        let (path, dest) = (self.archive.clone(), PathBuf::from(self.dest.trim()));
        if dest.as_os_str().is_empty() {
            self.note = "Choose a folder to put the files in".into();
            return;
        }
        let total: u64 = self.entries.iter().filter(|e| !e.is_dir && (only.is_empty() || only.iter().any(|o| e.path == *o || e.path.starts_with(&format!("{o}/"))))).map(|e| e.size).sum();
        let show = dest.clone();
        self.start("Extracting", total, move |cb| archive::extract(&path, &dest, &only, cb));
        self.shown = Some(show);
    }

    fn compress(&mut self) {
        if self.job.is_some() {
            return;
        }
        let format = CREATE[self.kind];
        let name = self.name.trim().to_string();
        if name.is_empty() || name.contains('/') {
            self.note = "Give the archive a name (no slashes)".into();
            return;
        }
        let dir = PathBuf::from(self.dest.trim());
        let out = fsops::unique_path(&dir, &format!("{name}.{}", format.extension()));
        let sources = self.sources.clone();
        let total: u64 = sources.iter().map(|s| size_of(s)).sum();
        let show = out.clone();
        self.start("Packing", total, move |cb| archive::create(format, &out, &sources, cb));
        self.shown = Some(show);
    }

    fn poll(&mut self) -> bool {
        let Some(job) = &mut self.job else { return false };
        let mut changed = false;
        let mut done = None;
        while let Ok(m) = job.rx.try_recv() {
            changed = true;
            match m {
                Msg::Progress(p) => job.progress = p,
                Msg::Done(r) => done = Some(r),
            }
        }
        if let Some(r) = done {
            let what = job.what;
            self.job = None;
            self.note = match r {
                Ok(s) => format!("{} {} file{}{} ({})", if what == "Packing" { "Packed" } else { "Extracted" }, s.files, if s.files == 1 { "" } else { "s" }, if s.skipped > 0 { format!(", {} left out", s.skipped) } else { String::new() }, human_size(s.bytes)),
                Err(e) => {
                    self.shown = None;
                    format!("Stopped: {e}")
                }
            };
        }
        changed
    }

    fn select(&mut self, full: &str, add: bool) {
        if add {
            if let Some(i) = self.sel.iter().position(|s| s == full) {
                self.sel.remove(i);
            } else {
                self.sel.push(full.to_string());
            }
        } else {
            self.sel = vec![full.to_string()];
        }
    }

    fn up(&mut self) {
        self.cwd = self.cwd.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
        self.sel.clear();
        self.dirty = true;
    }
}

fn size_of(p: &Path) -> u64 {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| size_of(&e.path())).sum()).unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

fn fit(ui: &mut Ui<'_>, text: &str, size: f32, bold: bool, max: f32) -> String {
    if ui.measure_raw(text, size, bold) <= max {
        return text.to_string();
    }
    let mut s = text.to_string();
    while !s.is_empty() && ui.measure_raw(&format!("{s}…"), size, bold) > max {
        s.pop();
    }
    format!("{s}…")
}

/// What the window looks like: an opaque page with cards on it (the glass of the other windows made the text hard to read
/// over a busy wallpaper).
fn page_color(dark: bool) -> [f32; 3] {
    if dark { [0.115, 0.115, 0.115] } else { [0.955, 0.960, 0.972] }
}

fn card_color(dark: bool) -> [f32; 3] {
    if dark { [0.165, 0.165, 0.165] } else { [1.0, 1.0, 1.0] }
}

/// A card: the colour of a surface and a hairline round it.
fn card(ui: &mut Ui<'_>, r: [f32; 4], radius: f32) {
    let ink = ui.ink;
    ui.fill(r, radius, ink, 0.09);
    ui.fill([r[0] + 1.0, r[1] + 1.0, r[2] - 2.0, r[3] - 2.0], radius - 1.0, card_color(ui.dark), 1.0);
}

/// A small pill with a word in it; returns its width.
fn chip(ui: &mut Ui<'_>, x: f32, y: f32, text: &str, tint: Option<[f32; 3]>) -> f32 {
    let w = ui.measure_raw(text, 11.5, true) + 16.0;
    let (ink, col) = (ui.ink, tint.unwrap_or(ui.ink));
    ui.fill([x, y, w, 20.0], 10.0, col, if tint.is_some() { 0.18 } else { 0.08 });
    ui.text_raw(x + 8.0, y + 14.5, text, 11.5, true, if tint.is_some() { col } else { ink }, if tint.is_some() { 1.0 } else { 0.62 }, Align::Left);
    w + 6.0
}

/// A section's small heading.
fn heading(ui: &mut Ui<'_>, x: f32, y: f32, text: &str) {
    let ink = ui.ink;
    ui.text(x, y, text, 11.5, true, ink, 0.50, Align::Left);
}

impl App for ArchiveApp {
    fn start(&mut self, waker: app::Waker) {
        self.waker = Some(waker);
    }

    fn background(&self, look: &chebrium::theme::Appearance) -> ([f32; 3], f32) {
        (page_color(look.dark), 1.0)
    }

    fn tick(&mut self, host: &mut Host) {
        if self.auto && self.job.is_none() && matches!(self.mode, Mode::Browse) && self.error.is_empty() {
            self.auto = false;
            self.extract(Vec::new());
            host.redraw = true;
        }
        if self.poll() {
            host.redraw = true;
        }
        if self.want_quit {
            host.quit = true;
        }
    }

    fn key(&mut self, key: Key, _mods: KeyMods, host: &mut Host) {
        host.redraw = true;
        match key {
            Key::Esc => {
                if let Some(j) = &self.job {
                    j.cancel.store(true, Ordering::Relaxed);
                }
            }
            Key::Backspace if matches!(self.mode, Mode::Browse) && !self.cwd.is_empty() => self.up(),
            _ => {}
        }
    }

    fn draw(&mut self, ui: &mut Ui<'_>, size: [f32; 2], host: &mut Host) {
        let [w, h] = size;
        let (ink, accent) = (ui.ink, ui.accent);
        if self.dirty && matches!(self.mode, Mode::Browse) {
            self.rebuild();
        }
        let title = match self.mode {
            Mode::Browse => self.archive.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            Mode::Compress => "New archive".to_string(),
            Mode::Empty => "Memrium Archive".to_string(),
        };
        host.title = Some(title);
        match self.mode {
            Mode::Empty => {
                let tile = [w / 2.0 - 36.0, h / 2.0 - 110.0, 72.0, 72.0];
                ui.fill(tile, 22.0, accent, 0.16);
                ui.glyph(w / 2.0, h / 2.0 - 74.0, 34.0, "file-archive", accent, 1.0);
                ui.text(w / 2.0, h / 2.0 - 4.0, "No archive open", 20.0, true, ink, 0.92, Align::Center);
                ui.text(w / 2.0, h / 2.0 + 24.0, "Open an archive from Files, or choose Compress on some files.", 13.0, false, ink, 0.55, Align::Center);
            }
            Mode::Browse => self.browse(ui, size),
            Mode::Compress => self.compress_view(ui, size),
        }
    }
}

impl ArchiveApp {
    /// The top of both views: the icon, the name, a few facts, and a line under it.
    fn header(&mut self, ui: &mut Ui<'_>, w: f32, title: &str, facts: &[(String, bool)]) {
        let (ink, accent) = (ui.ink, ui.accent);
        ui.fill([PAD, 18.0, 48.0, 48.0], 14.0, accent, 0.18);
        ui.glyph(PAD + 24.0, 42.0, 24.0, "file-archive", accent, 1.0);
        let t = fit(ui, title, 18.0, true, w - 2.0 * PAD - 62.0 - 330.0);
        ui.text_raw(PAD + 62.0, 36.0, &t, 18.0, true, ink, 0.96, Align::Left);
        let mut x = PAD + 62.0;
        for (f, tinted) in facts {
            x += chip(ui, x, 46.0, f, tinted.then_some(accent));
        }
        ui.fill([0.0, HEAD_H - 1.0, w, 1.0], 0.0, ink, 0.09);
    }

    fn browse(&mut self, ui: &mut Ui<'_>, size: [f32; 2]) {
        let [w, h] = size;
        let (ink, accent) = (ui.ink, ui.accent);
        let files = self.entries.iter().filter(|e| !e.is_dir).count();
        let total: u64 = self.entries.iter().map(|e| e.size).sum();
        let mut facts = vec![(self.format.map_or(String::new(), |f| f.label().to_string()), true), (format!("{files} file{}", if files == 1 { "" } else { "s" }), false)];
        if total > 0 {
            facts.push((human_size(total), false));
        }
        // where in the archive the list is
        if !self.cwd.is_empty() {
            facts.push((format!("/{}", self.cwd), false));
        }
        facts.retain(|f| !f.0.is_empty());
        let name = self.archive.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();

        // ---- the list, on a card between the header and the bottom bar
        let card_r = [PAD, HEAD_H + 14.0, w - 2.0 * PAD, (h - HEAD_H - BAR_H - 28.0).max(120.0)];
        card(ui, card_r, 14.0);
        let list = [card_r[0] + 1.0, card_r[1] + COLS_H, card_r[2] - 2.0, card_r[3] - COLS_H - 6.0];
        let n = self.rows.len();
        let max_scroll = (n as f32 * ROW_H + 8.0 - list[3]).max(0.0);
        if ui.scroll != 0.0 && inside(ui.mouse, list) {
            self.scroll += ui.scroll;
        }
        self.scroll = self.scroll.clamp(0.0, max_scroll);
        let scroll = ui.ease(SCROLL_KEY, self.scroll, 18.0);
        let first = (scroll / ROW_H).floor().max(0.0) as usize;
        let last = (((scroll + list[3]) / ROW_H).ceil() as usize + 1).min(n);
        let (mut open_dir, mut clicked_row) = (None, false);
        ui.push_clip(list);
        for i in first..last {
            let row = self.rows[i].clone();
            let r = [list[0] + 6.0, list[1] - scroll + 4.0 + i as f32 * ROW_H, list[2] - 12.0, ROW_H - 2.0];
            let hov = ui.hit(r) && inside(ui.mouse, list);
            let selected = self.sel.contains(&row.full);
            let wash = ui.ease(30_000 + i as u64, if hov { 1.0 } else { 0.0 }, 22.0);
            if selected {
                ui.fill(r, 8.0, accent, 0.26);
            } else if wash > 0.01 {
                ui.fill(r, 8.0, ink, 0.07 * wash);
            }
            let cy = r[1] + r[3] / 2.0;
            let kind = fsops::kind_of(&row.name, row.is_dir);
            ui.glyph(r[0] + 20.0, cy, 18.0, kind.icon(), kind.tint(accent), 1.0);
            let right = r[0] + r[2] - 16.0;
            let name = fit(ui, &row.name, 13.5, false, right - 110.0 - 150.0 - r[0] - 56.0);
            ui.text_raw(r[0] + 42.0, cy + 4.8, &name, 13.5, row.is_dir, ink, 0.95, Align::Left);
            if let Some(t) = row.modified {
                ui.text_raw(right - 110.0 - 150.0 + 16.0, cy + 4.5, &fsops::format_time(t), 12.0, false, ink, 0.55, Align::Left);
            }
            if row.is_dir {
                ui.glyph(right - 4.0, cy, 14.0, "chevron-right", ink, 0.35);
            } else {
                ui.text_raw(right, cy + 4.5, &human_size(row.size), 12.0, false, ink, 0.55, Align::Right);
            }
            if ui.click && hov {
                clicked_row = true;
                if ui.double && row.is_dir {
                    open_dir = Some(row.full.clone());
                } else {
                    self.select(&row.full, ui.mods.ctrl);
                }
            }
        }
        ui.pop_clip();
        if ui.click && inside(ui.mouse, list) && !clicked_row {
            self.sel.clear();
        }
        if let Some(d) = open_dir {
            self.cwd = d;
            self.sel.clear();
            self.scroll = 0.0;
            self.dirty = true;
        }
        if n == 0 {
            let msg = if self.error.is_empty() { "This folder is empty" } else { "Cannot open this archive" };
            ui.text(card_r[0] + card_r[2] / 2.0, list[1] + 56.0, msg, 14.0, false, ink, 0.5, Align::Center);
            if !self.error.is_empty() {
                let e = fit(ui, &self.error, 12.0, false, card_r[2] - 60.0);
                ui.text_raw(card_r[0] + card_r[2] / 2.0, list[1] + 82.0, &e, 12.0, false, ink, 0.4, Align::Center);
            }
        }
        // the column heads, on the card's top strip (over whatever scrolled under it)
        ui.fill([card_r[0] + 1.0, card_r[1] + 1.0, card_r[2] - 2.0, COLS_H - 1.0], 13.0, card_color(ui.dark), 1.0);
        ui.fill([card_r[0] + 14.0, card_r[1] + COLS_H - 1.0, card_r[2] - 28.0, 1.0], 0.0, ink, 0.09);
        let right = card_r[0] + card_r[2] - 6.0 - 16.0;
        ui.text(card_r[0] + 48.0, card_r[1] + 20.0, "Name", 11.5, true, ink, 0.5, Align::Left);
        ui.text(right - 110.0 - 150.0 + 22.0, card_r[1] + 20.0, "Modified", 11.5, true, ink, 0.5, Align::Left);
        ui.text(right + 6.0, card_r[1] + 20.0, "Size", 11.5, true, ink, 0.5, Align::Right);

        // ---- the header, over the list
        ui.fill([0.0, 0.0, w, HEAD_H], 0.0, page_color(ui.dark), 1.0);
        self.header(ui, w, &name, &facts);
        // actions at the top right: the main one stands out
        let busy = self.job.is_some();
        let mut x = w - PAD;
        if !busy {
            let all = [x - 128.0, 26.0, 128.0, 34.0];
            if ui.button(all, "Extract all", self.sel.is_empty()) {
                self.extract(Vec::new());
            }
            x -= 138.0;
            if !self.sel.is_empty() {
                let b = [x - 190.0, 26.0, 190.0, 34.0];
                if ui.button(b, &format!("Extract selected ({})", self.sel.len()), true) {
                    let only = self.sel.clone();
                    self.extract(only);
                }
                x -= 200.0;
            }
            if !self.cwd.is_empty() {
                let b = [x - 84.0, 26.0, 84.0, 34.0];
                if ui.button(b, "      Up", false) {
                    self.up();
                }
                ui.glyph(b[0] + 22.0, b[1] + 17.0, 14.0, "arrow-up", ink, 0.9);
            }
        }

        self.bottom_bar(ui, size, "Extract");
        let _ = (card_r, accent);
    }

    /// The bottom: where the files go (or where the archive is saved), what is happening, and the way out.
    fn bottom_bar(&mut self, ui: &mut Ui<'_>, size: [f32; 2], verb: &str) {
        let [w, h] = size;
        let (ink, accent) = (ui.ink, ui.accent);
        let bar = [0.0, h - BAR_H, w, BAR_H];
        ui.fill(bar, 0.0, card_color(ui.dark), 1.0);
        ui.fill([0.0, bar[1], w, 1.0], 0.0, ink, 0.09);
        if let Some(job) = &self.job {
            // a job: how far it is, what it is on, and a button to stop it
            let frac = if job.total > 0 { (job.progress.bytes as f32 / job.total as f32).clamp(0.0, 1.0) } else { 0.0 };
            let track = [PAD, bar[1] + 24.0, w - 2.0 * PAD - 120.0, 8.0];
            ui.fill(track, 4.0, ink, 0.10);
            // without a total (a single compressed file) a short bar moves along with the work
            let (fill_w, fill_x) = if job.total > 0 { (track[2] * frac, track[0]) } else { (track[2] * 0.25, track[0] + track[2] * 0.75 * ((job.progress.bytes / 200_000) % 40) as f32 / 40.0) };
            ui.fill([fill_x, track[1], fill_w.max(8.0), track[3]], 4.0, accent, 1.0);
            let what = fit(ui, &format!("{} {}…", job.what, job.progress.current), 12.5, false, w - 2.0 * PAD - 130.0);
            ui.text_raw(PAD, bar[1] + 58.0, &what, 12.5, false, ink, 0.75, Align::Left);
            ui.text_raw(PAD, bar[1] + 76.0, &format!("{} files · {}", job.progress.entries, human_size(job.progress.bytes)), 11.5, false, ink, 0.5, Align::Left);
            if ui.button([w - PAD - 100.0, bar[1] + 16.0, 100.0, 34.0], "Cancel", false) {
                job.cancel.store(true, Ordering::Relaxed);
            }
            return;
        }
        // the folder
        ui.glyph(PAD + 8.0, bar[1] + 26.0, 16.0, "folder", ink, 0.55);
        ui.text(PAD + 26.0, bar[1] + 30.0, if verb == "Extract" { "Extract to" } else { "Save in" }, 12.0, true, ink, 0.55, Align::Left);
        let lw = if verb == "Extract" { 84.0 } else { 62.0 };
        let mut dest = self.dest.clone();
        let field = [PAD + 34.0 + lw, bar[1] + 12.0, w - 2.0 * PAD - 34.0 - lw, 30.0];
        // (a long path is cut at the field's edge, not drawn over the window)
        ui.push_clip([field[0] - 3.0, field[1] - 3.0, field[2] + 6.0, field[3] + 6.0]);
        ui.text_field(field, "dest", &mut dest);
        ui.pop_clip();
        self.dest = dest;
        // the result of the last job, and what to do with it
        let done = !self.note.is_empty() && !self.note.starts_with("Stopped") && self.shown.is_some();
        let tint = if self.note.starts_with("Stopped") || self.note.starts_with("Choose") || self.note.starts_with("Give") { [0.95, 0.45, 0.35] } else { ink };
        let msg_w = w - 2.0 * PAD - if verb == "Compress" { 320.0 } else { 150.0 };
        let msg = fit(ui, &self.note, 12.5, false, msg_w.max(80.0));
        ui.text_raw(PAD, bar[1] + 70.0, &msg, 12.5, false, tint, if self.note.is_empty() { 0.0 } else { 0.85 }, Align::Left);
        let mut x = w - PAD;
        if verb == "Compress" {
            let b = [x - 160.0, bar[1] + 50.0, 160.0, 34.0];
            let label = if done { "Done" } else { "Create archive" };
            if ui.button(b, label, true) {
                if done {
                    self.want_quit = true;
                } else {
                    self.compress();
                }
            }
            x -= 172.0;
        }
        if done {
            let b = [x - 130.0, bar[1] + 50.0, 130.0, 34.0];
            if ui.button(b, "Show in Files", false) {
                if let Some(p) = self.shown.clone() {
                    let folder = if p.is_dir() { p } else { p.parent().map(|d| d.to_path_buf()).unwrap_or(p) };
                    let mut c = std::process::Command::new(fsops::own_program("memrium_files"));
                    c.arg(folder);
                    let _ = c.spawn();
                }
            }
        }
    }

    fn compress_view(&mut self, ui: &mut Ui<'_>, size: [f32; 2]) {
        let [w, h] = size;
        let (ink, accent) = (ui.ink, ui.accent);
        let total: u64 = self.sources.iter().map(|s| size_of(s)).sum();
        let facts = vec![(format!("{} item{}", self.sources.len(), if self.sources.len() == 1 { "" } else { "s" }), false), (human_size(total), false)];
        self.header(ui, w, "New archive", &facts);

        // ---- the kind: one card for each, with a word about it
        let mut y = HEAD_H + 22.0;
        heading(ui, PAD, y, "Kind");
        y += 12.0;
        let notes = ["Opens everywhere", "The Linux usual", "Smallest, slower", "Fast and small", "Older, slower", "No compression", "Best on Windows"];
        let (cols, gap) = (4usize, 10.0);
        let cw = (w - 2.0 * PAD - gap * (cols as f32 - 1.0)) / cols as f32;
        let ch = 56.0;
        for (i, f) in CREATE.iter().enumerate() {
            let (c, r) = (i % cols, i / cols);
            let rect = [PAD + c as f32 * (cw + gap), y + r as f32 * (ch + gap), cw, ch];
            let on = self.kind == i;
            let hov = ui.hit(rect);
            let t = ui.ease(50_000 + i as u64, if on { 1.0 } else { 0.0 }, 18.0);
            let wash = ui.ease(51_000 + i as u64, if hov { 1.0 } else { 0.0 }, 22.0);
            card(ui, rect, 12.0);
            if wash > 0.01 && !on {
                ui.fill(rect, 12.0, ink, 0.05 * wash);
            }
            if t > 0.01 {
                // the chosen one: an accent ring and a tint
                ui.fill(rect, 12.0, accent, 0.55 * t);
                ui.fill([rect[0] + 2.0, rect[1] + 2.0, rect[2] - 4.0, rect[3] - 4.0], 10.0, card_color(ui.dark), 1.0);
                ui.fill([rect[0] + 2.0, rect[1] + 2.0, rect[2] - 4.0, rect[3] - 4.0], 10.0, accent, 0.12 * t);
            }
            ui.text_raw(rect[0] + 14.0, rect[1] + 24.0, f.label(), 14.0, true, if on { accent } else { ink }, 0.95, Align::Left);
            ui.text(rect[0] + 14.0, rect[1] + 43.0, notes[i], 11.5, false, ink, 0.52, Align::Left);
            if ui.click && hov {
                self.kind = i;
            }
        }
        y += 2.0 * ch + gap + 26.0;

        // ---- the name, and the ending it gets
        heading(ui, PAD, y, "Name");
        y += 12.0;
        let ext = format!(".{}", CREATE[self.kind].extension());
        let ew = ui.measure_raw(&ext, 13.0, true) + 22.0;
        let mut name = self.name.clone();
        ui.text_field([PAD, y, w - 2.0 * PAD - ew - 8.0, 36.0], "name", &mut name);
        self.name = name;
        ui.fill([w - PAD - ew, y, ew, 36.0], 9.0, ink, 0.08);
        ui.text_raw(w - PAD - ew / 2.0, y + 23.0, &ext, 13.0, true, ink, 0.7, Align::Center);
        y += 36.0 + 24.0;

        // ---- what goes in
        heading(ui, PAD, y, "Contents");
        y += 12.0;
        let room = h - BAR_H - y - 14.0;
        let rows = ((room - 8.0) / 28.0).floor().max(1.0) as usize;
        let shown = self.sources.len().min(rows);
        let extra = self.sources.len().saturating_sub(shown);
        let list_h = shown as f32 * 28.0 + 8.0 + if extra > 0 { 22.0 } else { 0.0 };
        card(ui, [PAD, y, w - 2.0 * PAD, list_h.min(room.max(40.0))], 12.0);
        let srcs = self.sources.clone();
        for (i, s) in srcs.iter().take(shown).enumerate() {
            let ry = y + 4.0 + i as f32 * 28.0;
            let nm = s.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let kind = fsops::kind_of(&nm, s.is_dir());
            ui.glyph(PAD + 22.0, ry + 14.0, 16.0, kind.icon(), kind.tint(accent), 1.0);
            let nm = fit(ui, &nm, 13.0, false, w - 2.0 * PAD - 150.0);
            ui.text_raw(PAD + 42.0, ry + 18.5, &nm, 13.0, false, ink, 0.9, Align::Left);
            ui.text_raw(w - PAD - 16.0, ry + 18.5, &human_size(size_of(s)), 12.0, false, ink, 0.5, Align::Right);
        }
        if extra > 0 {
            ui.text_raw(PAD + 18.0, y + 4.0 + shown as f32 * 28.0 + 15.0, &format!("and {extra} more…"), 12.0, false, ink, 0.5, Align::Left);
        }

        self.bottom_bar(ui, size, "Compress");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // the dialog for a new archive has more to show than the list of an archive
    let packing = args.first().is_some_and(|a| a == "--compress");
    let (w, h) = if packing { (700, 660) } else { (820, 580) };
    app::run(Config::new("Memrium Archive", "memrium-archive").size(w, h).min_size(600, 460), ArchiveApp::new(args));
}
