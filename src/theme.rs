//! Colour themes: the built-in palettes, the user's own `[themes.NAME]`,
//! and fitting colours to what the terminal can show.
//!
//! A theme fills every field of `Colors`. Themes with `bg`/`fg` set to
//! `Color::Reset` follow the terminal's own background and text colour;
//! the named palettes (Catppuccin, Gruvbox, ...) set both, so they look the
//! same on any terminal.

use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::str::FromStr;

/// Colour theme: a theme name plus optional per-colour overrides that
/// apply on top of whichever theme is active.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    /// "auto", a built-in theme (`theme::BUILTIN`) or one of `[themes]`.
    pub preset: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_bg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marked: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_accent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dim: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            preset: "auto".into(),
            accent: None,
            selected_bg: None,
            installed: None,
            marked: None,
            update: None,
            error: None,
            on_accent: None,
            muted: None,
            dim: None,
            bg: None,
            fg: None,
        }
    }
}

/// One of the user's own themes: a built-in base plus changed colours.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserTheme {
    /// A built-in theme to start from; "dark" when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// "dark", "light" or "terminal" for the picker; the base's when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Show the selection reversed; the base's choice when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverse_selection: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_bg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marked: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_accent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dim: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
}

/// Resolved colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub accent: Color,
    pub selected_bg: Color,
    pub installed: Color,
    pub marked: Color,
    pub update: Color,
    pub error: Color,
    /// Text on accent-coloured buttons and key hints.
    pub on_accent: Color,
    /// Inactive buttons and secondary labels.
    pub muted: Color,
    /// Dim text: versions, borders, hints.
    pub dim: Color,
    /// Background and normal text; `Color::Reset` keeps the terminal's.
    pub bg: Color,
    pub fg: Color,
    /// Show the selection reversed instead of with `selected_bg` (themes
    /// that use only the terminal's own colours, or none).
    pub reverse_selection: bool,
}

pub(crate) fn color(s: &str, fallback: Color) -> Color {
    parse_color(s).unwrap_or(fallback)
}

/// A colour name ("cyan"), index ("42") or hex value ("#89b4fa").
pub fn parse_color(s: &str) -> Option<Color> {
    Color::from_str(s).ok()
}

/// (key, value) pairs of the colour overrides, for `theme::resolve`.
macro_rules! overrides {
    ($s:ident) => {
        [
            ("accent", &$s.accent),
            ("selected_bg", &$s.selected_bg),
            ("installed", &$s.installed),
            ("marked", &$s.marked),
            ("update", &$s.update),
            ("error", &$s.error),
            ("on_accent", &$s.on_accent),
            ("muted", &$s.muted),
            ("dim", &$s.dim),
            ("bg", &$s.bg),
            ("fg", &$s.fg),
        ]
    };
}

impl Theme {
    pub fn overrides(&self) -> [(&'static str, &Option<String>); 11] {
        overrides!(self)
    }

    /// Any single colour overridden in `[theme]`.
    pub fn has_overrides(&self) -> bool {
        self.overrides().iter().any(|(_, v)| v.is_some())
    }
}

impl UserTheme {
    pub fn overrides(&self) -> [(&'static str, &Option<String>); 11] {
        overrides!(self)
    }
}

/// Whether a theme is made for a dark or a light background, or follows
/// the terminal's own colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dark,
    Light,
    Terminal,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        [Kind::Dark, Kind::Light, Kind::Terminal]
            .into_iter()
            .find(|k| k.label() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Dark => "dark",
            Kind::Light => "light",
            Kind::Terminal => "terminal",
        }
    }
}

