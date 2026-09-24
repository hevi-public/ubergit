//! lazygit's default look, mapped onto a dark terminal palette.

use gpui_kit::{Hsla, Pixels, px, rgb};

fn c(hex: u32) -> Hsla {
    Hsla::from(rgb(hex))
}

pub struct Palette;

impl Palette {
    pub fn bg() -> Hsla {
        c(0x16181c)
    }
    pub fn fg() -> Hsla {
        c(0xc9ccd1)
    }
    pub fn dim() -> Hsla {
        c(0x6b717b)
    }
    /// lazygit `activeBorderColor: [green, bold]`
    pub fn active_border() -> Hsla {
        c(0x8cc265)
    }
    /// lazygit `inactiveBorderColor: [default]`
    pub fn inactive_border() -> Hsla {
        c(0x3c414a)
    }
    /// lazygit `selectedLineBgColor: [blue]`
    pub fn selection() -> Hsla {
        c(0x264f78)
    }
    /// lazygit `inactiveViewSelectedLineBgColor: [bold]`, softened with a faint bg.
    pub fn inactive_selection() -> Hsla {
        c(0x24282f)
    }
    pub fn green() -> Hsla {
        c(0x8cc265)
    }
    pub fn red() -> Hsla {
        c(0xe06c75)
    }
    pub fn yellow() -> Hsla {
        c(0xe5c07b)
    }
    pub fn blue() -> Hsla {
        c(0x61afef)
    }
    pub fn cyan() -> Hsla {
        c(0x56b6c2)
    }
    pub fn magenta() -> Hsla {
        c(0xc678dd)
    }
}

pub const FONT: &str = "Menlo";
pub const FONT_SIZE: Pixels = px(13.);
pub const LINE_HEIGHT: Pixels = px(18.);
