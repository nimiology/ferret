use std::process::Command;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eframe::egui::{
    self, text::LayoutJob, Align2, Color32, FontId, Key, Modifiers, Pos2, Rect, Rounding, Sense,
    Stroke, TextFormat, UiBuilder, Vec2,
};
use ferret_core::protocol::{self, HitOut, KindFilter, Op, Response, SortArg, Status};
use ferret_core::{snapshot, Hit, Index, Kind, Query, SearchResult};

use crate::config::{self, AppConfig};
use crate::shell::{self, Cmd, Shell};
use crate::theme::{self, Theme};

const ROW_H: f32 = 36.0;
const LIMIT: usize = 200;
const RECENT_LIMIT: usize = 6;
const W: f32 = 720.0;
const FDA_URL: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

/// Screen 03a "Try" cards.
const TRIES: [(&str, &str); 3] = [
    ("*.pdf", "All PDF files"),
    ("size:>1GB", "Files over 1 GB"),
    ("mod:<7d", "Changed this week"),
];
/// Screen 03b hints, in the syntax ferret_core::parse accepts (`in:` / `!word`, not the mockup's `path:` / `-word`).
const SYNTAX: [(&str, &str); 6] = [
    ("ext:pdf", "File extension"),
    ("size:>100MB", "Size, with < or >"),
    ("mod:<7d", "Modified within 7 days"),
    ("in:~/Projects", "Inside a folder"),
    ("\"exact name\"", "Exact phrase"),
    ("!node_modules", "Exclude a term"),
];

/// Loaded at most once, and only after the daemon was unreachable. None = no snapshot on disk.
type Snap = Arc<OnceLock<Option<Index>>>;

/// [all, files, folders] totals for the tab bar.
type Counts = [usize; 3];

/// (generation, query words, (result, came from daemon, tab counts) or error)
type Reply = (
    u64,
    Vec<String>,
    Result<(SearchResult, bool, Counts), String>,
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    All,
    Files,
    Folders,
    Recent,
    Large,
}

const TABS: [Tab; 5] = [Tab::All, Tab::Files, Tab::Folders, Tab::Recent, Tab::Large];

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::All => "All",
            Tab::Files => "Files",
            Tab::Folders => "Folders",
            Tab::Recent => "Recent",
            Tab::Large => "Large",
        }
    }

    fn sort_label(self) -> &'static str {
        match self {
            Tab::Recent => "Modified ↓",
            Tab::Large => "Size ↓",
            _ => "Relevance",
        }
    }

    fn count(self, c: &Counts) -> usize {
        match self {
            Tab::All | Tab::Recent => c[0],
            Tab::Files | Tab::Large => c[1],
            Tab::Folders => c[2],
        }
    }
}

/// What to ask the daemon for: (query, kind, sort, limit). Empty query = the "Recent" list of screen 03a.
pub fn plan(tab: Tab, query: &str) -> (String, KindFilter, SortArg, usize) {
    if query.trim().is_empty() {
        return (
            "mod:<7d".into(),
            KindFilter::File,
            SortArg::Mtime,
            RECENT_LIMIT,
        );
    }
    let (kind, sort) = match tab {
        Tab::All => (KindFilter::All, SortArg::Score),
        Tab::Files => (KindFilter::File, SortArg::Score),
        Tab::Folders => (KindFilter::Dir, SortArg::Score),
        Tab::Recent => (KindFilter::All, SortArg::Mtime),
        Tab::Large => (KindFilter::File, SortArg::Size),
    };
    (query.to_string(), kind, sort, LIMIT)
}

#[derive(Debug, PartialEq, Eq)]
pub enum View {
    Fda,
    Empty,
    NoResults,
    Results,
}

/// `pending`: a search is in flight, so "no hits" doesn't mean "no results" yet.
pub fn pick_view(fda: bool, query: &str, hits: usize, pending: bool, error: bool) -> View {
    if fda {
        View::Fda
    } else if query.trim().is_empty() {
        View::Empty
    } else if hits == 0 && !pending && !error {
        View::NoResults
    } else {
        View::Results
    }
}

/// Large tab bar width, relative to the biggest shown result.
pub fn bar_ratio(size: u64, max: u64) -> f32 {
    if max == 0 {
        0.0
    } else {
        (size as f64 / max as f64).clamp(0.0, 1.0) as f32
    }
}

pub struct App {
    ctx: egui::Context,
    snap: Snap,
    /// Latest daemon status; None = daemon unreachable.
    status: Arc<Mutex<Option<Status>>>,
    live: bool,
    query: String,
    tab: Tab,
    generation: u64,
    received: u64,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    hits: Vec<Hit>,
    words: Vec<String>,
    counts: Counts,
    total: usize,
    took_us: u64,
    error: Option<String>,
    selected: usize,
    /// Keyboard focus on a "Try" card (screen 03a), cycled with Tab.
    try_focus: Option<usize>,
    scroll_to: Option<f32>,
    last_offset: f32,
    last_view_h: f32,
    fda: bool,
    shell: Option<Shell>,
    was_focused: bool,
    positioned: bool,
    cfg: AppConfig,
    settings_open: bool,
    recording: bool,
    settings_msg: Option<String>,
    new_excluded: String,
    excluded_sel: Option<usize>,
}

impl App {
    pub fn new(ctx: egui::Context, shell: Option<Shell>) -> Self {
        let status = Arc::new(Mutex::new(None));
        let (st, c) = (status.clone(), ctx.clone());
        std::thread::spawn(move || loop {
            let s = protocol::request(Op::Status).ok().and_then(|r| r.status);
            *st.lock().unwrap() = s;
            c.request_repaint();
            std::thread::sleep(Duration::from_secs(2));
        });
        let (tx, rx) = channel();
        let mut app = Self {
            ctx,
            snap: Arc::new(OnceLock::new()),
            status,
            live: false,
            query: String::new(),
            tab: Tab::All,
            generation: 0,
            received: 0,
            tx,
            rx,
            hits: Vec::new(),
            words: Vec::new(),
            counts: [0; 3],
            total: 0,
            took_us: 0,
            error: None,
            selected: 0,
            try_focus: None,
            scroll_to: None,
            last_offset: 0.0,
            last_view_h: 0.0,
            fda: config::should_prompt_fda(),
            shell,
            was_focused: false,
            positioned: false,
            cfg: config::load(),
            settings_open: false,
            recording: false,
            settings_msg: None,
            new_excluded: String::new(),
            excluded_sel: None,
        };
        app.run_search(); // fills the Recent list
        app
    }

