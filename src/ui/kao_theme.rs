//! Kao Wallet palette — three themes: Petal (pink/lavender), Mint (teal/lime),
//! and Void (dark). Colors are sRGB approximations of the OKLCH values in the
//! original HTML mock.

use iced::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeKind {
    Petal,
    Mint,
    Void,
}

impl ThemeKind {
    pub const ALL: [ThemeKind; 3] = [ThemeKind::Petal, ThemeKind::Mint, ThemeKind::Void];

    pub fn key(&self) -> &'static str {
        match self {
            ThemeKind::Petal => "petal",
            ThemeKind::Mint => "mint",
            ThemeKind::Void => "void",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        match s {
            "petal" => Some(ThemeKind::Petal),
            "mint" => Some(ThemeKind::Mint),
            "void" => Some(ThemeKind::Void),
            _ => None,
        }
    }

    /// Swatch color used for the theme-picker dots in the sidebar.
    pub fn swatch(&self) -> Color {
        match self {
            ThemeKind::Petal => rgb(0xDE, 0x5A, 0x9D),
            ThemeKind::Mint => rgb(0x2F, 0xAF, 0x9E),
            ThemeKind::Void => rgb(0x1E, 0x23, 0x35),
        }
    }
}

/// Full palette for one theme.
#[derive(Debug, Clone, Copy)]
pub struct KaoTheme {
    #[allow(dead_code)]
    pub name: &'static str,
    #[allow(dead_code)]
    pub icon: &'static str,
    pub bg: Color,
    pub sidebar: Color,
    pub card: Color,
    pub card_alt: Color,
    pub a1: Color,
    pub a2: Color,
    pub a3: Color,
    pub ab1: Color,
    pub ab2: Color,
    pub ab3: Color,
    pub text: Color,
    pub sub: Color,
    pub border: Color,
    pub up: Color,
    pub down: Color,
    pub dark: bool,
}

impl KaoTheme {
    pub fn for_kind(kind: ThemeKind) -> Self {
        match kind {
            ThemeKind::Petal => Self::petal(),
            ThemeKind::Mint => Self::mint(),
            ThemeKind::Void => Self::void(),
        }
    }

    fn petal() -> Self {
        Self {
            name: "Petal",
            icon: "(｡♥‿♥｡)",
            bg: rgb(0xFB, 0xF5, 0xFA),
            sidebar: rgb(0xF1, 0xE4, 0xEE),
            card: rgb(0xFF, 0xFF, 0xFF),
            card_alt: rgb(0xF4, 0xEB, 0xF1),
            a1: rgb(0xD2, 0x5B, 0x9B),
            a2: rgb(0x8C, 0x79, 0xD3),
            a3: rgb(0x4F, 0xB0, 0x92),
            ab1: rgb(0xF7, 0xDC, 0xE8),
            ab2: rgb(0xE6, 0xDF, 0xF4),
            ab3: rgb(0xDA, 0xEE, 0xE5),
            text: rgb(0x2B, 0x1F, 0x33),
            sub: rgb(0x7E, 0x6F, 0x81),
            border: rgb(0xE3, 0xD8, 0xE1),
            up: rgb(0x4E, 0xA8, 0x6B),
            down: rgb(0xDD, 0x6E, 0x5F),
            dark: false,
        }
    }

    fn mint() -> Self {
        Self {
            name: "Mint",
            icon: "( ´ ▽ ` )ﾉ",
            bg: rgb(0xF5, 0xFB, 0xF9),
            sidebar: rgb(0xDE, 0xEE, 0xEA),
            card: rgb(0xFF, 0xFF, 0xFF),
            card_alt: rgb(0xEC, 0xF5, 0xF2),
            a1: rgb(0x1F, 0xAD, 0x99),
            a2: rgb(0x52, 0x9B, 0xCF),
            a3: rgb(0x6E, 0xC0, 0x58),
            ab1: rgb(0xD5, 0xEE, 0xE8),
            ab2: rgb(0xD7, 0xE6, 0xF2),
            ab3: rgb(0xDE, 0xF0, 0xD4),
            text: rgb(0x18, 0x2A, 0x27),
            sub: rgb(0x64, 0x78, 0x72),
            border: rgb(0xD3, 0xE2, 0xDE),
            up: rgb(0x45, 0x9F, 0x62),
            down: rgb(0xD7, 0x68, 0x5A),
            dark: false,
        }
    }

    fn void() -> Self {
        Self {
            name: "Void",
            icon: "(⌐■_■)",
            bg: rgb(0x17, 0x1B, 0x2A),
            sidebar: rgb(0x1F, 0x24, 0x36),
            card: rgb(0x26, 0x2C, 0x40),
            card_alt: rgb(0x2F, 0x36, 0x4C),
            a1: rgb(0xE9, 0x87, 0xB4),
            a2: rgb(0xAA, 0x8F, 0xD6),
            a3: rgb(0x6A, 0xC7, 0xA0),
            ab1: rgb(0x45, 0x31, 0x42),
            ab2: rgb(0x3B, 0x33, 0x52),
            ab3: rgb(0x2B, 0x42, 0x3D),
            text: rgb(0xEC, 0xEE, 0xF2),
            sub: rgb(0x8B, 0x93, 0xA8),
            border: rgb(0x39, 0x40, 0x5B),
            up: rgb(0x63, 0xCE, 0x94),
            down: rgb(0xE3, 0x82, 0x72),
            dark: true,
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
}

/// Mix two colors by `t` in [0, 1].
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color::from_rgba(
        a.r * (1.0 - t) + b.r * t,
        a.g * (1.0 - t) + b.g * t,
        a.b * (1.0 - t) + b.b * t,
        a.a * (1.0 - t) + b.a * t,
    )
}

/// Same color with a different alpha channel.
pub fn with_alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}