pub struct Builtin {
    pub name: &'static str,
    pub kind: Kind,
    pub colors: Colors,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// The colours a theme does not set: follow the terminal.
const PLAIN: Colors = Colors {
    accent: Color::Reset,
    selected_bg: Color::Reset,
    installed: Color::Reset,
    marked: Color::Reset,
    update: Color::Reset,
    error: Color::Reset,
    on_accent: Color::Reset,
    muted: Color::Reset,
    dim: Color::Reset,
    bg: Color::Reset,
    fg: Color::Reset,
    reverse_selection: false,
};

/// Built-in themes, in the order the picker shows them.
pub const BUILTIN: &[Builtin] = &[
    Builtin {
        name: "dark",
        kind: Kind::Dark,
        colors: Colors {
            accent: Color::Cyan,
            // Not DarkGray: that is `dim`, and dim text on the selected row
            // would vanish.
            selected_bg: Color::Indexed(238),
            installed: Color::Green,
            marked: Color::Yellow,
            update: Color::Magenta,
            error: Color::Red,
            on_accent: Color::Black,
            muted: Color::Gray,
            dim: Color::DarkGray,
            ..PLAIN
        },
    },
    Builtin {
        name: "light",
        kind: Kind::Light,
        colors: Colors {
            accent: Color::Blue,
            selected_bg: Color::Indexed(252),
            installed: Color::Indexed(28),
            marked: Color::Indexed(130),
            update: Color::Magenta,
            error: Color::Red,
            on_accent: Color::White,
            muted: Color::Indexed(243),
            dim: Color::Indexed(245),
            ..PLAIN
        },
    },
    Builtin {
        name: "high-contrast",
        kind: Kind::Dark,
        colors: Colors {
            accent: Color::Yellow,
            selected_bg: Color::Blue,
            installed: Color::LightGreen,
            marked: Color::LightMagenta,
            update: Color::LightCyan,
            error: Color::LightRed,
            on_accent: Color::Black,
            muted: Color::White,
            dim: Color::Gray,
            ..PLAIN
        },
    },
    // Only the 16 ANSI colours, so it takes on the terminal's own colour
    // scheme; the selection is shown reversed.
    Builtin {
        name: "terminal",
        kind: Kind::Terminal,
        colors: Colors {
            accent: Color::Blue,
            installed: Color::Green,
            marked: Color::Yellow,
            update: Color::Magenta,
            error: Color::Red,
            on_accent: Color::Black,
            muted: Color::Gray,
            dim: Color::DarkGray,
            reverse_selection: true,
            ..PLAIN
        },
    },
    // No colour at all (also used for NO_COLOR): selection reversed,
    // meaning carried by markers and words.
    Builtin {
        name: "monochrome",
        kind: Kind::Terminal,
        colors: Colors {
            reverse_selection: true,
            ..PLAIN
        },
    },
    // Okabe-Ito colours, distinguishable with the common colour vision
    // deficiencies: installed, update and error never rely on red vs green.
    Builtin {
        name: "colorblind",
        kind: Kind::Dark,
        colors: Colors {
            accent: rgb(0x56, 0xb4, 0xe9),
            selected_bg: rgb(0x3a, 0x3a, 0x3a),
            installed: rgb(0x00, 0x9e, 0x73),
            marked: rgb(0xf0, 0xe4, 0x42),
            update: rgb(0xcc, 0x79, 0xa7),
            error: rgb(0xd5, 0x5e, 0x00),
            on_accent: rgb(0x00, 0x00, 0x00),
            muted: rgb(0xbd, 0xbd, 0xbd),
            dim: rgb(0x8a, 0x8a, 0x8a),
            ..PLAIN
        },
    },
    Builtin {
        name: "colorblind-light",
        kind: Kind::Light,
        colors: Colors {
            accent: rgb(0x00, 0x72, 0xb2),
            selected_bg: rgb(0xdc, 0xdc, 0xdc),
            installed: rgb(0x00, 0x9b, 0x71), // Okabe-Ito green, a shade darker (256-colour contrast)
            marked: rgb(0xb0, 0x78, 0x00),    // Okabe-Ito orange, darkened for white
            update: rgb(0xc2, 0x73, 0x9f),    // Okabe-Ito purple, a shade darker (256-colour contrast)
            error: rgb(0xd5, 0x5e, 0x00),
            on_accent: rgb(0xff, 0xff, 0xff),
            muted: rgb(0x5a, 0x5a, 0x5a),
            dim: rgb(0x76, 0x76, 0x76),
            ..PLAIN
        },
    },
    Builtin {
        name: "catppuccin-mocha",
        kind: Kind::Dark,
        colors: Colors {
            accent: rgb(0x89, 0xb4, 0xfa),      // blue
            selected_bg: rgb(0x45, 0x47, 0x5a), // surface1
            installed: rgb(0xa6, 0xe3, 0xa1),   // green
            marked: rgb(0xf9, 0xe2, 0xaf),      // yellow
            update: rgb(0xcb, 0xa6, 0xf7),      // mauve
            error: rgb(0xf3, 0x8b, 0xa8),       // red
            on_accent: rgb(0x11, 0x11, 0x1b),   // crust
            muted: rgb(0xa6, 0xad, 0xc8),       // subtext0
            dim: rgb(0x7f, 0x84, 0x9c),         // overlay1
            bg: rgb(0x1e, 0x1e, 0x2e),          // base
            fg: rgb(0xcd, 0xd6, 0xf4),          // text
            reverse_selection: false,
        },
    },
    Builtin {
        name: "catppuccin-latte",
        kind: Kind::Light,
        colors: Colors {
            accent: rgb(0x1e, 0x66, 0xf5),      // blue
            selected_bg: rgb(0xcc, 0xd0, 0xda), // surface0
            installed: rgb(0x3e, 0x9b, 0x2a),   // green, darker for contrast (also in 256 colours)
            marked: rgb(0xea, 0x5c, 0x0a),      // peach, darker for contrast (also in 256 colours)
            update: rgb(0x88, 0x39, 0xef),      // mauve
            error: rgb(0xd2, 0x0f, 0x39),       // red
            on_accent: rgb(0xef, 0xf1, 0xf5),   // base
            muted: rgb(0x6c, 0x6f, 0x85),       // subtext0
            dim: rgb(0x7c, 0x7f, 0x93),         // overlay2 (overlay1 is too faint)
            bg: rgb(0xef, 0xf1, 0xf5),          // base
            fg: rgb(0x4c, 0x4f, 0x69),          // text
            reverse_selection: false,
        },
    },
    Builtin {
        name: "gruvbox-dark",
        kind: Kind::Dark,
        colors: Colors {
            accent: rgb(0x83, 0xa5, 0x98),      // blue
            selected_bg: rgb(0x50, 0x49, 0x45), // bg2
            installed: rgb(0xb8, 0xbb, 0x26),   // green
            marked: rgb(0xfa, 0xbd, 0x2f),      // yellow
            update: rgb(0xd3, 0x86, 0x9b),      // purple
            error: rgb(0xfb, 0x49, 0x34),       // red
            on_accent: rgb(0x28, 0x28, 0x28),   // bg
            muted: rgb(0xa8, 0x99, 0x84),       // fg4
            dim: rgb(0x92, 0x83, 0x74),         // gray
            bg: rgb(0x28, 0x28, 0x28),          // bg
            fg: rgb(0xeb, 0xdb, 0xb2),          // fg
            reverse_selection: false,
        },
    },
    Builtin {
        name: "gruvbox-light",
        kind: Kind::Light,
        colors: Colors {
            accent: rgb(0x07, 0x66, 0x78),      // blue
            selected_bg: rgb(0xd5, 0xc4, 0xa1), // bg2
            installed: rgb(0x79, 0x74, 0x0e),   // green
            marked: rgb(0xb5, 0x76, 0x14),      // yellow
            update: rgb(0x8f, 0x3f, 0x71),      // purple
            error: rgb(0x9d, 0x00, 0x06),       // red
            on_accent: rgb(0xfb, 0xf1, 0xc7),   // bg
            muted: rgb(0x7c, 0x6f, 0x64),       // fg4
            dim: rgb(0x92, 0x83, 0x74),         // gray
            bg: rgb(0xfb, 0xf1, 0xc7),          // bg
            fg: rgb(0x3c, 0x38, 0x36),          // fg
            reverse_selection: false,
        },
    },
    Builtin {
        name: "nord",
        kind: Kind::Dark,
        colors: Colors {
            accent: rgb(0x88, 0xc0, 0xd0),      // nord8
            selected_bg: rgb(0x43, 0x4c, 0x5e), // nord2
            installed: rgb(0xa3, 0xbe, 0x8c),   // nord14
            marked: rgb(0xeb, 0xcb, 0x8b),      // nord13
            update: rgb(0xb4, 0x8e, 0xad),      // nord15
            error: rgb(0xc4, 0x6e, 0x76),       // nord11, lighter (256-colour contrast)
            on_accent: rgb(0x2e, 0x34, 0x40),   // nord0
            muted: rgb(0xd8, 0xde, 0xe9),       // nord4
            dim: rgb(0x7b, 0x88, 0xa1),         // between nord3 and nord4, for contrast
            bg: rgb(0x2e, 0x34, 0x40),          // nord0
            fg: rgb(0xd8, 0xde, 0xe9),          // nord4
            reverse_selection: false,
        },
    },
    Builtin {
        name: "dracula",
        kind: Kind::Dark,
        colors: Colors {
            accent: rgb(0xbd, 0x93, 0xf9),      // purple
            selected_bg: rgb(0x44, 0x47, 0x5a), // current line
            installed: rgb(0x50, 0xfa, 0x7b),   // green
            marked: rgb(0xf1, 0xfa, 0x8c),      // yellow
            update: rgb(0xff, 0x79, 0xc6),      // pink
            error: rgb(0xff, 0x55, 0x55),       // red
            on_accent: rgb(0x28, 0x2a, 0x36),   // background
            muted: rgb(0xbf, 0xbf, 0xbf),
            dim: rgb(0x65, 0x75, 0xa6), // comment, lighter (256-colour contrast)
            bg: rgb(0x28, 0x2a, 0x36),  // background
            fg: rgb(0xf8, 0xf8, 0xf2),  // foreground
            reverse_selection: false,
        },
    },
    Builtin {
        name: "solarized-dark",
        kind: Kind::Dark,
        colors: Colors {
            accent: rgb(0x26, 0x8b, 0xd2),      // blue
            selected_bg: rgb(0x07, 0x36, 0x42), // base02
            installed: rgb(0x85, 0x99, 0x00),   // green
            marked: rgb(0xb5, 0x89, 0x00),      // yellow
            update: rgb(0xd3, 0x36, 0x82),      // magenta
            error: rgb(0xdc, 0x32, 0x2f),       // red
            on_accent: rgb(0x00, 0x2b, 0x36),   // base03
            muted: rgb(0x93, 0xa1, 0xa1),       // base1
            dim: rgb(0x5f, 0x74, 0x7b),         // base01, lighter for contrast
            bg: rgb(0x00, 0x2b, 0x36),          // base03
            fg: rgb(0x93, 0xa1, 0xa1),          // base1 (base0 is too faint on the selection)
            reverse_selection: false,
        },
    },
    Builtin {
        name: "solarized-light",
        kind: Kind::Light,
        colors: Colors {
            accent: rgb(0x26, 0x8b, 0xd2),      // blue
            selected_bg: rgb(0xee, 0xe8, 0xd5), // base2
            installed: rgb(0x84, 0x97, 0x00),   // green, a shade darker for contrast
            marked: rgb(0xb3, 0x88, 0x00),      // yellow, a shade darker for contrast
            update: rgb(0xd3, 0x36, 0x82),      // magenta
            error: rgb(0xdc, 0x32, 0x2f),       // red
            on_accent: rgb(0xfd, 0xf6, 0xe3),   // base3
            muted: rgb(0x58, 0x6e, 0x75),       // base01
            dim: rgb(0x84, 0x91, 0x91),         // base1, darker for contrast
            bg: rgb(0xfd, 0xf6, 0xe3),          // base3
            fg: rgb(0x56, 0x6c, 0x73),          // base01, darker (readable on the selection)
            reverse_selection: false,
        },
    },
    Builtin {
        name: "tokyo-night",
        kind: Kind::Dark,
        colors: Colors {
            accent: rgb(0x7a, 0xa2, 0xf7),      // blue
            selected_bg: rgb(0x32, 0x45, 0x7a), // bg_visual, a shade darker (256-colour contrast)
            installed: rgb(0x9e, 0xce, 0x6a),   // green
            marked: rgb(0xe0, 0xaf, 0x68),      // yellow
            update: rgb(0xbb, 0x9a, 0xf7),      // magenta
            error: rgb(0xf7, 0x76, 0x8e),       // red
            on_accent: rgb(0x1a, 0x1b, 0x26),   // bg
            muted: rgb(0xa9, 0xb1, 0xd6),       // fg_dark
            dim: rgb(0x73, 0x7a, 0xa2),         // dark5 (comment is too faint)
            bg: rgb(0x1a, 0x1b, 0x26),          // bg
            fg: rgb(0xc0, 0xca, 0xf5),          // fg
            reverse_selection: false,
        },
    },
];

/// Colours that follow the terminal entirely (no theme resolved yet).
pub const fn fallback() -> Colors {
    PLAIN
}

pub fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTIN.iter().find(|b| b.name == name)
}

