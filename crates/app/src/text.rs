//! Single-line styled text built from coloured spans, rendered as one `StyledText`.

use std::ops::Range;

use gpui_kit::{FontWeight, HighlightStyle, Hsla, SharedString, StyledText};

#[derive(Clone, Default)]
pub struct Line {
    text: String,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
}

impl Line {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn plain(text: impl AsRef<str>) -> Self {
        let mut line = Self::new();
        line.push(text);
        line
    }

    pub fn push(&mut self, text: impl AsRef<str>) -> &mut Self {
        self.text.push_str(text.as_ref());
        self
    }

    pub fn color(&mut self, text: impl AsRef<str>, color: Hsla) -> &mut Self {
        self.styled(text, HighlightStyle { color: Some(color), ..Default::default() })
    }

    pub fn bold(&mut self, text: impl AsRef<str>, color: Hsla) -> &mut Self {
        self.styled(
            text,
            HighlightStyle { color: Some(color), font_weight: Some(FontWeight::BOLD), ..Default::default() },
        )
    }

    pub fn styled(&mut self, text: impl AsRef<str>, style: HighlightStyle) -> &mut Self {
        let start = self.text.len();
        self.text.push_str(text.as_ref());
        if self.text.len() > start {
            self.highlights.push((start..self.text.len(), style));
        }
        self
    }

    pub fn append(&mut self, other: Line) -> &mut Self {
        let offset = self.text.len();
        self.text.push_str(&other.text);
        self.highlights.extend(
            other
                .highlights
                .into_iter()
                .map(|(range, style)| (range.start + offset..range.end + offset, style)),
        );
        self
    }

    /// Pads with spaces up to `width` characters.
    pub fn pad_to(&mut self, width: usize) -> &mut Self {
        let len = self.text.chars().count();
        if len < width {
            self.text.extend(std::iter::repeat_n(' ', width - len));
        }
        self
    }

    pub fn build(&self) -> StyledText {
        let text: SharedString = if self.text.is_empty() { " ".into() } else { self.text.clone().into() };
        StyledText::new(text).with_highlights(self.highlights.clone())
    }
}

/// Truncates to `max` characters, marking the cut with `…`.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

/// `3m`, `2h`, `5d`, `1w`, `4M`, `2y` like lazygit's recency column.
pub fn age(time: Option<std::time::SystemTime>) -> String {
    let Some(time) = time else { return String::new() };
    let secs = std::time::SystemTime::now()
        .duration_since(time)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 7 * 86_400 => format!("{}d", s / 86_400),
        s if s < 30 * 86_400 => format!("{}w", s / (7 * 86_400)),
        s if s < 365 * 86_400 => format!("{}M", s / (30 * 86_400)),
        s => format!("{}y", s / (365 * 86_400)),
    }
}
