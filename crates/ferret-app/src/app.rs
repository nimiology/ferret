use std::process::Command;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui::{
    self, text::LayoutJob, Align2, Color32, FontId, Key, Modifiers, Rect, Rounding, Sense,
    Stroke, TextFormat, Vec2,
};
use ferret_core::{snapshot, Hit, Index, Kind, Sort};

use crate::theme::{self, Theme};

const ROW_H: f32 = 36.0;
const LIMIT: usize = 200;

enum Load {
    Loading,
    Missing,
    Ready(Arc<Index>),
}

/// (generation, query words, result or error)
type Reply = (u64, Vec<String>, Result<ferret_core::SearchResult, String>);

pub struct App {
    ctx: egui::Context,
    load: Load,
    load_rx: Option<Receiver<Option<Index>>>,
    query: String,
    generation: u64,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    hits: Vec<Hit>,
    words: Vec<String>,
    total: usize,
    took_us: u64,
    error: Option<String>,
    selected: usize,
    scroll_to: Option<f32>,
    last_offset: f32,
    last_view_h: f32,
}

impl App {
    pub fn new(ctx: egui::Context) -> Self {
        let (load_tx, load_rx) = channel();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let _ = load_tx.send(snapshot::load(&snapshot::default_path()).ok());
            c.request_repaint();
        });
        let (tx, rx) = channel();
        Self {
            ctx,
            load: Load::Loading,
            load_rx: Some(load_rx),
            query: String::new(),
            generation: 0,
            tx,
            rx,
            hits: Vec::new(),
            words: Vec::new(),
            total: 0,
            took_us: 0,
            error: None,
            selected: 0,
            scroll_to: None,
            last_offset: 0.0,
            last_view_h: 0.0,
        }
    }

    fn run_search(&mut self) {
        self.generation += 1;
        let Load::Ready(index) = &self.load else { return };
        if self.query.trim().is_empty() {
            self.hits.clear();
            self.total = 0;
            self.error = None;
            return;
        }
        let (gen, q, index, tx, ctx) =
            (self.generation, self.query.clone(), index.clone(), self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let reply = match ferret_core::parse(&q) {
                Ok(parsed) => {
                    let words = parsed.words.clone();
                    (gen, words, Ok(ferret_core::search(&index, &parsed, LIMIT, Sort::Score)))
                }
                Err(e) => (gen, Vec::new(), Err(e.to_string())),
            };
            let _ = tx.send(reply);
            ctx.request_repaint();
        });
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.load_rx {
            if let Ok(idx) = rx.try_recv() {
                self.load = idx.map_or(Load::Missing, |i| Load::Ready(Arc::new(i)));
                self.load_rx = None;
                self.run_search();
            }
        }
        while let Ok((gen, words, res)) = self.rx.try_recv() {
            if gen != self.generation {
                continue; // stale
            }
            match res {
                Ok(r) => {
                    self.hits = r.hits;
                    self.total = r.total;
                    self.took_us = r.took_us;
                    self.words = words;
                    self.error = None;
                    self.selected = 0;
                    self.scroll_to = Some(0.0);
                }
                Err(e) => self.error = Some(e), // keep old results
            }
        }
    }

    fn open(&self, reveal: bool) {
        if let Some(hit) = self.hits.get(self.selected) {
            let mut cmd = Command::new("open");
            if reveal {
                cmd.arg("-R");
            }
            let _ = cmd.arg(&hit.path).spawn();
        }
    }

    fn move_sel(&mut self, down: bool) {
        if self.hits.is_empty() {
            return;
        }
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

    fn keys(&mut self, ctx: &egui::Context) {
        let (down, up, reveal, open, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::COMMAND, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if down || up {
            self.move_sel(down);
        }
        if reveal || open {
            self.open(reveal);
        }
        if esc {
            self.query.clear();
            self.run_search();
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        self.keys(ctx);
        let t = theme::current(ctx.style().visuals.dark_mode);

        egui::TopBottomPanel::bottom("status")
            .frame(egui::Frame::none().fill(t.bar).inner_margin(egui::Margin::symmetric(14.0, 8.0)))
            .show(ctx, |ui| self.status(ui, t));

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(t.panel).inner_margin(10.0))
            .show(ctx, |ui| {
                self.search_field(ui, t);
                ui.add_space(8.0);
                self.results(ui, t);
            });
    }
}

impl App {
    fn search_field(&mut self, ui: &mut egui::Ui, t: &Theme) {
        egui::Frame::none()
            .fill(t.sub)
            .stroke(Stroke::new(1.0_f32, t.line))
            .rounding(7.0)
            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // magnifier glyph
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                    let p = ui.painter();
                    let c = r.center() - Vec2::splat(1.5);
                    p.circle_stroke(c, 5.0, Stroke::new(1.6_f32, t.mu));
                    p.line_segment([c + Vec2::splat(3.6), c + Vec2::splat(7.5)], Stroke::new(1.6_f32, t.mu));

                    let edit = egui::TextEdit::singleline(&mut self.query)
                        .frame(false)
                        .hint_text(egui::RichText::new("Search files and folders").color(t.fa))
                        .text_color(t.tx)
                        .font(FontId::proportional(17.0))
                        .desired_width(ui.available_width() - 40.0);
                    let resp = ui.add(edit);
                    resp.request_focus();
                    if resp.changed() {
                        self.run_search();
                    }
                    keycap(ui, t, "esc");
                });
            });
    }

    fn results(&mut self, ui: &mut egui::Ui, t: &Theme) {
        let msg = match &self.load {
            Load::Loading => Some("Loading index…"),
            Load::Missing => Some("No index yet — run `ff index`"),
            Load::Ready(_) => None,
        };
        if let Some(msg) = msg {
            ui.centered_and_justified(|ui| ui.label(egui::RichText::new(msg).color(t.mu)));
            return;
        }
        let mut area = egui::ScrollArea::vertical().auto_shrink(false);
        if let Some(off) = self.scroll_to.take() {
            area = area.vertical_scroll_offset(off);
        }
        let mut clicked = None;
        let out = area.show_rows(ui, ROW_H, self.hits.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in range {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
                self.paint_row(ui, t, rect, i, resp.hovered());
                if resp.clicked() || resp.double_clicked() {
                    clicked = Some((i, resp.double_clicked()));
                }
            }
        });
        self.last_offset = out.state.offset.y;
        self.last_view_h = out.inner_rect.height();
        if let Some((i, double)) = clicked {
            self.selected = i;
            if double {
                self.open(false);
            }
        }
    }

    fn paint_row(&self, ui: &egui::Ui, t: &Theme, rect: Rect, i: usize, hovered: bool) {
        let hit = &self.hits[i];
        let p = ui.painter();
        if i == self.selected {
            p.rect_filled(rect, 7.0, t.acs);
        } else if hovered {
            p.rect_filled(rect, 7.0, t.hl);
        }
        let mono = |s| FontId::monospace(s);
        let (parent, name) = hit.path.rsplit_once('/').unwrap_or(("", &hit.path));

        // type badge
        let badge = Rect::from_center_size(rect.left_center() + Vec2::new(24.0, 0.0), Vec2::new(32.0, 20.0));
        p.rect(badge, Rounding::same(4.0), t.sub, Stroke::new(1.0_f32, t.line));
        let label = match hit.kind {
            Kind::Dir => "DIR".to_string(),
            _ => name.rsplit_once('.').map_or(String::new(), |(_, e)| e.chars().take(4).collect::<String>().to_uppercase()),
        };
        p.text(badge.center(), Align2::CENTER_CENTER, label, mono(8.5), t.mu);

        // right columns: size, date
        let right = rect.right() - 12.0;
        let date = fmt_date(hit.mtime);
        p.text(egui::pos2(right, rect.center().y), Align2::RIGHT_CENTER, &date, mono(11.0), t.mu);
        if hit.kind != Kind::Dir {
            p.text(egui::pos2(right - 96.0, rect.center().y), Align2::RIGHT_CENTER, fmt_size(hit.size), mono(11.0), t.mu);
        }

        // name + dimmed parent
        let x = rect.left() + 50.0;
        let max_w = right - 180.0 - x;
        let mut job = highlight(name, &self.words, t);
        job.wrap.max_width = max_w;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let g = ui.fonts(|f| f.layout_job(job));
        p.galley(egui::pos2(x, rect.top() + 3.0), g, t.tx);
        let mut pj = LayoutJob::simple_singleline(tilde(parent), mono(10.5), t.fa);
        pj.wrap.max_width = max_w;
        pj.wrap.max_rows = 1;
        pj.wrap.break_anywhere = true;
        let g = ui.fonts(|f| f.layout_job(pj));
        p.galley(egui::pos2(x, rect.top() + 20.0), g, t.fa);
    }

    fn status(&self, ui: &mut egui::Ui, t: &Theme) {
        ui.horizontal(|ui| {
            let small = |s: String, c: Color32| egui::RichText::new(s).size(11.5).color(c);
            if let Some(e) = &self.error {
                ui.label(small(e.clone(), t.ac));
            } else if let Load::Ready(idx) = &self.load {
                ui.label(small(
                    format!(
                        "{} matches · {} ms · {} files indexed",
                        self.total,
                        self.took_us / 1000,
                        fmt_count(idx.len())
                    ),
                    t.mu,
                ));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for (k, what) in [("esc", "clear"), ("⌘↵", "reveal"), ("↵", "open")] {
                    ui.label(small(what.into(), t.mu));
                    keycap(ui, t, k);
                    ui.add_space(6.0);
                }
            });
        });
    }
}