/// `preset = "auto"`: no colour with NO_COLOR, else the terminal's
/// background decides between "dark" and "light".
pub fn auto(no_color: bool, light_terminal: bool) -> &'static str {
    match (no_color, light_terminal) {
        (true, _) => "monochrome",
        (false, true) => "light",
        (false, false) => "dark",
    }
}

/// Every named theme the picker offers: the built-ins, then the user's
/// own (sorted by name). ("auto" is offered by the app, which knows what
/// it resolved to.)
pub fn all(themes: &BTreeMap<String, UserTheme>) -> Vec<(String, Kind)> {
    let mut v: Vec<(String, Kind)> = BUILTIN.iter().map(|b| (b.name.to_string(), b.kind)).collect();
    for (name, t) in themes {
        let kind = t
            .kind
            .as_deref()
            .and_then(Kind::parse)
            .or_else(|| builtin(t.base.as_deref().unwrap_or("dark")).map(|b| b.kind))
            .unwrap_or(Kind::Dark);
        v.push((name.clone(), kind));
    }
    v
}

/// The colours of theme `name` (built-in or the user's own), with the
/// `[theme]` overrides on top, fitted to the terminal. `None` for an
/// unknown name.
pub fn resolve(theme: &Theme, themes: &BTreeMap<String, UserTheme>, name: &str, truecolor: bool) -> Option<Colors> {
    let base = match (builtin(name), themes.get(name)) {
        (Some(b), _) => b.colors,
        (None, Some(user)) => {
            let from = builtin(user.base.as_deref().unwrap_or("dark"))?;
            let mut c = apply(from.colors, &user.overrides());
            if let Some(r) = user.reverse_selection {
                c.reverse_selection = r;
            }
            c
        }
        (None, None) => return None,
    };
    let colors = apply(base, &theme.overrides());
    Some(if truecolor { colors } else { fit_256(colors) })
}

