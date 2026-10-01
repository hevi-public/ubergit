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
        let len = self.width();
        if len < width {
            self.text.extend(std::iter::repeat_n(' ', width - len));
        }
        self
    }

    /// Length in characters. Laying lines out by it takes each character as one column of
    /// the monospace font, which a wide one, like CJK or most emoji, isn't.
    pub fn width(&self) -> usize {
        self.text.chars().count()
    }

    /// The text in runs, each with its colour (`None` unstyled), for tests to check.
    #[cfg(test)]
    pub fn spans(&self) -> Vec<(&str, Option<Hsla>)> {
        let mut spans = Vec::new();
        let mut at = 0;
        for (range, style) in &self.highlights {
            if range.start > at {
                spans.push((&self.text[at..range.start], None));
            }
            spans.push((&self.text[range.clone()], style.color));
            at = range.end;
        }
        if at < self.text.len() {
            spans.push((&self.text[at..], None));
        }
        spans
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

/// Like [`truncate`], but cutting the middle out: `phonosemantic-ne…-35c46e`. For names
/// that are told apart by their end — agent worktrees share a long prefix and differ only
/// in a trailing hash — where cutting the tail would render two of them identically.
pub fn truncate_middle(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max <= 1 {
        return truncate(s, max);
    }
    // The tail is what distinguishes them, so give it the odd character.
    let head = (max - 1) / 2;
    let tail = max - 1 - head;
    let mut out: String = s.chars().take(head).collect();
    out.push('…');
    out.extend(s.chars().skip(count - tail));
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_middle_keeps_the_end_that_tells_names_apart() {
        // Agent worktrees share a long prefix; only the trailing hash differs.
        let a = "phonosemantic-network-analysis-35c46e";
        let b = "phonosemantic-network-analysis-26735e";
        assert_ne!(truncate_middle(a, 24), truncate_middle(b, 24));
        // Cutting the tail instead would lose exactly that.
        assert_eq!(truncate(a, 24), truncate(b, 24));

        assert_eq!(truncate_middle(a, 24).chars().count(), 24);
        assert_eq!(truncate_middle("short", 24), "short");
        assert_eq!(truncate_middle("exactly-ten", 11), "exactly-ten");
        assert_eq!(truncate_middle("abcdefghij", 5), "ab…ij");
        // Odd room goes to the tail.
        assert_eq!(truncate_middle("abcdefghij", 6), "ab…hij");
        assert_eq!(truncate_middle("abcdefghij", 1), "…");
        assert_eq!(truncate_middle("abcdefghij", 0), "");
        // Multi-byte characters are counted, not bytes.
        assert_eq!(truncate_middle("ααααββββ", 5).chars().count(), 5);
    }
}
