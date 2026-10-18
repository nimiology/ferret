//! `ff -i`: interactive picker drawn on /dev/tty, chosen path on stdout.
//!
//! Keys: type to search (Space inserts a space), ↑/↓ or Ctrl-P/Ctrl-N move, Enter prints the path,
//! Tab cycles All/Files/Folders/Recent, Ctrl-O open, Ctrl-R reveal in Finder, Ctrl-Y copy,
//! Ctrl-L Quick Look, Esc/Ctrl-C quit (exit 1).

use std::fs::{File, OpenOptions};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use ferret_core::protocol::{KindFilter, SortArg};
use ferret_core::Index;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use crate::{match_mask, runs, search_daemon, search_local, Found};

const LIMIT: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    All,
    Files,
    Folders,
    Recent,
}

impl Tab {
    pub fn next(self) -> Tab {
        match self {
            Tab::All => Tab::Files,
            Tab::Files => Tab::Folders,
            Tab::Folders => Tab::Recent,
            Tab::Recent => Tab::All,
        }
    }
    fn kind(self) -> KindFilter {
        match self {
            Tab::Files => KindFilter::File,
            Tab::Folders => KindFilter::Dir,
            _ => KindFilter::All,
        }
    }
    fn sort(self) -> SortArg {
        if self == Tab::Recent {
            SortArg::Mtime
        } else {
            SortArg::Score
        }
    }
}

/// Everything the UI shows; no terminal needed, so it is unit-tested.
pub struct State {
    pub query: String,
    pub tab: Tab,
    pub gen: u64,
    pub paths: Vec<String>,
    pub total: usize,
    pub took_us: u64,
    pub error: Option<String>,
    pub sel: usize,
    pub offset: usize,
    pub words: Vec<String>,
}

impl State {
    pub fn new(query: &str, tab: Tab) -> State {
        State {
            query: query.into(),
            tab,
            gen: 0,
            paths: vec![],
            total: 0,
            took_us: 0,
            error: None,
            sel: 0,
            offset: 0,
            words: vec![],
        }
    }

    /// The query or tab changed: new generation, anything older is stale.
    pub fn bump(&mut self) -> u64 {
        self.gen += 1;
        self.words = ferret_core::parse(&self.query)
            .map(|q| q.words)
            .unwrap_or_default();
        self.gen
    }

    /// Apply a search result; false = stale, dropped.
    pub fn apply(&mut self, gen: u64, r: Result<Found, String>) -> bool {
        if gen != self.gen {
            return false;
        }
        match r {
            Ok(f) => {
                self.paths = f.hits.into_iter().map(|h| h.path).collect();
                (self.total, self.took_us, self.error) = (f.total, f.took_us, None);
            }
            Err(e) => (self.paths, self.total, self.error) = (vec![], 0, Some(e)),
        }
        (self.sel, self.offset) = (0, 0);
        true
    }

    pub fn move_sel(&mut self, d: isize, height: usize) {
        if self.paths.is_empty() {
            return;
        }
        self.sel = self.sel.saturating_add_signed(d).min(self.paths.len() - 1);
        if self.sel < self.offset {
            self.offset = self.sel;
        } else if height > 0 && self.sel >= self.offset + height {
            self.offset = self.sel + 1 - height;
        }
    }

    pub fn selected(&self) -> Option<&str> {
        self.paths.get(self.sel).map(String::as_str)
    }
}

enum Backend {
    Daemon,
    Local(Arc<OnceLock<Index>>),
}

enum Msg {
    Result(u64, Result<Found, String>),
    Loaded,
}