/// Override the named colours that are set (and parse).
fn apply(mut c: Colors, overrides: &[(&str, &Option<String>)]) -> Colors {
    for (key, value) in overrides {
        let Some(value) = value.as_deref() else { continue };
        let slot = match *key {
            "accent" => &mut c.accent,
            "selected_bg" => &mut c.selected_bg,
            "installed" => &mut c.installed,
            "marked" => &mut c.marked,
            "update" => &mut c.update,
            "error" => &mut c.error,
            "on_accent" => &mut c.on_accent,
            "muted" => &mut c.muted,
            "dim" => &mut c.dim,
            "bg" => &mut c.bg,
            "fg" => &mut c.fg,
            _ => continue,
        };
        *slot = color(value, *slot);
    }
    c
}

/// The fixed RGB value of an xterm-256 colour; `None` for 0 to 15, which
/// every terminal defines for itself.
fn indexed_rgb(i: u8) -> Option<(u8, u8, u8)> {
    match i {
        0..=15 => None,
        232..=255 => {
            let v = i.saturating_sub(232).saturating_mul(10).saturating_add(8);
            Some((v, v, v))
        }
        _ => {
            let n = i.saturating_sub(16);
            let level = |k: u8| CUBE.get(usize::from(k)).copied().unwrap_or(0);
            let r = n.checked_div(36).unwrap_or(0);
            let g = n.checked_div(6).unwrap_or(0).checked_rem(6).unwrap_or(0);
            let b = n.checked_rem(6).unwrap_or(0);
            Some((level(r), level(g), level(b)))
        }
    }
}