fn keycap(ui: &mut egui::Ui, t: &Theme, k: &str) {
    egui::Frame::none()
        .fill(t.panel)
        .stroke(Stroke::new(1.0_f32, t.line))
        .rounding(4.0)
        .inner_margin(egui::Margin::symmetric(5.0, 1.0))
        .show(ui, |ui| ui.label(egui::RichText::new(k).font(FontId::monospace(10.5)).color(t.mu)));
}

/// Name with query words in accent color (same rules as the mockup's `seg`).
fn highlight(name: &str, words: &[String], t: &Theme) -> LayoutJob {
    let font = FontId::proportional(14.0);
    let fmt = |c| TextFormat { font_id: font.clone(), color: c, ..Default::default() };
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
    const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
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
    fn formats() {
        assert_eq!(fmt_size(184 * 1024), "184 KB");
        assert_eq!(fmt_size(1536), "1.5 KB");
        assert_eq!(fmt_count(2_100_000), "2.1M");
        assert_eq!(fmt_date(0).len(), 4); // old file -> just the year
        let j = highlight("Invoice-2025.pdf", &["invoice".into(), "2025".into()], &theme::L);
        assert_eq!(j.sections.len(), 4);
        assert_eq!(j.sections[0].format.color, theme::L.ac);
    }
}