fn spawn_search(be: &Backend, st: &State, tx: &Sender<Msg>) {
    let (gen, q, tab, tx) = (st.gen, st.query.clone(), st.tab, tx.clone());
    let index = match be {
        Backend::Daemon => None,
        Backend::Local(cell) => match cell.get() {
            Some(_) => Some(cell.clone()),
            None => return, // still loading; Msg::Loaded re-triggers
        },
    };
    std::thread::spawn(move || {
        let r = match index {
            None => search_daemon(&q, LIMIT, tab.sort(), tab.kind())
                .map_err(|e| anyhow::anyhow!("daemon: {e}"))
                .and_then(|r| r),
            Some(i) => search_local(i.get().unwrap(), &q, LIMIT, tab.sort(), tab.kind()),
        };
        let _ = tx.send(Msg::Result(gen, r.map_err(|e| e.to_string())));
    });
}

fn spawn_detached(cmd: &str, args: &[&str]) {
    let _ = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn copy(s: &str) {
    use std::io::Write;
    if let Ok(mut c) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        let _ = c.stdin.take().map(|mut i| i.write_all(s.as_bytes()));
        let _ = c.wait();
    }
}

pub fn run(query: &str, dirs: bool) -> Result<()> {
    let (tx, rx) = channel();
    let be = if crate::daemon::socket_status().is_some() {
        Backend::Daemon
    } else {
        let cell = Arc::new(OnceLock::new());
        let (c, tx) = (cell.clone(), tx.clone());
        std::thread::spawn(move || match crate::load_snapshot() {
            Ok(i) => {
                let _ = c.set(i);
                let _ = tx.send(Msg::Loaded);
            }
            Err(e) => {
                let _ = tx.send(Msg::Result(0, Err(e.to_string())));
            }
        });
        Backend::Local(cell)
    };

    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("cannot open /dev/tty")?;
    enable_raw_mode()?;
    let mut out = tty.try_clone()?;
    execute!(out, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(tty))?;
    let res = event_loop(&mut term, &be, query, dirs, &tx, &rx);
    let _ = disable_raw_mode();
    let _ = execute!(out, LeaveAlternateScreen);
    match res? {
        Some(p) => {
            println!("{p}");
            Ok(())
        }
        None => std::process::exit(1),
    }
}

fn event_loop(
    term: &mut Terminal<CrosstermBackend<File>>,
    be: &Backend,
    query: &str,
    dirs: bool,
    tx: &Sender<Msg>,
    rx: &std::sync::mpsc::Receiver<Msg>,
) -> Result<Option<String>> {
    let mut st = State::new(query, if dirs { Tab::Folders } else { Tab::All });
    st.bump();
    spawn_search(be, &st, tx);
    let mut height = 0;
    loop {
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Result(0, Err(e)) => st.error = Some(e), // snapshot load failure
                Msg::Result(g, r) => {
                    st.apply(g, r);
                }
                Msg::Loaded => spawn_search(be, &st, tx),
            }
        }
        term.draw(|f| {
            let [q, list, status] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(f.area());
            height = list.height as usize;
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("> ", Style::new().fg(Color::Indexed(208)).bold()),
                    Span::raw(st.query.as_str()),
                ])),
                q,
            );
            f.set_cursor_position((q.x + 2 + st.query.chars().count() as u16, q.y));
            // Virtualized: only the rows that fit are turned into Lines.
            let rows: Vec<Line> = st
                .paths
                .iter()
                .enumerate()
                .skip(st.offset)
                .take(height)
                .map(|(i, p)| row(p, &st.words, i == st.sel))
                .collect();
            f.render_widget(Paragraph::new(rows), list);
            let tabs = [Tab::All, Tab::Files, Tab::Folders, Tab::Recent]
                .iter()
                .map(|t| {
                    let s = Span::raw(format!(" {t:?} "));
                    if *t == st.tab {
                        s.reversed()
                    } else {
                        s.dim()
                    }
                });
            let msg = match &st.error {
                Some(e) => e.clone(),
                None => format!("{} matches · {}", st.total, crate::fmt_ms(st.took_us)),
            };
            let mut spans: Vec<Span> = vec![Span::raw(msg).dim(), Span::raw("  ")];
            spans.extend(tabs);
            f.render_widget(Paragraph::new(Line::from(spans)), status);
        })?;

        if !event::poll(Duration::from_millis(30))? {
            continue;
        }
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind == KeyEventKind::Release {
            continue;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let mut changed = false;
        match (k.code, ctrl) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), true) => return Ok(None),
            (KeyCode::Enter, _) => {
                if let Some(p) = st.selected() {
                    return Ok(Some(p.to_string()));
                }
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), true) => st.move_sel(-1, height),
            (KeyCode::Down, _) | (KeyCode::Char('n'), true) => st.move_sel(1, height),
            (KeyCode::PageUp, _) => st.move_sel(-(height as isize), height),
            (KeyCode::PageDown, _) => st.move_sel(height as isize, height),
            (KeyCode::Tab, _) => {
                st.tab = st.tab.next();
                changed = true;
            }
            (KeyCode::Char(c), true) => {
                if let Some(p) = st.selected() {
                    match c {
                        'o' => spawn_detached("open", &[p]),
                        'r' => spawn_detached("open", &["-R", p]),
                        'y' => copy(p),
                        'l' => spawn_detached("qlmanage", &["-p", p]),
                        _ => {}
                    }
                }
            }
            (KeyCode::Char(c), false) => {
                st.query.push(c);
                changed = true;
            }
            (KeyCode::Backspace, _) => changed = st.query.pop().is_some(),
            _ => {}
        }
        if changed {
            st.bump();
            spawn_search(be, &st, tx);
        }
    }
}