/// WCAG relative luminance of an RGB colour (or a fixed xterm-256 one).
fn luminance(c: Color) -> Option<f64> {
    let (r, g, b) = match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(i) => indexed_rgb(i)?,
        _ => return None,
    };
    let ch = |v: u8| {
        let v = f64::from(v) / 255.0;
        if v <= 0.039_28 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    Some(0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b))
}

/// WCAG contrast ratio of two colours (1 to 21); `None` for colours whose
/// shade depends on the terminal (named ones and indexed 0 to 15).
pub fn contrast(a: Color, b: Color) -> Option<f64> {
    let (la, lb) = (luminance(a)?, luminance(b)?);
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    Some((hi + 0.05) / (lo + 0.05))
}

/// Text for a label on `bg` (the DRY RUN badge): black or white, whichever
/// reads better; `fallback` when `bg`'s shade is up to the terminal. Black
/// and white are xterm-256 colours 16 and 231, the same with or without
/// 24-bit colour.
pub fn text_on(bg: Color, fallback: Color) -> Color {
    let black = Color::Indexed(16);
    let white = Color::Indexed(231);
    match (contrast(black, bg), contrast(white, bg)) {
        (Some(b), Some(w)) if b >= w => black,
        (Some(_), Some(_)) => white,
        _ => fallback,
    }
}

