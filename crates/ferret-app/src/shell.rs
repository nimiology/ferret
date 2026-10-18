//! Menu bar icon, global hotkey and app-level show/hide. Everything here must run on the main thread
//! after the event loop started (eframe's creation closure).
//!
//! Show/hide is done with NSApplication hide:/unhide: rather than ViewportCommand::Visible, because
//! eframe stops calling `update` for an invisible window, so nothing could bring it back. The hotkey
//! and menu callbacks fire on the main thread and act on NSApp directly; `update` keeps polling `rx`.
use std::sync::mpsc::{channel, Receiver};

use eframe::egui;
use global_hotkey::{hotkey::HotKey, GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use tray_icon::menu::{accelerator::Accelerator, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::config;

pub enum Cmd {
    /// Panel just became visible: focus the query.
    Shown,
    Settings,
}

pub struct Shell {
    _tray: Option<TrayIcon>,
    status: MenuItem,
    open: MenuItem,
    status_text: String,
    hotkeys: Option<GlobalHotKeyManager>,
    hotkey: Option<HotKey>,
    pub rx: Receiver<Cmd>,
}

fn app() -> Option<objc2::rc::Retained<NSApplication>> {
    MainThreadMarker::new().map(NSApplication::sharedApplication)
}

pub fn show() {
    if let Some(a) = app() {
        a.unhide(None);
        #[allow(deprecated)] // activate() is macOS 14+
        a.activateIgnoringOtherApps(true);
    }
}

pub fn hide() {
    if let Some(a) = app() {
        a.hide(None);
    }
}

fn visible() -> bool {
    app().is_some_and(|a| a.isActive() && !a.isHidden())
}

impl Shell {
    pub fn new(ctx: &egui::Context, hotkey: &str) -> Self {
        if let Some(a) = app() {
            a.setActivationPolicy(NSApplicationActivationPolicy::Accessory); // no Dock icon
        }
        let (tx, rx) = channel();

        let open = MenuItem::new("Open Ferret", true, None::<Accelerator>);
        let status = MenuItem::new("Starting…", false, None::<Accelerator>);
        let settings = MenuItem::new("Settings…", true, "cmd+comma".parse().ok());
        let quit = MenuItem::new("Quit Ferret", true, "cmd+q".parse().ok());
        let menu = Menu::new();
        let sep = PredefinedMenuItem::separator;
        let _ = menu.append_items(&[&open, &status, &sep(), &settings, &sep(), &quit]);

        let (rgba, n) = crate::icon::tray_rgba();
        let tray = TrayIconBuilder::new()
            .with_icon_templated(Icon::from_rgba(rgba, n, n).expect("valid icon"))
            .with_tooltip("Ferret")
            .with_menu(Box::new(menu))
            .build()
            .map_err(|e| eprintln!("ferret: tray icon unavailable: {e}"))
            .ok();

        let (open_id, settings_id, quit_id) = (open.id().clone(), settings.id().clone(), quit.id().clone());
        let (c, t) = (ctx.clone(), tx.clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if e.id == quit_id {
                std::process::exit(0);
            }
            if e.id == open_id || e.id == settings_id {
                show();
                let _ = t.send(if e.id == settings_id { Cmd::Settings } else { Cmd::Shown });
                c.request_repaint();
            }
        }));
        let c = ctx.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            if e.state != HotKeyState::Pressed {
                return;
            }
            if visible() {
                hide();
            } else {
                show();
                let _ = tx.send(Cmd::Shown);
            }
            c.request_repaint();
        }));

        let mut s = Self {
            _tray: tray,
            status,
            open,
            status_text: String::new(),
            hotkeys: GlobalHotKeyManager::new().map_err(|e| eprintln!("ferret: hotkeys unavailable: {e}")).ok(),
            hotkey: None,
            rx,
        };
        if let Err(e) = s.set_hotkey(hotkey) {
            eprintln!("ferret: hotkey {hotkey}: {e}; using {}", config::DEFAULT_HOTKEY);
            let _ = s.set_hotkey(config::DEFAULT_HOTKEY);
        }
        s
    }

    /// Swap the registered global hotkey. Err text is for the settings window.
    pub fn set_hotkey(&mut self, s: &str) -> Result<(), String> {
        let hk = config::parse_hotkey(s).ok_or_else(|| format!("Can't parse hotkey '{s}'"))?;
        let mgr = self.hotkeys.as_ref().ok_or("Global hotkeys unavailable")?;
        if let Some(old) = self.hotkey.take() {
            let _ = mgr.unregister(old);
        }
        mgr.register(hk).map_err(|e| e.to_string())?;
        self.hotkey = Some(hk);
        // Shown right-aligned like ⌘Q; menus only fire it while open, the real trigger is the global hotkey.
        let _ = self.open.set_accelerator(s.parse::<Accelerator>().ok());
        Ok(())
    }

    pub fn set_status(&mut self, s: String) {
        if s != self.status_text {
            self.status.set_text(&s);
            self.status_text = s;
        }
    }
}
