//! Help-modal rendering: [`help_lines`] and the label machinery
//! ([`KeyLabel`], [`ResolvedLabel`], [`effective_label`]).

use unicode_width::UnicodeWidthStr;

use super::KeybindingResolver;
use super::table::{KEYBINDS, Keybind, all_contexts};

#[derive(Debug, Clone, Copy)]
pub enum KeyLabel {
    Single(&'static str),
    Alt(&'static str, &'static str),
    /// Alt on Mac, Single (first) on other platforms.
    MacAlt(&'static str, &'static str),
    /// Multi on Mac, Multi (first slice) on other platforms.
    MacMulti(&'static [&'static str], &'static [&'static str]),
}

pub const ALT_SEP: &str = " / ";

#[derive(Debug, Clone)]
pub enum ResolvedLabel {
    Single(&'static str),
    Alt(&'static str, &'static str),
    Multi(Box<[&'static str]>),
}

impl ResolvedLabel {
    pub fn display_width(&self) -> usize {
        let sep_w = UnicodeWidthStr::width(ALT_SEP);
        match self {
            Self::Single(s) => UnicodeWidthStr::width(*s),
            Self::Alt(a, b) => UnicodeWidthStr::width(*a) + sep_w + UnicodeWidthStr::width(*b),
            Self::Multi(keys) => {
                keys.iter()
                    .map(|k| UnicodeWidthStr::width(*k))
                    .sum::<usize>()
                    + sep_w * keys.len().saturating_sub(1)
            }
        }
    }
}

impl KeyLabel {
    pub fn resolve(self) -> ResolvedLabel {
        match self {
            Self::Single(s) => ResolvedLabel::Single(s),
            Self::Alt(a, b) => ResolvedLabel::Alt(a, b),
            Self::MacAlt(a, b) => {
                if cfg!(target_os = "macos") {
                    ResolvedLabel::Alt(a, b)
                } else {
                    ResolvedLabel::Single(a)
                }
            }
            Self::MacMulti(normal, mac) => {
                if cfg!(target_os = "macos") {
                    ResolvedLabel::Multi(Box::from(mac))
                } else {
                    ResolvedLabel::Multi(Box::from(normal))
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn flat_str(&self) -> String {
        match self.resolve() {
            ResolvedLabel::Single(s) => s.to_string(),
            ResolvedLabel::Alt(a, b) => format!("{a}/{b}"),
            ResolvedLabel::Multi(keys) => keys.join("/"),
        }
    }
}

/// Effective label for a help-table row, applying user overrides.
/// Returns `None` when the action is disabled (empty overlay).
pub fn effective_label(kb: &Keybind, resolver: &KeybindingResolver) -> Option<ResolvedLabel> {
    match kb.action_id {
        Some(id) => {
            let binds = resolver.binds(id);
            if binds.is_empty() {
                return None;
            }
            if !resolver.is_overridden(id) {
                return Some(kb.label.resolve());
            }
            let labels: Vec<&'static str> = binds.iter().map(|b| b.label).collect();
            match labels.len() {
                1 => Some(ResolvedLabel::Single(labels[0])),
                _ => Some(ResolvedLabel::Multi(labels.into_boxed_slice())),
            }
        }
        None => Some(kb.label.resolve()),
    }
}

/// The help modal's body, grouped by context (children nested under their
/// parent) with the resolver's effective labels. Auto-generated from
/// [`KEYBINDS`]; returns (lines, key-column width) so callers can pad.
pub fn help_lines(resolver: &KeybindingResolver) -> (Vec<ratatui::text::Line<'static>>, usize) {
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};

    let t = crate::tui::ui::theme::current();
    let key_style = Style::default().fg(t.cyan);
    let desc_style = Style::default().fg(t.text_tertiary);
    let section_style = Style::default().fg(t.text_secondary);

    let key_col_width = KEYBINDS
        .iter()
        .filter(|kb| kb.platform.is_visible())
        .filter_map(|kb| effective_label(kb, resolver))
        .map(|label| label.display_width())
        .max()
        .unwrap_or(0)
        + 2;

    let key_spans = |label: ResolvedLabel, pad: usize, prefix: &str| -> Vec<Span<'static>> {
        let keys: Vec<&'static str> = match label {
            ResolvedLabel::Single(s) => vec![s],
            ResolvedLabel::Alt(a, b) => vec![a, b],
            ResolvedLabel::Multi(keys) => keys.into_vec(),
        };
        let sep_w = UnicodeWidthStr::width(ALT_SEP);
        let content_w: usize = keys
            .iter()
            .map(|k| UnicodeWidthStr::width(*k))
            .sum::<usize>()
            + sep_w * keys.len().saturating_sub(1);
        let trailing = pad.saturating_sub(content_w);
        let mut spans = Vec::with_capacity(keys.len() * 2);
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(ALT_SEP, desc_style));
            }
            let text = if i == 0 && i == keys.len() - 1 {
                format!("{prefix}{k}{:trailing$}", "")
            } else if i == 0 {
                format!("{prefix}{k}")
            } else if i == keys.len() - 1 {
                format!("{k}{:trailing$}", "")
            } else {
                (*k).to_string()
            };
            spans.push(Span::styled(text, key_style));
        }
        spans
    };

    let mut lines: Vec<Line> = Vec::new();
    let mut first = true;
    for ctx in all_contexts() {
        if ctx.parent().is_some() {
            continue;
        }
        if !first {
            lines.push(Line::default());
        }
        first = false;

        lines.push(Line::from(Span::styled(
            format!("  {}", ctx.label()),
            section_style,
        )));
        for kb in KEYBINDS
            .iter()
            .filter(|kb| kb.context == ctx && kb.platform.is_visible())
        {
            let Some(label) = effective_label(kb, resolver) else {
                continue;
            };
            let mut spans = key_spans(label, key_col_width, "  ");
            spans.push(Span::styled(kb.description, desc_style));
            lines.push(Line::from(spans));
        }

        for child in all_contexts() {
            if child.parent() != Some(ctx) {
                continue;
            }
            let child_binds: Vec<_> = KEYBINDS
                .iter()
                .filter(|kb| kb.context == child && kb.platform.is_visible())
                .collect();
            if child_binds.is_empty() {
                continue;
            }
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!("    {}", child.label()),
                section_style,
            )));
            for kb in child_binds {
                let Some(label) = effective_label(kb, resolver) else {
                    continue;
                };
                let mut spans = key_spans(label, key_col_width.saturating_sub(2), "    ");
                spans.push(Span::styled(kb.description, desc_style));
                lines.push(Line::from(spans));
            }
        }
    }
    (lines, key_col_width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_lines_render_and_honor_overrides() {
        let (lines, _) = help_lines(&KeybindingResolver::new());
        assert!(lines.len() > KEYBINDS.len(), "section headers add lines");

        let entries = vec![("search".to_string(), vec![])];
        let mut warnings = Vec::new();
        let disabled = KeybindingResolver::from_overlay(&entries, &mut warnings);
        let (disabled_lines, _) = help_lines(&disabled);
        assert!(
            disabled_lines.len() < lines.len(),
            "disabled action drops its row"
        );
    }
}