    fn run_search(&mut self) {
        self.generation += 1;
        let (q, kind, sort, limit) = plan(self.tab, &self.query);
        let counts_too = !self.query.trim().is_empty();
        let (gen, snap, tx, ctx) = (
            self.generation,
            self.snap.clone(),
            self.tx.clone(),
            self.ctx.clone(),
        );
        std::thread::spawn(move || {
            let reply = match ferret_core::parse(&q) {
                Ok(parsed) => {
                    // One request: the reply's per-kind counts fill every tab.
                    let res = fetch(&q, &parsed, kind, sort, limit, &snap).map(|(r, live)| {
                        let counts = if counts_too {
                            tab_counts(r.by_kind, parsed.kind)
                        } else {
                            [0; 3]
                        };
                        (r, live, counts)
                    });
                    (gen, parsed.words, res)
                }
                Err(e) => (gen, Vec::new(), Err(e.to_string())),
            };
            let _ = tx.send(reply);
            ctx.request_repaint();
        });
    }

    fn poll(&mut self) {
        while let Ok((gen, words, res)) = self.rx.try_recv() {
            if gen != self.generation {
                continue; // stale
            }
            self.received = gen;
            match res {
                Ok((r, live, counts)) => {
                    self.live = live;
                    self.hits = r.hits;
                    self.total = r.total;
                    self.took_us = r.took_us;
                    self.counts = counts;
                    self.words = words;
                    self.error = None;
                    self.selected = 0;
                    self.scroll_to = Some(0.0);
                }
                Err(e) => self.error = Some(e), // keep old results
            }
        }
        while let Some(cmd) = self.shell.as_ref().and_then(|s| s.rx.try_recv().ok()) {
            match cmd {
                Cmd::Shown => self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus),
                Cmd::Settings => self.settings_open = true,
            }
        }
    }

    fn view(&self) -> View {
        pick_view(
            self.fda,
            &self.query,
            self.hits.len(),
            self.received != self.generation,
            self.error.is_some(),
        )
    }

    fn set_query(&mut self, q: &str) {
        self.query = q.to_string();
        self.try_focus = None;
        self.run_search();
    }

    fn set_tab(&mut self, tab: Tab) {
        if tab != self.tab {
            self.tab = tab;
            self.run_search();
        }
    }

    fn selected_path(&self) -> Option<&str> {
        self.hits.get(self.selected).map(|h| h.path.as_str())
    }

    fn open(&self, reveal: bool) {
        if let Some(p) = self.selected_path() {
            let mut cmd = Command::new("open");
            if reveal {
                cmd.arg("-R");
            }
            let _ = cmd.arg(p).spawn();
        }
    }

    fn move_sel(&mut self, down: bool) {
        if self.hits.is_empty() {
            return;
        }
        self.try_focus = None;
        self.selected = if down {
            (self.selected + 1).min(self.hits.len() - 1)
        } else {
            self.selected.saturating_sub(1)
        };
        let top = self.selected as f32 * ROW_H;
        if top < self.last_offset {
            self.scroll_to = Some(top);
        } else if top + ROW_H > self.last_offset + self.last_view_h {
            self.scroll_to = Some(top + ROW_H - self.last_view_h);
        }
    }

    fn dismiss_fda(&mut self, open_settings: bool) {
        if open_settings {
            let _ = Command::new("open").arg(FDA_URL).spawn();
        }
        config::dismiss_fda();
        self.fda = false;
    }

    fn keys(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        let (k, tab_to) = ctx.input_mut(|i| {
            let k = [
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(cmd, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::NONE, Key::Tab),
                i.consume_key(Modifiers::SHIFT, Key::Tab),
                i.consume_key(cmd, Key::Y),
                i.consume_key(cmd, Key::Comma),
                // egui-winit turns ⌘C into Event::Copy, not a key press.
                i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
            ];
            let nums = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5];
            (k, nums.iter().position(|n| i.consume_key(cmd, *n)))
        });
        let [down, up, reveal, enter, esc, tab, back_tab, quick_look, settings, copy] = k;
        if settings {
            self.settings_open = true;
        }
        if self.fda {
            if enter || esc {
                self.dismiss_fda(enter);
            }
            return;
        }
        if down || up {
            self.move_sel(down);
        }
        let empty = self.query.trim().is_empty();
        if tab || back_tab {
            if empty {
                let n = TRIES.len();
                self.try_focus = Some(match (self.try_focus, back_tab) {
                    (None, false) => 0,
                    (None, true) => n - 1,
                    (Some(i), false) => (i + 1) % n,
                    (Some(i), true) => (i + n - 1) % n,
                });
            } else {
                let i = TABS.iter().position(|t| *t == self.tab).unwrap_or(0);
                self.set_tab(TABS[if back_tab { (i + 4) % 5 } else { (i + 1) % 5 }]);
            }
        }
        if let Some(i) = tab_to {
            self.set_tab(TABS[i]);
        }
        if enter && empty && self.try_focus.is_some() {
            self.set_query(TRIES[self.try_focus.unwrap()].0);
        } else if reveal || enter {
            self.open(reveal);
        }
        if quick_look {
            if let Some(p) = self.selected_path() {
                let _ = Command::new("qlmanage").args(["-p", p]).spawn();
            }
        }
        if copy {
            if let Some(p) = self.selected_path().map(String::from) {
                ctx.output_mut(|o| o.copied_text = p);
            }
        }
        if esc {
            if self.query.is_empty() {
                shell::hide();
            } else {
                self.set_query("");
            }
        }
    }

    /// Hide when the panel loses focus (not to our own Settings window); place it on first frame.
    fn window(&mut self, ctx: &egui::Context) {
        let (focused, monitor) = ctx.input(|i| (i.viewport().focused, i.viewport().monitor_size));
        if !self.positioned {
            if let Some(m) = monitor {
                self.positioned = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(
                    (m.x - W) / 2.0,
                    m.y * 0.18,
                )));
            }
        }
        if let Some(f) = focused {
            if self.was_focused && !f && !self.settings_open && self.shell.is_some() {
                shell::hide();
            }
            self.was_focused = f;
        }
    }

    fn menu_status(&self) -> String {
        match self.status.lock().unwrap().as_ref() {
            Some(s) if s.indexing => format!("Indexing… {} files", fmt_count(s.files as usize)),
            Some(s) => format!("{} files · live", fmt_count(s.files as usize)),
            None => "Daemon not running".into(),
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.0; 4] // transparent window; the rounded panel is painted by us
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        self.keys(ctx);
        self.window(ctx);
        let status = self.menu_status();
        if let Some(s) = self.shell.as_mut() {
            s.set_status(status);
        }
        // Keeps the hotkey / menu receivers and daemon status flowing while the panel is hidden.
        ctx.request_repaint_after(Duration::from_millis(100));
        let t = theme::current(ctx.style().visuals.dark_mode);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(t.panel)
                    .stroke(st(1.0, t.line))
                    .rounding(12.0),
            )
            .show(ctx, |ui| {
                let full = ui.max_rect();
                let footer =
                    Rect::from_min_max(Pos2::new(full.left(), full.bottom() - 34.0), full.max);
                let body = Rect::from_min_max(full.min, Pos2::new(full.right(), footer.top()));
                ui.allocate_new_ui(UiBuilder::new().max_rect(body), |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    match self.view() {
                        View::Fda => self.fda_screen(ui, t),
                        v => {
                            self.search_field(ui, t);
                            self.banner(ui, t);
                            match v {
                                View::Empty => self.empty(ui, t),
                                View::NoResults => {
                                    self.tab_bar(ui, t);
                                    self.no_results(ui, t)
                                }
                                _ => {
                                    self.tab_bar(ui, t);
                                    self.results(ui, t)
                                }
                            }
                        }
                    }
                });
                ui.allocate_new_ui(UiBuilder::new().max_rect(footer), |ui| {
                    self.footer(ui, t, footer)
                });
            });

        if self.settings_open {
            self.settings_window(ctx, t);
        }
    }
}