fn row(path: &str, words: &[String], selected: bool) -> Line<'static> {
    let shown = crate::tilde(path);
    let (parent, name, hit) = match_mask(&shown, words);
    let mut spans = vec![Span::styled(
        parent.to_string(),
        Style::new().add_modifier(Modifier::DIM),
    )];
    for (t, on) in runs(name, &hit) {
        spans.push(if on {
            Span::styled(t.to_string(), Style::new().fg(Color::Indexed(208)).bold())
        } else {
            Span::raw(t.to_string())
        });
    }
    let line = Line::from(spans);
    if selected {
        line.style(Style::new().bg(Color::Indexed(236)))
    } else {
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(n: usize) -> Found {
        let hit = |i| ferret_core::protocol::HitOut {
            path: format!("/p{i}"),
            kind: ferret_core::Kind::File,
            size: 0,
            mtime: 0,
        };
        Found {
            hits: (0..n).map(hit).collect(),
            total: n,
            took_us: 4000,
        }
    }

    #[test]
    fn tabs_cycle() {
        let mut t = Tab::All;
        let seen: Vec<Tab> = (0..5)
            .map(|_| {
                t = t.next();
                t
            })
            .collect();
        assert_eq!(
            seen,
            [Tab::Files, Tab::Folders, Tab::Recent, Tab::All, Tab::Files]
        );
        assert_eq!(Tab::Folders.kind(), KindFilter::Dir);
        assert_eq!(Tab::Recent.sort(), SortArg::Mtime);
    }

    #[test]
    fn stale_results_dropped() {
        let mut st = State::new("inv", Tab::All);
        let g1 = st.bump();
        st.query.push('o');
        let g2 = st.bump();
        assert!(st.apply(g2, Ok(found(3))));
        assert!(
            !st.apply(g1, Ok(found(9))),
            "older generation must be ignored"
        );
        assert_eq!((st.paths.len(), st.total), (3, 3));
        assert_eq!(st.words, ["invo"]);
    }

    #[test]
    fn selection_scrolls_window() {
        let mut st = State::new("x", Tab::All);
        let g = st.bump();
        st.apply(g, Ok(found(10)));
        st.move_sel(-1, 4);
        assert_eq!(st.sel, 0);
        for _ in 0..5 {
            st.move_sel(1, 4);
        }
        assert_eq!((st.sel, st.offset), (5, 2));
        st.move_sel(100, 4);
        assert_eq!((st.sel, st.selected()), (9, Some("/p9")));
    }
}