/// Can the terminal show 24-bit colour? Terminals that can say so in
/// COLORTERM; others get the nearest of the 256 standard colours.
pub fn truecolor() -> bool {
    std::env::var("COLORTERM").is_ok_and(|v| {
        let v = v.to_ascii_lowercase();
        v.contains("truecolor") || v.contains("24bit")
    })
}

/// NO_COLOR (https://no-color.org): set and not empty.
pub fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

/// Every RGB colour replaced by the nearest of the 256 standard colours.
pub fn fit_256(c: Colors) -> Colors {
    let f = |col: Color| match col {
        Color::Rgb(r, g, b) => Color::Indexed(nearest_256(r, g, b)),
        other => other,
    };
    Colors {
        accent: f(c.accent),
        selected_bg: f(c.selected_bg),
        installed: f(c.installed),
        marked: f(c.marked),
        update: f(c.update),
        error: f(c.error),
        on_accent: f(c.on_accent),
        muted: f(c.muted),
        dim: f(c.dim),
        bg: f(c.bg),
        fg: f(c.fg),
        reverse_selection: c.reverse_selection,
    }
}

/// Levels of the 6x6x6 colour cube (indices 16..=231).
const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// The nearest xterm-256 colour: the best cube colour or grey (232..=255).
/// The first 16 are left out: terminals redefine them.
pub fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    let dist = |(r2, g2, b2): (u8, u8, u8)| -> u32 {
        let d = |a: u8, b: u8| u32::from(a.abs_diff(b)).pow(2);
        d(r, r2).saturating_add(d(g, g2)).saturating_add(d(b, b2))
    };
    let level = |v: u8| -> (u8, u8) {
        // (index into CUBE, level value)
        CUBE.iter()
            .enumerate()
            .min_by_key(|(_, l)| l.abs_diff(v))
            .map_or((0, 0), |(i, l)| (u8::try_from(i).unwrap_or(0), *l))
    };
    let (ri, rl) = level(r);
    let (gi, gl) = level(g);
    let (bi, bl) = level(b);
    let cube_index = 16u8
        .saturating_add(ri.saturating_mul(36))
        .saturating_add(gi.saturating_mul(6))
        .saturating_add(bi);
    let cube = (cube_index, dist((rl, gl, bl)));
    // Greys 232..=255 are 8, 18, ..., 238.
    let grey = (0u8..24)
        .map(|i| {
            let v = i.saturating_mul(10).saturating_add(8);
            (232u8.saturating_add(i), dist((v, v, v)))
        })
        .min_by_key(|(_, d)| *d)
        .unwrap_or(cube);
    if grey.1 < cube.1 { grey.0 } else { cube.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_every_theme_is_readable() {
        for (i, b) in BUILTIN.iter().enumerate() {
            assert!(BUILTIN.iter().skip(i + 1).all(|o| o.name != b.name), "{} twice", b.name);
            let c = b.colors;
            if c.reverse_selection {
                continue;
            }
            assert_ne!(c.selected_bg, c.bg, "{}", b.name);
            assert_ne!(c.accent, c.selected_bg, "{}", b.name);
            assert_ne!(c.on_accent, c.accent, "{}", b.name);
            assert_ne!(c.on_accent, c.muted, "{}", b.name);
        }
    }

    /// WCAG contrast for every palette with exact colours. Themes that
    /// follow the terminal's background are measured against black (dark)
    /// or white (light). Dim text is never drawn on the selection: the
    /// selected row switches it to the normal text colour.
    #[test]
    fn palettes_have_enough_contrast() {
        let black = Color::Rgb(0, 0, 0);
        let white = Color::Rgb(255, 255, 255);
        // As designed (24-bit), and as shown without 24-bit colour.
        let versions = BUILTIN
            .iter()
            .flat_map(|b| [(b, b.colors, "24-bit"), (b, fit_256(b.colors), "256")]);
        for (b, c, depth) in versions {
            let assumed = if b.kind == Kind::Light { white } else { black };
            let bg = if c.bg == Color::Reset { assumed } else { c.bg };
            let fg = if c.fg == Color::Reset {
                if b.kind == Kind::Light { black } else { white }
            } else {
                c.fg
            };
            let checks = [
                ("dim on bg", c.dim, bg, 3.0),
                ("accent on bg", c.accent, bg, 3.0),
                ("installed on bg", c.installed, bg, 3.0),
                ("marked on bg", c.marked, bg, 3.0),
                ("update on bg", c.update, bg, 3.0),
                ("error on bg", c.error, bg, 3.0),
                ("text on the selection", fg, c.selected_bg, 4.5),
                ("button text on accent", c.on_accent, c.accent, 3.0),
                ("button text on muted", c.on_accent, c.muted, 3.0),
                ("DRY RUN badge", text_on(c.marked, c.on_accent), c.marked, 4.5),
            ];
            for (what, f, back, min) in checks {
                if let Some(ratio) = contrast(f, back) {
                    assert!(
                        ratio >= min,
                        "{} ({depth}): {what} is {ratio:.2}:1, needs {min}:1",
                        b.name
                    );
                }
            }
        }
    }

    #[test]
    fn contrast_and_badge_text() {
        let black = Color::Indexed(16);
        let white = Color::Indexed(231);
        assert_eq!(indexed_rgb(16), Some((0, 0, 0)));
        assert_eq!(indexed_rgb(231), Some((255, 255, 255)));
        assert_eq!(indexed_rgb(196), Some((255, 0, 0)));
        assert_eq!(indexed_rgb(232), Some((8, 8, 8)));
        assert_eq!(indexed_rgb(255), Some((238, 238, 238)));
        assert_eq!(indexed_rgb(9), None, "0 to 15 depend on the terminal");
        // The fitted catppuccin-latte badge (peach -> 202) wants black.
        assert_eq!(text_on(Color::Indexed(202), Color::White), black);
        assert!((contrast(black, white).unwrap() - 21.0).abs() < 0.01);
        assert_eq!(
            contrast(Color::Red, white),
            None,
            "named colours depend on the terminal"
        );
        assert_eq!(
            text_on(Color::Rgb(0xf0, 0xe4, 0x42), Color::Red),
            black,
            "yellow wants black"
        );
        assert_eq!(
            text_on(Color::Rgb(0x00, 0x48, 0x8b), Color::Red),
            white,
            "dark blue wants white"
        );
        assert_eq!(
            text_on(Color::Yellow, Color::Black),
            Color::Black,
            "terminal colour: fallback"
        );
    }

    #[test]
    fn palettes_set_their_own_background() {
        for name in ["catppuccin-mocha", "gruvbox-light", "nord", "dracula", "tokyo-night"] {
            let b = builtin(name).unwrap();
            assert!(matches!(b.colors.bg, Color::Rgb(..)), "{name}");
            assert!(matches!(b.colors.fg, Color::Rgb(..)), "{name}");
        }
        // The terminal-following themes leave it to the terminal.
        for name in ["dark", "light", "terminal", "monochrome", "colorblind"] {
            assert_eq!(builtin(name).unwrap().colors.bg, Color::Reset, "{name}");
        }
    }

    #[test]
    fn nearest_256_colours() {
        assert_eq!(nearest_256(0, 0, 0), 16);
        assert_eq!(nearest_256(255, 255, 255), 231);
        assert_eq!(nearest_256(255, 0, 0), 196);
        // A grey is nearer on the grey ramp than in the cube.
        assert_eq!(nearest_256(0x1e, 0x1e, 0x1e), 234);
        // Catppuccin blue (#89b4fa) lands on a light blue.
        assert_eq!(nearest_256(0x89, 0xb4, 0xfa), 111);
        let fitted = fit_256(builtin("nord").unwrap().colors);
        assert!(!matches!(fitted.accent, Color::Rgb(..)));
        assert!(!matches!(fitted.bg, Color::Rgb(..)));
        // ANSI colours are left alone.
        assert_eq!(
            fit_256(builtin("dark").unwrap().colors),
            builtin("dark").unwrap().colors
        );
    }

    #[test]
    fn auto_choice() {
        assert_eq!(auto(true, true), "monochrome");
        assert_eq!(auto(false, true), "light");
        assert_eq!(auto(false, false), "dark");
    }

    #[test]
    fn resolve_builtin_user_and_overrides() {
        use crate::config::Config;
        let resolve = |cfg: &Config, name: &str, tc: bool| super::resolve(&cfg.theme, &cfg.themes, name, tc);
        let all = |cfg: &Config| super::all(&cfg.themes);
        let cfg: Config = toml::from_str(
            "[theme]\npreset = \"mine\"\nerror = \"#ff0000\"\n\
             [themes.mine]\nbase = \"nord\"\naccent = \"#123456\"\n",
        )
        .unwrap();
        let mine = resolve(&cfg, "mine", true).unwrap();
        let nord = builtin("nord").unwrap().colors;
        assert_eq!(mine.accent, rgb(0x12, 0x34, 0x56), "the user's own colour");
        assert_eq!(mine.bg, nord.bg, "the rest from its base");
        assert_eq!(mine.error, rgb(0xff, 0, 0), "[theme] overrides apply on top");
        assert_eq!(resolve(&cfg, "nord", true).unwrap().error, rgb(0xff, 0, 0));
        assert!(resolve(&cfg, "nope", true).is_none());
        let names: Vec<String> = all(&cfg).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names.last().map(String::as_str), Some("mine"));
        assert!(names.contains(&"tokyo-night".to_string()));
        // Without true colour, the user's RGB colours are fitted too.
        assert!(matches!(
            resolve(&cfg, "mine", false).unwrap().accent,
            Color::Indexed(_)
        ));
        // kind and reverse_selection can be set for one's own theme.
        let cfg: Config = toml::from_str(
            "[themes.paper]\nbase = \"dark\"\nkind = \"light\"\nreverse_selection = true\nbg = \"#ffffff\"\n",
        )
        .unwrap();
        assert_eq!(all(&cfg).last(), Some(&("paper".to_string(), Kind::Light)));
        assert!(resolve(&cfg, "paper", true).unwrap().reverse_selection);
    }
}