/// Stroke with an f32 width (bare float literals trip the f32-fallback lint).
fn st(w: f32, c: Color32) -> Stroke {
    Stroke::new(w, c)
}

fn hline(ui: &egui::Ui, t: &Theme, y: f32) {
    let r = ui.max_rect();
    ui.painter().hline(r.x_range(), y, st(1.0, t.line));
}

impl App {
    fn search_field(&mut self, ui: &mut egui::Ui, t: &Theme) {
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 60.0), Sense::hover());
        hline(ui, t, rect.bottom());
        let inner = rect.shrink2(Vec2::new(18.0, 0.0));
        ui.allocate_new_ui(
            UiBuilder::new()
                .max_rect(inner)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                let p = ui.painter();
                let c = r.min + Vec2::splat(7.5);
                p.circle_stroke(c, 5.75, st(1.6, t.mu));
                p.line_segment(
                    [r.min + Vec2::splat(12.0), r.min + Vec2::splat(16.0)],
                    st(1.6, t.mu),
                );

                let edit = egui::TextEdit::singleline(&mut self.query)
                    .frame(false)
                    .hint_text(egui::RichText::new("Search files…").color(t.fa))
                    .text_color(t.tx)
                    .font(FontId::proportional(19.0))
                    .desired_width(ui.available_width() - 40.0);
                let resp = ui.add(edit);
                if !self.settings_open {
                    resp.request_focus();
                }
                if resp.changed() {
                    self.try_focus = None;
                    self.run_search();
                }
                if !self.query.is_empty() {
                    keycap(ui, t, "esc");
                }
            },
        );
    }

    /// Screen 04: shown while the daemon is still scanning; results below keep working.
    fn banner(&self, ui: &mut egui::Ui, t: &Theme) {
        let Some(s) = self.status.lock().unwrap().clone().filter(|s| s.indexing) else {
            return;
        };
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 0.0, t.sub);
        hline(ui, t, rect.bottom());
        let x = rect.left() + 18.0;
        p.text(
            Pos2::new(x, rect.top() + 14.0),
            Align2::LEFT_CENTER,
            format!("Indexing… {} files", fmt_commas(s.files)),
            FontId::proportional(13.0),
            t.tx,
        );
        let path = s.current_path.as_deref().map(tilde).unwrap_or_default();
        let pct = format!("{} · {:.0}%", path, s.progress.clamp(0.0, 1.0) * 100.0);
        let mut job = LayoutJob::simple_singleline(pct, FontId::monospace(11.0), t.mu);
        job.wrap = egui::text::TextWrapping {
            max_width: rect.width() - 36.0,
            max_rows: 1,
            break_anywhere: true,
            ..Default::default()
        };
        p.galley(
            Pos2::new(x, rect.top() + 24.0),
            ui.fonts(|f| f.layout_job(job)),
            t.mu,
        );
        let track = Rect::from_min_size(
            Pos2::new(x, rect.bottom() - 10.0),
            Vec2::new(rect.width() - 36.0, 3.0),
        );
        p.rect_filled(track, 2.0, t.line);
        let mut fill = track;
        fill.set_width(track.width() * s.progress.clamp(0.0, 1.0));
        p.rect_filled(fill, 2.0, t.ac);
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui, t: &Theme) {
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::hover());
        hline(ui, t, rect.bottom());
        let mut x = rect.left() + 10.0;
        let mut clicked = None;
        for tab in TABS {
            let on = tab == self.tab;
            let font = FontId::proportional(12.5);
            let label = ui.fonts(|f| {
                f.layout_no_wrap(
                    tab.label().into(),
                    font.clone(),
                    if on { t.tx } else { t.mu },
                )
            });
            let count = ui.fonts(|f| {
                f.layout_no_wrap(
                    fmt_count(tab.count(&self.counts)),
                    FontId::monospace(11.0),
                    if on { t.ac } else { t.mu },
                )
            });
            let w = 16.0 + label.size().x + 6.0 + count.size().x;
            let r = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(w, rect.height()));
            let resp = ui.interact(r, ui.id().with(tab.label()), Sense::click());
            let p = ui.painter();
            p.galley(
                Pos2::new(x + 8.0, r.center().y - label.size().y / 2.0),
                label.clone(),
                t.tx,
            );
            p.galley(
                Pos2::new(
                    x + 14.0 + label.size().x,
                    r.center().y - count.size().y / 2.0,
                ),
                count,
                t.mu,
            );
            if on {
                p.rect_filled(
                    Rect::from_min_max(Pos2::new(r.left(), r.bottom() - 2.0), r.max),
                    0.0,
                    t.ac,
                );
            }
            if resp.clicked() {
                clicked = Some(tab);
            }
            x += w + 4.0;
        }
        let right = Pos2::new(rect.right() - 16.0, rect.center().y);
        let p = ui.painter();
        if matches!(self.tab, Tab::Recent | Tab::Large) {
            p.text(
                right,
                Align2::RIGHT_CENTER,
                self.tab.sort_label(),
                FontId::proportional(11.5),
                t.mu,
            );
        } else {
            let g = p.text(
                right,
                Align2::RIGHT_CENTER,
                "⇥",
                FontId::monospace(10.5),
                t.mu,
            );
            p.rect_stroke(g.expand2(Vec2::new(5.0, 2.0)), 4.0, st(1.0, t.line));
            p.text(
                Pos2::new(g.left() - 12.0, right.y),
                Align2::RIGHT_CENTER,
                self.tab.sort_label(),
                FontId::proportional(11.5),
                t.mu,
            );
        }
        if let Some(tab) = clicked {
            self.set_tab(tab);
        }
    }

    fn results(&mut self, ui: &mut egui::Ui, t: &Theme) {
        ui.add_space(6.0);
        let mut area = egui::ScrollArea::vertical().auto_shrink(false);
        if let Some(off) = self.scroll_to.take() {
            area = area.vertical_scroll_offset(off);
        }
        let large = self.tab == Tab::Large;
        let max = if large {
            self.hits.iter().map(|h| h.size).max().unwrap_or(0)
        } else {
            0
        };
        let mut clicked = None;
        let out = area.show_rows(ui, ROW_H, self.hits.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in range {
                let (rect, resp) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
                self.paint_row(
                    ui,
                    t,
                    rect.shrink2(Vec2::new(8.0, 0.0)),
                    i,
                    resp.hovered(),
                    large.then_some(max),
                );
                if resp.clicked() || resp.double_clicked() {
                    clicked = Some((i, resp.double_clicked()));
                }
            }
        });
        self.last_offset = out.state.offset.y;
        self.last_view_h = out.inner_rect.height();
        self.handle_click(clicked);
    }

    fn handle_click(&mut self, clicked: Option<(usize, bool)>) {
        if let Some((i, double)) = clicked {
            self.selected = i;
            self.try_focus = None;
            if double {
                self.open(false);
            }
        }
    }

    /// Screen 03a: recent files + example queries.
    fn empty(&mut self, ui: &mut egui::Ui, t: &Theme) {
        let section = |ui: &mut egui::Ui, s: &str, top: f32| {
            ui.add_space(top);
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                ui.label(egui::RichText::new(s).size(11.0).strong().color(t.mu));
            });
            ui.add_space(6.0);
        };
        section(ui, "RECENT", 12.0);
        let mut clicked = None;
        for i in 0..self.hits.len() {
            let (rect, resp) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
            self.paint_row(
                ui,
                t,
                rect.shrink2(Vec2::new(8.0, 0.0)),
                i,
                resp.hovered(),
                None,
            );
            if resp.clicked() || resp.double_clicked() {
                clicked = Some((i, resp.double_clicked()));
            }
        }
        if self.hits.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                ui.label(
                    egui::RichText::new("Nothing changed in the last 7 days.")
                        .size(12.5)
                        .color(t.fa),
                );
            });
        }
        self.handle_click(clicked);

        section(ui, "TRY", 16.0);
        let row = ui
            .allocate_exact_size(Vec2::new(ui.available_width(), 52.0), Sense::hover())
            .0
            .shrink2(Vec2::new(18.0, 0.0));
        let w = (row.width() - 16.0) / 3.0;
        for (i, (q, d)) in TRIES.iter().enumerate() {
            let r = Rect::from_min_size(
                Pos2::new(row.left() + i as f32 * (w + 8.0), row.top()),
                Vec2::new(w, row.height()),
            );
            let resp = ui.interact(r, ui.id().with(("try", i)), Sense::click());
            let focus = self.try_focus == Some(i);
            let p = ui.painter();
            if resp.hovered() {
                p.rect_filled(r, 7.0, t.hl);
            }
            p.rect_stroke(
                r,
                7.0,
                st(
                    if focus { 2.0 } else { 1.0 },
                    if focus { t.ac } else { t.line },
                ),
            );
            p.text(
                r.min + Vec2::new(12.0, 16.0),
                Align2::LEFT_CENTER,
                *q,
                FontId::monospace(12.5),
                t.tx,
            );
            p.text(
                r.min + Vec2::new(12.0, 35.0),
                Align2::LEFT_CENTER,
                *d,
                FontId::proportional(12.0),
                t.mu,
            );
            if resp.clicked() {
                self.set_query(q);
            }
        }
    }

    /// Screen 03b.
    fn no_results(&mut self, ui: &mut egui::Ui, t: &Theme) {
        ui.add_space(28.0);
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new(format!("No matches for {}", self.query.trim()))
                    .size(15.0)
                    .strong()
                    .color(t.tx),
            );
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Check the spelling, or narrow the search with filters.")
                    .size(13.0)
                    .color(t.mu),
            );
        });
        ui.add_space(16.0);
        let box_w = 420.0;
        let left = ui.max_rect().center().x - box_w / 2.0;
        let top = ui.cursor().top();
        let rect = Rect::from_min_size(
            Pos2::new(left, top),
            Vec2::new(box_w, SYNTAX.len() as f32 * 30.0),
        );
        ui.allocate_rect(rect, Sense::hover());
        let p = ui.painter();
        p.rect_stroke(rect, 7.0, st(1.0, t.line));
        for (i, (code, what)) in SYNTAX.iter().enumerate() {
            let y = rect.top() + i as f32 * 30.0;
            if i > 0 {
                p.hline(rect.x_range(), y, st(1.0, t.line));
            }
            p.text(
                Pos2::new(rect.left() + 14.0, y + 15.0),
                Align2::LEFT_CENTER,
                *code,
                FontId::monospace(12.0),
                t.tx,
            );
            p.text(
                Pos2::new(rect.left() + 170.0, y + 15.0),
                Align2::LEFT_CENTER,
                *what,
                FontId::proportional(12.5),
                t.mu,
            );
        }
    }

    /// Screen 05.
    fn fda_screen(&mut self, ui: &mut egui::Ui, t: &Theme) {
        ui.add_space(70.0);
        ui.vertical_centered(|ui| {
            ui.set_max_width(440.0);
            ui.label(egui::RichText::new("Allow Full Disk Access").size(19.0).strong().color(t.tx));
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(
                    "macOS keeps folders like Mail, Desktop and Downloads private until you allow access, \
                     so Ferret can’t see every file yet. Ferret only reads names, sizes and dates, and the \
                     index never leaves this Mac.",
                )
                .size(13.0)
                .color(t.mu),
            );
            ui.add_space(22.0);
            let open = egui::Button::new(egui::RichText::new("Open System Settings  ↵").color(Color32::WHITE).size(13.0))
                .fill(t.ac)
                .rounding(7.0)
                .min_size(Vec2::new(220.0, 32.0));
            if ui.add(open).clicked() {
                self.dismiss_fda(true);
            }
            ui.add_space(8.0);
            let skip = egui::Button::new(egui::RichText::new("Skip for now").color(t.mu).size(13.0)).frame(false);
            if ui.add(skip).clicked() {
                self.dismiss_fda(false);
            }
            ui.add_space(26.0);
            ui.label(egui::RichText::new("Privacy & Security → Full Disk Access → Ferret").font(FontId::monospace(11.0)).color(t.fa));
        });
    }

    fn paint_row(
        &self,
        ui: &egui::Ui,
        t: &Theme,
        rect: Rect,
        i: usize,
        hovered: bool,
        large_max: Option<u64>,
    ) {
        let hit = &self.hits[i];
        let p = ui.painter();
        let selected = i == self.selected && self.try_focus.is_none();
        if selected {
            p.rect_filled(rect, 7.0, t.acs);
        } else if hovered {
            p.rect_filled(rect, 7.0, t.hl);
        }
        let mono = |s| FontId::monospace(s);
        let (parent, name) = hit.path.rsplit_once('/').unwrap_or(("", &hit.path));

        // type badge / folder glyph
        let icon = Rect::from_min_size(
            rect.left_center() + Vec2::new(10.0, -10.0),
            Vec2::splat(20.0),
        );
        if hit.kind == Kind::Dir {
            p.rect_filled(
                Rect::from_min_size(icon.min + Vec2::new(1.0, 3.0), Vec2::new(8.0, 4.0)),
                2.0,
                t.fa,
            );
            p.rect_filled(
                Rect::from_min_size(icon.min + Vec2::new(1.0, 5.0), Vec2::new(18.0, 12.0)),
                2.0,
                t.fa,
            );
        } else {
            p.rect(icon, Rounding::same(4.0), t.sub, st(1.0, t.line));
            let ext = name.rsplit_once('.').map_or(String::new(), |(_, e)| {
                e.chars().take(4).collect::<String>().to_uppercase()
            });
            p.text(icon.center(), Align2::CENTER_CENTER, ext, mono(7.0), t.mu);
        }

        // right columns: date (84), size (72 or 64 + 96 bar)
        let right = rect.right() - 10.0;
        let cy = rect.center().y;
        p.text(
            Pos2::new(right, cy),
            Align2::RIGHT_CENTER,
            fmt_date(hit.mtime),
            mono(11.5),
            t.mu,
        );
        let size_right = right - 84.0 - 12.0;
        if hit.kind != Kind::Dir {
            let c = if large_max.is_some() { t.tx } else { t.mu };
            p.text(
                Pos2::new(size_right, cy),
                Align2::RIGHT_CENTER,
                fmt_size(hit.size),
                mono(11.5),
                c,
            );
        }
        let mut cols_left = size_right - 72.0;
        if let Some(max) = large_max {
            let track = Rect::from_min_size(
                Pos2::new(size_right - 64.0 - 12.0 - 96.0, cy - 2.0),
                Vec2::new(96.0, 4.0),
            );
            p.rect(track, 2.0, t.sub, st(1.0, t.line));
            let mut fill = track;
            fill.set_width(96.0 * bar_ratio(hit.size, max));
            p.rect_filled(fill, 2.0, if selected { t.ac } else { t.fa });
            cols_left = track.left();
        }

        // name, then dimmed parent on the same line
        let x = icon.right() + 12.0;
        let max_w = cols_left - 12.0 - x;
        let mut job = highlight(name, &self.words, t);
        job.wrap.max_width = max_w;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let g = ui.fonts(|f| f.layout_job(job));
        let name_w = g.size().x;
        p.galley(Pos2::new(x, cy - g.size().y / 2.0), g, t.tx);
        let rest = max_w - name_w - 10.0;
        if rest > 20.0 {
            let mut pj = LayoutJob::simple_singleline(tilde(parent), mono(11.5), t.mu);
            pj.wrap.max_width = rest;
            pj.wrap.max_rows = 1;
            pj.wrap.break_anywhere = true;
            let g = ui.fonts(|f| f.layout_job(pj));
            p.galley(Pos2::new(x + name_w + 10.0, cy - g.size().y / 2.0), g, t.mu);
        }
    }

    fn footer(&self, ui: &mut egui::Ui, t: &Theme, rect: Rect) {
        let p = ui.painter();
        p.rect_filled(
            rect,
            Rounding {
                nw: 0.0,
                ne: 0.0,
                sw: 12.0,
                se: 12.0,
            },
            t.sub,
        );
        p.hline(rect.x_range(), rect.top(), st(1.0, t.line));
        let inner = rect.shrink2(Vec2::new(14.0, 0.0));
        ui.allocate_new_ui(
            UiBuilder::new()
                .max_rect(inner)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                let small = |s: String, c: Color32| egui::RichText::new(s).size(11.5).color(c);
                let view = self.view();
                if let Some(e) = &self.error {
                    ui.label(small(e.clone(), t.ac));
                } else if view == View::Results || view == View::NoResults {
                    let st = self.status.lock().unwrap().clone();
                    let snap_files = self.snap.get().and_then(|o| o.as_ref()).map(|i| i.len());
                    // A fresh daemon reply wins; otherwise trust the last search's source.
                    let live = st.is_some() && (self.live || snap_files.is_none());
                    if let Some(s) = status_text(
                        self.total,
                        self.took_us,
                        st.as_ref().filter(|_| live),
                        snap_files,
                    ) {
                        ui.label(
                            egui::RichText::new(s)
                                .font(FontId::monospace(11.0))
                                .color(t.mu),
                        );
                    }
                }
                let hints: &[(&str, &str)] = match view {
                    View::Fda => &[("↵", "Open System Settings"), ("esc", "Skip")],
                    View::Empty => &[("↑↓", "Select"), ("↵", "Open"), ("⇥", "Examples")],
                    _ => &[
                        ("↵", "Open"),
                        ("⌘↵", "Reveal"),
                        ("⌘Y", "Quick Look"),
                        ("⌘C", "Copy path"),
                    ],
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for (k, what) in hints.iter().rev() {
                        ui.label(small((*what).into(), t.mu));
                        keycap(ui, t, k);
                        ui.add_space(8.0);
                    }
                });
            },
        );
    }

    /// Screen 07, its own native window.
    fn settings_window(&mut self, ctx: &egui::Context, t: &'static Theme) {
        let vb = egui::ViewportBuilder::default()
            .with_title("Ferret Settings")
            .with_inner_size([560.0, 600.0])
            .with_resizable(false);
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("settings"), vb, |ctx, _| {
            if ctx.input(|i| i.viewport().close_requested()) {
                self.settings_open = false;
                self.recording = false;
                // Opened from the menu bar with the panel ordered out: don't stay active windowless.
                if self.shell.is_some() && !shell::panel_visible() {
                    shell::hide();
                }
            }
            egui::CentralPanel::default()
                .frame(egui::Frame::none().fill(t.panel).inner_margin(22.0))
                .show(ctx, |ui| self.settings_ui(ui, t));
        });
    }

    fn save_cfg(&mut self) {
        self.settings_msg = config::save(&self.cfg)
            .err()
            .map(|e| format!("Couldn't save config.toml: {e}"));
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui, t: &Theme) {
        let heading = |ui: &mut egui::Ui, h: &str, sub: &str| {
            ui.label(egui::RichText::new(h).size(13.0).strong().color(t.tx));
            if !sub.is_empty() {
                ui.label(egui::RichText::new(sub).size(12.0).color(t.mu));
            }
            ui.add_space(6.0);
        };

        heading(ui, "Global hotkey", "Opens the search panel from anywhere.");
        if self.recording {
            let got = ui.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key {
                        key: Key::Escape,
                        pressed: true,
                        ..
                    } => Some(None),
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => config::hotkey_from_egui(*modifiers, *key).map(Some),
                    _ => None,
                })
            });
            if let Some(got) = got {
                self.recording = false;
                if let Some(s) = got {
                    let res = self.shell.as_mut().map_or(Ok(()), |sh| sh.set_hotkey(&s));
                    match res {
                        Ok(()) => {
                            self.cfg.hotkey = s;
                            self.save_cfg();
                        }
                        Err(e) => self.settings_msg = Some(e),
                    }
                }
            }
        }
        let label = if self.recording {
            "Type new shortcut…".to_string()
        } else {
            config::hotkey_glyphs(&self.cfg.hotkey)
        };
        let b = egui::Button::new(
            egui::RichText::new(label)
                .font(FontId::monospace(13.0))
                .color(t.tx),
        )
        .fill(t.sub)
        .stroke(st(
            if self.recording { 2.0 } else { 1.0 },
            if self.recording { t.ac } else { t.line },
        ))
        .min_size(Vec2::new(180.0, 28.0));
        if ui.add(b).clicked() {
            self.recording = !self.recording;
        }
        ui.add_space(18.0);

        heading(
            ui,
            "Excluded folders",
            "Ferret won’t index these paths. Globs allowed. node_modules, virtualenvs, build output and .gitignore’d files are skipped by default. Takes effect after a re-index (see README).",
        );
        let mut changed = false;
        egui::Frame::none()
            .stroke(st(1.0, t.line))
            .rounding(7.0)
            .inner_margin(6.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .max_height(90.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (i, p) in self.cfg.excluded.iter().enumerate() {
                            let on = self.excluded_sel == Some(i);
                            if ui
                                .selectable_label(
                                    on,
                                    egui::RichText::new(p).font(FontId::monospace(12.0)),
                                )
                                .clicked()
                            {
                                self.excluded_sel = Some(i);
                            }
                        }
                    });
                ui.horizontal(|ui| {
                    let add = ui.add(
                        egui::TextEdit::singleline(&mut self.new_excluded)
                            .hint_text("~/path or glob")
                            .desired_width(300.0),
                    );
                    let enter = add.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if (ui.button("+").clicked() || enter) && !self.new_excluded.trim().is_empty() {
                        self.cfg.excluded.push(self.new_excluded.trim().to_string());
                        self.new_excluded.clear();
                        changed = true;
                    }
                    if ui
                        .add_enabled(self.excluded_sel.is_some(), egui::Button::new("−"))
                        .clicked()
                    {
                        if let Some(i) = self
                            .excluded_sel
                            .take()
                            .filter(|i| *i < self.cfg.excluded.len())
                        {
                            self.cfg.excluded.remove(i);
                            changed = true;
                        }
                    }
                });
            });
        ui.add_space(18.0);

        if ui
            .checkbox(&mut self.cfg.external_drives, "Index external drives")
            .changed()
        {
            changed = true;
        }
        ui.label(
            egui::RichText::new("Includes volumes mounted under /Volumes.")
                .size(12.0)
                .color(t.mu),
        );
        ui.add_space(10.0);
        if ui
            .checkbox(&mut self.cfg.launch_at_login, "Launch at login")
            .changed()
        {
            match config::set_launch_at_login(self.cfg.launch_at_login) {
                Ok(()) => changed = true,
                Err(e) => {
                    self.cfg.launch_at_login = !self.cfg.launch_at_login;
                    self.settings_msg = Some(format!("Couldn't update login item: {e}"));
                }
            }
        }
        ui.add_space(18.0);

        heading(
            ui,
            "AI agents (MCP)",
            "Let Claude Code and other MCP clients query your index. Run once in a terminal:",
        );
        let line = config::mcp_line();
        ui.horizontal(|ui| {
            egui::Frame::none()
                .fill(t.sub)
                .stroke(st(1.0, t.line))
                .rounding(6.0)
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(&line)
                            .font(FontId::monospace(12.0))
                            .color(t.tx),
                    );
                });
            if ui.button("Copy").clicked() {
                ui.output_mut(|o| o.copied_text = line.clone());
            }
        });
        ui.add_space(6.0);
        if ui
            .checkbox(&mut self.cfg.mcp_contents, "Let agents search file contents")
            .changed()
        {
            changed = true;
        }
        ui.label(
            egui::RichText::new("Turns on search_contents. Secret folders (~/.ssh, keychains, browser profiles…) stay off limits.")
                .size(12.0)
                .color(t.mu),
        );
        if config::bundled_ff().is_some() {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let res = if ui.button("Install `ff` command").clicked() {
                    Some(config::install_cli())
                } else if ui.button("Start background indexer").clicked() {
                    Some(config::start_daemon())
                } else {
                    None
                };
                if let Some(r) = res {
                    self.settings_msg = Some(r.unwrap_or_else(|e| format!("Failed: {e}")));
                }
            });
        }

        if changed {
            self.save_cfg();
        }
        if let Some(m) = &self.settings_msg {
            ui.add_space(10.0);
            ui.label(egui::RichText::new(m).size(12.0).color(t.ac));
        }
    }
}

