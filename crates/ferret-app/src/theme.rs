//! Color tokens from docs/design/Ferret.dc.html (`L`, `D`, `ACC.Orange`).
use eframe::egui::Color32;

pub struct Theme {
    pub panel: Color32,
    pub sub: Color32,
    pub line: Color32,
    pub tx: Color32,
    pub mu: Color32,
    pub fa: Color32,
    pub hl: Color32,
    pub ac: Color32,
    pub acs: Color32,
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub const L: Theme = Theme {
    panel: hex(0xFFFFFF),
    sub: hex(0xF6F5F3),
    line: hex(0xE4E2DD),
    tx: hex(0x1C1B19),
    mu: hex(0x6E6B65),
    fa: hex(0xA29E97),
    hl: hex(0xE9E7E3),
    ac: hex(0xC4501B),
    acs: hex(0xFBEBE2),
};

pub const D: Theme = Theme {
    panel: hex(0x19191B),
    sub: hex(0x202023),
    line: hex(0x2D2D31),
    tx: hex(0xECEAE6),
    mu: hex(0x9D9A94),
    fa: hex(0x67655F),
    hl: hex(0x2E2E33),
    ac: hex(0xFF9259),
    acs: hex(0x3A251B),
};

pub fn current(dark: bool) -> &'static Theme {
    if dark {
        &D
    } else {
        &L
    }
}