/// Daemon if reachable, else the snapshot. `kind` is applied locally the way ferretd does it.
fn fetch(
    q: &str,
    parsed: &Query,
    kind: KindFilter,
    sort: SortArg,
    limit: usize,
    snap: &Snap,
) -> Result<(SearchResult, bool), String> {
    let op = Op::Search {
        q: q.to_string(),
        limit,
        sort,
        kind,
    };
    choose(protocol::request(op), || {
        let Some(index) = snap.get_or_init(|| snapshot::load(&snapshot::default_path()).ok())
        else {
            return Err("No index yet — run `ff index` or start ferretd".into());
        };
        let mut p = parsed.clone();
        let want = match kind {
            KindFilter::All => None,
            KindFilter::File => Some(Kind::File),
            KindFilter::Dir => Some(Kind::Dir),
        };
        if let Some(w) = want {
            if p.kind.is_some_and(|k| k != w) {
                return Ok(SearchResult::default());
            }
            p.kind = Some(w);
        }
        Ok(ferret_core::search(index, &p, limit, sort.into()))
    })
}

fn keycap(ui: &mut egui::Ui, t: &Theme, k: &str) {
    egui::Frame::none()
        .fill(t.panel)
        .stroke(st(1.0_f32, t.line))
        .rounding(4.0)
        .inner_margin(egui::Margin::symmetric(5.0, 1.0))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(k)
                    .font(FontId::monospace(10.5))
                    .color(t.mu),
            )
        });
}

/// Name with query words in accent color (same rules as the mockup's `seg`).
fn highlight(name: &str, words: &[String], t: &Theme) -> LayoutJob {
    let font = FontId::proportional(13.0);
    let fmt = |c| TextFormat {
        font_id: font.clone(),
        color: c,
        ..Default::default()
    };
    let lower = name.to_lowercase();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    // shortcut: byte offsets only line up when lowercasing keeps lengths; otherwise no highlight.
    if lower.len() == name.len() {
        for w in words {
            if let Some(i) = lower.find(&w.to_lowercase()) {
                let end = i + w.len();
                if name.is_char_boundary(i) && name.is_char_boundary(end) {
                    ranges.push((i, end));
                }
            }
        }
    }
    ranges.sort();
    let mut job = LayoutJob::default();
    let mut pos = 0;
    for (a, b) in ranges {
        if a < pos {
            continue;
        }
        if a > pos {
            job.append(&name[pos..a], 0.0, fmt(t.tx));
        }
        job.append(&name[a..b], 0.0, fmt(t.ac));
        pos = b;
    }
    if pos < name.len() {
        job.append(&name[pos..], 0.0, fmt(t.tx));
    }
    job
}

/// Daemon answer if it was reachable, else the in-process fallback. Bool = came from the daemon.
fn choose(
    daemon: std::io::Result<Response>,
    fallback: impl FnOnce() -> Result<SearchResult, String>,
) -> Result<(SearchResult, bool), String> {
    match daemon {
        Ok(Response { error: Some(e), .. }) => Err(e),
        Ok(r) => Ok((
            SearchResult {
                hits: r.hits.into_iter().map(hit_from).collect(),
                total: r.total,
                by_kind: r.by_kind,
                took_us: r.took_us,
            },
            true,
        )),
        Err(_) => fallback().map(|r| (r, false)),
    }
}

/// [all, files, folders] from per-kind match counts, honouring a `type:` the user typed.
fn tab_counts(by_kind: [usize; 3], typed: Option<Kind>) -> Counts {
    let k = |kind: Kind| {
        if typed.is_none_or(|t| t == kind) {
            by_kind[kind as usize]
        } else {
            0
        }
    };
    let (f, d, s) = (k(Kind::File), k(Kind::Dir), k(Kind::Symlink));
    [f + d + s, f, d]
}

fn hit_from(h: HitOut) -> Hit {
    Hit {
        node: 0,
        path: h.path,
        kind: h.kind,
        size: h.size,
        mtime: h.mtime,
        score: 0,
    }
}

/// `live` = daemon status when the shown results are from the daemon; else snapshot file count.
fn status_text(
    total: usize,
    took_us: u64,
    live: Option<&Status>,
    snap_files: Option<usize>,
) -> Option<String> {
    let (files, src) = match (live, snap_files) {
        (Some(s), _) if s.indexing => {
            return Some(format!(
                "{total} matches so far · {} · {} indexed",
                fmt_ms(took_us),
                fmt_commas(s.files)
            ))
        }
        (Some(s), _) => (s.files as usize, "live"),
        (None, Some(n)) => (n, "snapshot"),
        (None, None) => return None,
    };
    Some(format!(
        "{total} matches · {} · {} files · {src}",
        fmt_ms(took_us),
        fmt_count(files)
    ))
}

fn fmt_ms(us: u64) -> String {
    if us < 10_000 {
        format!("{:.1} ms", us as f64 / 1000.0)
    } else {
        format!("{} ms", us / 1000)
    }
}

fn tilde(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.to_string(),
    }
}

fn fmt_size(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut u) = (b as f64, 0);
    while v >= 1024.0 && u < U.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 || v >= 10.0 {
        format!("{v:.0} {}", U[u])
    } else {
        format!("{v:.1} {}", U[u])
    }
}

fn fmt_count(n: usize) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// 840213 -> "840,213"
fn fmt_commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn local(secs: i64) -> libc::tm {
    // SAFETY: localtime_r writes only into the tm we pass.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = secs as libc::time_t;
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

/// "Today 09:12", "Mar 11" (this year), "2024" (older).
fn fmt_date(mtime: i64) -> String {
    const M: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (n, d) = (local(now), local(mtime));
    if d.tm_year != n.tm_year {
        format!("{}", d.tm_year + 1900)
    } else if d.tm_yday == n.tm_yday {
        format!("Today {:02}:{:02}", d.tm_hour, d.tm_min)
    } else {
        format!("{} {}", M[d.tm_mon.clamp(0, 11) as usize], d.tm_mday)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_counts_from_one_reply() {
        assert_eq!(tab_counts([5, 2, 1], None), [8, 5, 2]);
        assert_eq!(tab_counts([5, 2, 1], Some(Kind::Dir)), [2, 0, 2]);
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_size(184 * 1024), "184 KB");
        assert_eq!(fmt_size(1536), "1.5 KB");
        assert_eq!(fmt_count(2_100_000), "2.1M");
        assert_eq!(fmt_commas(840_213), "840,213");
        assert_eq!(fmt_commas(12), "12");
        assert_eq!(fmt_commas(1_000_000), "1,000,000");
        assert_eq!(fmt_date(0).len(), 4); // old file -> just the year
        let j = highlight(
            "Invoice-2025.pdf",
            &["invoice".into(), "2025".into()],
            &theme::L,
        );
        assert_eq!(j.sections.len(), 4);
        assert_eq!(j.sections[0].format.color, theme::L.ac);
    }

    fn out() -> HitOut {
        HitOut {
            path: "/a/b.txt".into(),
            kind: Kind::File,
            size: 5,
            mtime: 9,
        }
    }

    #[test]
    fn maps_hitout() {
        let h = hit_from(out());
        assert_eq!(
            (h.path.as_str(), h.kind, h.size, h.mtime),
            ("/a/b.txt", Kind::File, 5, 9)
        );
    }

    #[test]
    fn ms() {
        assert_eq!(fmt_ms(3_412), "3.4 ms");
        assert_eq!(fmt_ms(0), "0.0 ms");
        assert_eq!(fmt_ms(10_000), "10 ms");
        assert_eq!(fmt_ms(123_900), "123 ms");
    }

    #[test]
    fn status_lines() {
        let s = Status {
            files: 2_100_000,
            ..Default::default()
        };
        assert_eq!(
            status_text(7, 3_400, Some(&s), None).unwrap(),
            "7 matches · 3.4 ms · 2.1M files · live"
        );
        assert_eq!(
            status_text(7, 25_000, None, Some(2_100_000)).unwrap(),
            "7 matches · 25 ms · 2.1M files · snapshot"
        );
        let idx = Status {
            files: 840_213,
            indexing: true,
            ..Default::default()
        };
        assert_eq!(
            status_text(41, 3_000, Some(&idx), None).unwrap(),
            "41 matches so far · 3.0 ms · 840,213 indexed"
        );
        assert_eq!(status_text(0, 0, None, None), None);
    }

    #[test]
    fn daemon_vs_fallback() {
        let fb = || {
            Ok(SearchResult {
                hits: Vec::new(),
                total: 42,
                took_us: 1,
                ..Default::default()
            })
        };
        let ok = Response {
            total: 1,
            took_us: 5,
            hits: vec![out()],
            ..Default::default()
        };
        let (r, live) = choose(Ok(ok), || panic!("fallback must not run")).unwrap();
        assert!(live);
        assert_eq!((r.total, r.hits.len()), (1, 1));
        let down = std::io::Error::from(std::io::ErrorKind::NotFound);
        let (r, live) = choose(Err(down), fb).unwrap();
        assert!(!live);
        assert_eq!(r.total, 42);
        let bad = Response {
            error: Some("bad query".into()),
            ..Default::default()
        };
        assert_eq!(choose(Ok(bad), fb).unwrap_err(), "bad query");
    }

    #[test]
    fn tabs_map_to_kind_sort_query() {
        use KindFilter as K;
        use SortArg as S;
        let cases = [
            (Tab::All, K::All, S::Score),
            (Tab::Files, K::File, S::Score),
            (Tab::Folders, K::Dir, S::Score),
            (Tab::Recent, K::All, S::Mtime),
            (Tab::Large, K::File, S::Size),
        ];
        for (tab, kind, sort) in cases {
            assert_eq!(
                plan(tab, "invoice"),
                ("invoice".into(), kind, sort, LIMIT),
                "{tab:?}"
            );
            // no query: always the recent-files list, whatever the tab
            assert_eq!(
                plan(tab, "  "),
                ("mod:<7d".into(), K::File, S::Mtime, RECENT_LIMIT)
            );
        }
        assert!(ferret_core::parse("mod:<7d").is_ok());
        let c = [10, 7, 3];
        assert_eq!(
            (
                Tab::Recent.count(&c),
                Tab::Large.count(&c),
                Tab::Folders.count(&c)
            ),
            (10, 7, 3)
        );
        assert_eq!(Tab::Large.sort_label(), "Size ↓");
    }

    #[test]
    fn size_bar() {
        assert_eq!(bar_ratio(50, 100), 0.5);
        assert_eq!(bar_ratio(100, 100), 1.0);
        assert_eq!(bar_ratio(0, 0), 0.0);
        assert_eq!(bar_ratio(200, 100), 1.0);
    }

    #[test]
    fn view_selection() {
        assert_eq!(pick_view(true, "x", 3, false, false), View::Fda);
        assert_eq!(pick_view(false, "", 0, false, false), View::Empty);
        assert_eq!(pick_view(false, " ", 5, true, false), View::Empty);
        assert_eq!(pick_view(false, "zzz", 0, false, false), View::NoResults);
        assert_eq!(pick_view(false, "zzz", 0, true, false), View::Results); // still loading
        assert_eq!(pick_view(false, "bad:", 0, false, true), View::Results); // error shows in footer
        assert_eq!(pick_view(false, "a", 2, false, false), View::Results);
    }
}
