#[derive(Debug, Clone, Default)]
pub struct HelpEntry {
    /// Keys that trigger this action, e.g. ["↑", "k"]. Rendered labels,
    /// owned so registry-derived chord labels (`Chord::display`) can be
    /// used alongside static literals.
    pub keys: Vec<String>,
    /// Human-readable description of the action.
    pub action: String,
}

impl HelpEntry {
    pub fn new<K: Into<String>>(
        keys: impl IntoIterator<Item = K>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            keys: keys.into_iter().map(Into::into).collect(),
            action: action.into(),
        }
    }

    /// Convenience: single key.
    pub fn key(key: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            keys: vec![key.into()],
            action: action.into(),
        }
    }

    /// Blank spacer row.
    pub fn spacer() -> Self {
        Self {
            keys: vec![],
            action: String::new(),
        }
    }

    /// Keyless description row (no key column, just text).
    pub fn desc(action: impl Into<String>) -> Self {
        Self {
            keys: vec![],
            action: action.into(),
        }
    }

    /// Format keys as "↑ / k" etc.
    pub fn key_str(&self) -> String {
        let mut labels = Vec::new();
        let mut start = 0;
        while start < self.keys.len() {
            let key = &self.keys[start];
            let mut end = start + 1;
            if let Some(digit) = key.as_bytes().last().copied().filter(u8::is_ascii_digit) {
                let prefix = &key[..key.len() - 1];
                while end < self.keys.len() {
                    let next = digit as usize + end - start;
                    if next > b'9' as usize
                        || self.keys[end] != format!("{prefix}{}", next as u8 as char)
                    {
                        break;
                    }
                    end += 1;
                }
                if end - start >= 3 {
                    labels.push(format!(
                        "{key}-{}",
                        digit as usize + end - start - 1 - b'0' as usize
                    ));
                } else {
                    end = start + 1;
                    labels.push(key.clone());
                }
            } else {
                labels.push(key.clone());
            }
            start = end;
        }
        labels.join(" / ")
    }
}

#[derive(Debug, Clone, Default)]
pub struct HelpNode {
    /// Short label shown as a dim section header (e.g. "Tab", "Widget"). None = no header.
    pub title: Option<String>,
    /// Prose description shown above the keybindings, soft-wrapped to modal width.
    pub description: Option<String>,
    /// Keybinding entries for this level.
    pub entries: Vec<HelpEntry>,
    /// Child nodes (tab-level, widget-level, etc.).
    pub children: Vec<HelpNode>,
}

impl HelpNode {
    /// Leaf node with just keybindings.
    pub fn entries(entries: Vec<HelpEntry>) -> Self {
        Self {
            entries,
            ..Default::default()
        }
    }

    #[cfg(test)]
    pub fn titled(title: impl Into<String>, entries: Vec<HelpEntry>) -> Self {
        Self {
            title: Some(title.into()),
            entries,
            ..Default::default()
        }
    }

    /// Consume self and append a child node, returning the modified node.
    pub fn with_child(mut self, child: HelpNode) -> Self {
        self.children.push(child);
        self
    }
}

/// Implement this on any struct that can contribute to the help modal.
pub trait HelpContext {
    fn help_context(&self) -> HelpNode;
}

// ── CtxBar ────────────────────────────────────────────────────────────────────

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

/// A one-row context-window usage bar.
/// Renders left-aligned into whatever `Rect` you hand it.
/// Returns `None` from `widget()` when there is nothing to show.
pub struct CtxBar {
    pub input_tokens: Option<u64>,
    pub max_tokens: Option<u64>,
}

impl CtxBar {
    pub fn new(input_tokens: Option<u64>, max_tokens: Option<u64>) -> Self {
        Self {
            input_tokens,
            max_tokens,
        }
    }

    fn line(&self) -> Option<Line<'static>> {
        let (text, pct_opt) = match (self.input_tokens, self.max_tokens) {
            (Some(used), Some(max)) if max > 0 => {
                let pct = (used as f64 / max as f64 * 100.0).min(100.0);
                let bar_width: usize = 16;
                let filled = ((pct / 100.0) * bar_width as f64).round() as usize;
                let empty = bar_width.saturating_sub(filled);
                let bar = format!(
                    "[{}{}]",
                    "\u{2588}".repeat(filled),
                    "\u{2591}".repeat(empty)
                );
                let label = format!(
                    " ctx: {:>7} / {:>7}  {}  {:.0}%",
                    fmt_tokens(used),
                    fmt_tokens(max),
                    bar,
                    pct,
                );
                (label, Some(pct))
            }
            (Some(used), None) => {
                let label = format!(" ctx: {} tokens", fmt_tokens(used));
                (label, None)
            }
            _ => return None,
        };

        let color = match pct_opt {
            Some(p) if p >= 90.0 => Color::Red,
            Some(p) if p >= 75.0 => Color::Yellow,
            _ => Color::DarkGray,
        };

        Some(Line::from(Span::styled(text, Style::default().fg(color))))
    }

    /// Build a `Paragraph` widget, or `None` if there is nothing to show.
    pub fn widget(&self) -> Option<Paragraph<'static>> {
        let line = self.line()?;
        Some(Paragraph::new(line))
    }
}

fn fmt_tokens(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

// ── InfoBar ─────────────────────────────────────────────────────────────────

use std::time::{Duration, Instant};

/// How long an info message stays on the bar before it auto-clears. Named so
/// the timeout is not a bare literal at the clear site.
pub const INFO_BAR_TTL: Duration = Duration::from_secs(10);

/// Severity of an info-bar message. Drives the colour; never matched on as a
/// string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoKind {
    /// Neutral operational note (e.g. "Fetching models for anthropic…").
    Info,
    /// A completed action worth confirming (e.g. "Model switched to …").
    Note,
    /// A failure the user should see (e.g. an RPC error).
    Error,
}

/// A single user-facing message shown in the conversation metadata row. Owned
/// by the pane as `Option<InfoMessage>`; `None` means no feedback is shown.
/// `set_at` drives the [`INFO_BAR_TTL`] auto-clear during pane rendering.
#[derive(Debug, Clone)]
pub struct InfoMessage {
    pub kind: InfoKind,
    pub text: String,
    pub set_at: Instant,
}

impl InfoMessage {
    pub fn new(kind: InfoKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            set_at: Instant::now(),
        }
    }

    pub fn info(text: impl Into<String>) -> Self {
        Self::new(InfoKind::Info, text)
    }

    pub fn note(text: impl Into<String>) -> Self {
        Self::new(InfoKind::Note, text)
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self::new(InfoKind::Error, text)
    }

    /// `true` once the message has been visible for at least [`INFO_BAR_TTL`].
    pub fn is_expired(&self) -> bool {
        self.set_at.elapsed() >= INFO_BAR_TTL
    }
}

/// A one-row, single-line info bar. Renders the current message truncated to the
/// available width; stores the full text untruncated so a wider window shows
/// more without any state change.
pub struct InfoBar<'a> {
    message: Option<&'a InfoMessage>,
}

impl<'a> InfoBar<'a> {
    pub fn new(message: Option<&'a InfoMessage>) -> Self {
        Self { message }
    }

    #[cfg(test)]
    pub fn has_content(&self) -> bool {
        self.message.is_some()
    }

    /// Build the `Paragraph`, or `None` when there is no message. `width` is the
    /// available column count; the text is truncated (with an ellipsis) to fit.
    pub fn widget(&self, width: usize) -> Option<Paragraph<'static>> {
        let msg = self.message?;
        let palette = crate::theme::active();
        let color = match msg.kind {
            InfoKind::Info => palette.dim,
            InfoKind::Note => palette.accent,
            InfoKind::Error => palette.warn,
        };
        let text = truncate_to_width(&msg.text, width);
        Some(Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(color),
        ))))
    }
}

/// Truncate `s` to at most `width` display columns, appending an ellipsis when
/// it overflows without splitting a grapheme cluster.
pub(crate) fn truncate_to_width(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if crate::display_width::display_width(s) <= width {
        return s.to_string();
    }
    if width == 1 {
        return "\u{2026}".to_string();
    }
    let keep = width - 1;
    let mut used: usize = 0;
    let mut out = String::new();
    for (_, grapheme, grapheme_width) in crate::display_width::grapheme_widths(s) {
        if used.saturating_add(grapheme_width) > keep {
            break;
        }
        out.push_str(grapheme);
        used += grapheme_width;
    }
    out.push('\u{2026}');
    out
}

// ── PickerModal ─────────────────────────────────────────────────────────────

use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
};
use unicode_width::UnicodeWidthStr;

/// Cells between a row label and its `current` suffix.
const CURRENT_SUFFIX_GAP: usize = 2;

pub struct PickerModal<'a> {
    title: &'a str,
    items: &'a [String],
    cursor: usize,
    /// Row whose value is currently in force, with the localized suffix it
    /// is drawn with.
    current: Option<(usize, &'a str)>,
}

impl<'a> PickerModal<'a> {
    pub fn new(title: &'a str, items: &'a [String], cursor: usize) -> Self {
        Self {
            title,
            items,
            cursor,
            current: None,
        }
    }

    /// Tag `row` as the value currently in force by appending a dim `label`
    /// suffix. A word rather than a glyph: East Asian labels already mix
    /// single- and double-width cells, and a marker glyph would misalign the
    /// column. `None` draws every row plain.
    pub fn with_current(mut self, row: Option<usize>, label: &'a str) -> Self {
        self.current = row.map(|row| (row, label));
        self
    }

    /// Modal rect for a picker without a current-row suffix.
    pub fn area_for(title: &str, items: &[String], area: Rect) -> Option<Rect> {
        PickerModal::new(title, items, 0).area(area)
    }

    /// The rect this modal renders into, centered within `area`; `None` when
    /// there are no rows. Keep this geometry in sync with `render` so mouse
    /// hit-testing lands on the same rows the user sees.
    pub fn area(&self, area: Rect) -> Option<Rect> {
        if self.items.is_empty() {
            return None;
        }

        let longest = self
            .items
            .iter()
            .enumerate()
            .map(|(row, label)| self.row_width(row, label))
            .max()
            .unwrap_or(0);
        picker_rect(self.title, longest, self.items.len(), area)
    }

    fn current_suffix(&self, row: usize) -> Option<&'a str> {
        self.current
            .and_then(|(current, label)| (current == row).then_some(label))
    }

    fn row_width(&self, row: usize, label: &str) -> usize {
        let base = UnicodeWidthStr::width(label);
        match self.current_suffix(row) {
            Some(suffix) => base + CURRENT_SUFFIX_GAP + UnicodeWidthStr::width(suffix),
            None => base,
        }
    }

    fn row_line(&self, row: usize, label: &str, style: ratatui::style::Style) -> Line<'static> {
        let mut spans = vec![Span::styled(label.to_string(), style)];
        if let Some(suffix) = self.current_suffix(row) {
            spans.push(Span::styled(
                format!("{}{suffix}", " ".repeat(CURRENT_SUFFIX_GAP)),
                crate::theme::dim_style(),
            ));
        }
        Line::from(spans)
    }

    /// Render the modal centered within `area`. No-op when there are no items.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let Some(modal_rect) = self.area(area) else {
            return;
        };

        frame.render_widget(Clear, modal_rect);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(crate::theme::overlay_border_style())
            .style(crate::theme::fill_style())
            .title(Span::styled(
                format!(" {} ", self.title),
                crate::theme::heading_style(),
            ));

        let items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let style = if i == self.cursor {
                    crate::theme::selected_style()
                } else {
                    crate::theme::body_style()
                };
                ListItem::new(self.row_line(i, label, style))
            })
            .collect();

        let mut list_state = ListState::default();
        list_state.select(Some(self.cursor.min(self.items.len().saturating_sub(1))));

        let list = List::new(items)
            .block(block)
            .highlight_style(crate::theme::selected_style());

        frame.render_stateful_widget(list, modal_rect, &mut list_state);
    }
}

fn picker_rect(title: &str, label_width: usize, rows: usize, area: Rect) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let width = label_width
        .max(UnicodeWidthStr::width(title))
        .saturating_add(4)
        .max(12)
        .min(usize::from(area.width)) as u16;
    let height = rows.saturating_add(2).max(3).min(usize::from(area.height)) as u16;
    Some(Rect::new(
        area.x.saturating_add((area.width - width) / 2),
        area.y.saturating_add((area.height - height) / 2),
        width,
        height,
    ))
}

/// Read-only legacy field access through [`PickerState`]'s `Deref`.
/// The state deliberately does not implement `DerefMut`: indices and cached
/// widths are derived from this one immutable catalog.
#[derive(Debug, Clone, Default)]
pub struct PickerCatalog {
    pub items: Vec<String>,
}

/// Owned picker state. Search and current-value markers are opt-in; plain
/// pickers retain their compact presentation and legacy item access.
#[derive(Debug, Clone, Default)]
pub struct PickerState {
    catalog: PickerCatalog,
    filtered: Vec<usize>,
    max_label_width: usize,
    searchable: bool,
    query: String,
    scroll_offset: usize,
    last_list_height: usize,
    /// Position within the filtered results, not the catalog.
    pub cursor: usize,
    /// Row holding the value currently in force, drawn with the `current`
    /// suffix. `None` when that value is not among the rows.
    pub current: Option<usize>,
}

impl std::ops::Deref for PickerState {
    type Target = PickerCatalog;

    fn deref(&self) -> &Self::Target {
        &self.catalog
    }
}

struct PickerGeometry {
    modal: Rect,
    search: Option<Rect>,
    list: Rect,
    rows: std::ops::Range<usize>,
}

impl PickerState {
    /// Build a picker over `items`. `default` is the value currently in
    /// force: when present it is pre-selected and marked as the current row,
    /// else the cursor starts on the first row and no row is marked.
    pub fn new(items: Vec<String>, default: Option<&str>) -> Self {
        let current = default.and_then(|d| items.iter().position(|i| i == d));
        let max_label_width = items
            .iter()
            .map(|label| UnicodeWidthStr::width(label.as_str()))
            .max()
            .unwrap_or(0);
        Self {
            filtered: (0..items.len()).collect(),
            catalog: PickerCatalog { items },
            max_label_width,
            current,
            cursor: current.unwrap_or(0),
            ..Self::default()
        }
    }

    pub fn new_searchable(items: Vec<String>, default: Option<&str>) -> Self {
        Self {
            searchable: true,
            ..Self::new(items, default)
        }
    }

    pub fn push_query(&mut self, ch: char) {
        if self.searchable && !ch.is_control() {
            self.query.push(ch);
            self.filter();
        }
    }

    pub fn pop_query(&mut self) {
        if self.searchable && self.query.pop().is_some() {
            self.filter();
        }
    }

    pub fn paste_query(&mut self, text: &str) {
        if self.searchable {
            self.query
                .extend(text.chars().filter(|ch| !ch.is_control()));
            self.filter();
        }
    }

    fn filter(&mut self) {
        let selected = self.filtered.get(self.cursor).copied();
        let query = self.query.to_lowercase();
        self.filtered.clear();
        self.filtered.extend(
            self.catalog
                .items
                .iter()
                .enumerate()
                .filter(|(_, label)| query.is_empty() || label.to_lowercase().contains(&query))
                .map(|(index, _)| index),
        );
        self.cursor = selected
            .and_then(|index| self.filtered.iter().position(|&i| i == index))
            .unwrap_or(0);
        self.scroll_offset = 0;
        self.scroll_offset = self.offset_for(self.last_list_height);
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.visible_count() == 0
    }

    pub fn move_up(&mut self) {
        self.move_by(-1);
    }

    pub fn move_down(&mut self) {
        self.move_by(1);
    }

    pub fn move_by(&mut self, delta: isize) {
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.visible_count().saturating_sub(1));
    }

    pub fn first(&mut self) {
        self.cursor = 0;
    }

    pub fn last(&mut self) {
        self.cursor = self.visible_count().saturating_sub(1);
    }

    pub fn page_up(&mut self) {
        self.move_by(-(self.last_list_height.max(1) as isize));
    }

    pub fn page_down(&mut self) {
        self.move_by(self.last_list_height.max(1) as isize);
    }

    /// The currently highlighted value, if any.
    pub fn selected(&self) -> Option<&str> {
        self.filtered
            .get(self.cursor)
            .map(|&index| self.catalog.items[index].as_str())
    }

    pub fn visible_count(&self) -> usize {
        self.filtered.len()
    }

    fn offset_for(&self, height: usize) -> usize {
        if height == 0 || self.filtered.is_empty() {
            return 0;
        }
        let cursor = self.cursor.min(self.filtered.len() - 1);
        self.scroll_offset
            .min(self.filtered.len().saturating_sub(height))
            .min(cursor)
            .max(cursor.saturating_sub(height - 1))
    }

    fn geometry(&self, title: &str, area: Rect) -> Option<PickerGeometry> {
        if !self.searchable && self.catalog.items.is_empty() {
            return None;
        }
        let mut width = self.max_label_width;
        if self.searchable {
            if self.current.is_some() {
                width = width.saturating_add(
                    UnicodeWidthStr::width(crate::i18n::t("zc-picker-current").as_str()) + 3,
                );
            }
            width = width
                .max(UnicodeWidthStr::width(crate::i18n::t("zc-picker-search").as_str()) + 2)
                .max(UnicodeWidthStr::width(
                    crate::i18n::t("zc-picker-no-results").as_str(),
                ));
        }
        // Catalog dimensions, not result/query dimensions, keep filtering stable.
        let rows = self.catalog.items.len().max(1) + usize::from(self.searchable);
        let modal = picker_rect(title, width, rows, area)?;
        let inner = Block::default().borders(Borders::ALL).inner(modal);
        let search_height = u16::from(self.searchable && inner.height > 0);
        let search =
            (search_height > 0).then(|| Rect::new(inner.x, inner.y, inner.width, search_height));
        let list = Rect::new(
            inner.x,
            inner.y.saturating_add(search_height),
            inner.width,
            inner.height.saturating_sub(search_height),
        );
        let height = if list.width == 0 {
            0
        } else {
            usize::from(list.height)
        };
        let offset = self.offset_for(height);
        Some(PickerGeometry {
            modal,
            search,
            list,
            rows: offset..offset.saturating_add(height).min(self.filtered.len()),
        })
    }

    pub fn modal_area(&self, title: &str, area: Rect) -> Option<Rect> {
        self.geometry(title, area).map(|geometry| geometry.modal)
    }

    /// Hit-test precisely the same clipped rows and retained offset as rendering.
    pub fn select_at(&mut self, column: u16, row: u16, title: &str, area: Rect) -> bool {
        let Some(geometry) = self.geometry(title, area) else {
            return false;
        };
        if column < geometry.list.x
            || column >= geometry.list.right()
            || row < geometry.list.y
            || row >= geometry.list.bottom()
        {
            return false;
        }
        let position = geometry.rows.start + usize::from(row - geometry.list.y);
        if !geometry.rows.contains(&position) {
            return false;
        }
        self.scroll_offset = geometry.rows.start;
        self.cursor = position;
        true
    }

    /// Draw only the visible catalog slice; filtering and catalog width scans
    /// happen at query changes and construction, never on the frame path.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, title: &str) {
        let Some(geometry) = self.geometry(title, area) else {
            self.last_list_height = 0;
            return;
        };
        self.last_list_height = if geometry.list.width == 0 {
            0
        } else {
            usize::from(geometry.list.height)
        };
        self.scroll_offset = geometry.rows.start;
        frame.render_widget(Clear, geometry.modal);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_style(crate::theme::overlay_border_style())
                .style(crate::theme::fill_style())
                .title(Span::styled(
                    format!(" {title} "),
                    crate::theme::heading_style(),
                )),
            geometry.modal,
        );
        if let Some(search) = geometry.search {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        format!("{}: ", crate::i18n::t("zc-picker-search")),
                        crate::theme::dim_style(),
                    ),
                    Span::styled(self.query.as_str(), crate::theme::body_style()),
                ])),
                search,
            );
        }
        if self.filtered.is_empty() {
            frame.render_widget(
                Paragraph::new(crate::i18n::t("zc-picker-no-results"))
                    .style(crate::theme::dim_style()),
                geometry.list,
            );
            return;
        }
        let marker = if self.searchable && self.current.is_some() {
            format!(" ({})", crate::i18n::t("zc-picker-current"))
        } else {
            String::new()
        };
        for (row, position) in geometry.rows.enumerate() {
            let index = self.filtered[position];
            let style = if position == self.cursor {
                crate::theme::selected_style()
            } else {
                crate::theme::body_style()
            };
            let mut line = Line::from(self.catalog.items[index].as_str());
            if self.searchable && self.current == Some(index) {
                line.push_span(marker.as_str());
            }
            frame.render_widget(
                Paragraph::new(line).style(style),
                Rect::new(
                    geometry.list.x,
                    geometry.list.y.saturating_add(row as u16),
                    geometry.list.width,
                    1,
                ),
            );
        }
    }
}

#[cfg(test)]
mod info_bar_tests {
    use super::*;

    #[test]
    fn truncate_shorter_than_width_is_unchanged() {
        assert_eq!(truncate_to_width("model", 10), "model");
    }

    #[test]
    fn truncate_exact_width_is_unchanged() {
        assert_eq!(truncate_to_width("model", 5), "model");
    }

    #[test]
    fn truncate_overflow_appends_ellipsis() {
        assert_eq!(truncate_to_width("anthropic", 5), "anth\u{2026}");
    }

    #[test]
    fn truncate_zero_width_is_empty() {
        assert_eq!(truncate_to_width("anything", 0), "");
    }

    #[test]
    fn truncate_width_one_is_ellipsis() {
        assert_eq!(truncate_to_width("anything", 1), "\u{2026}");
    }

    #[test]
    fn truncate_respects_wide_cells() {
        let truncated = truncate_to_width("界界界", 5);
        assert_eq!(truncated, "界界\u{2026}");
        assert_eq!(crate::display_width::display_width(&truncated), 5);
    }

    #[test]
    fn truncate_keeps_grapheme_clusters_intact() {
        let family = "👨‍👩‍👧‍👦";
        let truncated = truncate_to_width(&format!("{family}abc"), 4);
        assert_eq!(truncated, format!("{family}a\u{2026}"));
        assert_eq!(crate::display_width::display_width(&truncated), 4);
    }

    #[test]
    fn fresh_message_is_not_expired() {
        let m = InfoMessage::info("hi");
        assert!(!m.is_expired());
    }

    #[test]
    fn ttl_aged_message_is_expired() {
        let mut m = InfoMessage::error("boom");
        m.set_at = Instant::now() - INFO_BAR_TTL - Duration::from_secs(1);
        assert!(m.is_expired());
    }

    #[test]
    fn no_message_renders_nothing() {
        let bar = InfoBar::new(None);
        assert!(!bar.has_content());
        assert!(bar.widget(80).is_none());
    }

    #[test]
    fn message_renders_widget() {
        let m = InfoMessage::note("switched");
        let bar = InfoBar::new(Some(&m));
        assert!(bar.has_content());
        assert!(bar.widget(80).is_some());
    }
}

#[cfg(test)]
mod picker_tests {
    use super::*;

    fn draw_picker(picker: &mut PickerState, area: Rect) -> ratatui::buffer::Buffer {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
            area.right(),
            area.bottom(),
        ))
        .unwrap();
        terminal
            .draw(|frame| picker.render(frame, area, "Pick"))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn row_text(buffer: &ratatui::buffer::Buffer, area: Rect, row: u16) -> String {
        (area.x..area.right())
            .map(|x| buffer[(x, row)].symbol())
            .collect()
    }

    fn catalog(count: usize) -> Vec<String> {
        (0..count).map(|i| format!("model-{i:04}")).collect()
    }

    #[test]
    fn searchable_matching_is_case_insensitive_and_accepts_y_and_n() {
        let mut p =
            PickerState::new_searchable(vec!["Yarn".into(), "ONYX".into(), "other".into()], None);
        p.push_query('y');
        assert_eq!(p.visible_count(), 2);
        p.push_query('A');
        assert_eq!(p.selected(), Some("Yarn"));
        assert_eq!(p.visible_count(), 1);
        p.pop_query();
        p.pop_query();
        p.push_query('n');
        assert_eq!(p.visible_count(), 2);
        p.push_query('Y');
        assert_eq!(p.selected(), Some("ONYX"));
        p.push_query('\n');
        assert_eq!(p.query, "nY");
    }

    #[test]
    fn plain_picker_ignores_search_and_preserves_legacy_rendering() {
        let area = Rect::new(0, 0, 80, 24);
        let mut p = PickerState::new(vec!["one".into(), "two".into()], Some("two"));
        p.push_query('x');
        p.pop_query();
        assert!(p.query.is_empty());
        assert_eq!(p.visible_count(), 2);
        // The sidebar's existing field reads still work, without mutable access.
        let items: Vec<String> = p.items.clone();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| PickerModal::new("Pick", &items, p.cursor).render(frame, area))
            .unwrap();
        assert_eq!(draw_picker(&mut p, area), *terminal.backend().buffer());
    }

    #[test]
    fn filtering_preserves_catalog_identity_and_current_marker() {
        let area = Rect::new(0, 0, 80, 24);
        let mut p = PickerState::new_searchable(
            vec!["alpha".into(), "beta".into(), "beta".into()],
            Some("beta"),
        );
        p.last();
        p.push_query('b');
        assert_eq!(p.cursor, 1);
        assert_eq!(p.filtered[p.cursor], 2);
        assert_eq!(p.current, Some(1));
        let buffer = draw_picker(&mut p, area);
        let geometry = p.geometry("Pick", area).unwrap();
        let marker = crate::i18n::t("zc-picker-current");
        assert!(row_text(&buffer, geometry.list, geometry.list.y).contains(&marker));
        assert!(!row_text(&buffer, geometry.list, geometry.list.y + 1).contains(&marker));
        p.pop_query();
        assert_eq!(p.cursor, 2);
        p.push_query('l');
        assert_eq!(p.selected(), Some("alpha"));
        assert_eq!(p.cursor, 0);
        assert_eq!(p.current, Some(1));
    }

    #[test]
    fn no_results_and_blank_rows_cannot_be_selected() {
        let area = Rect::new(0, 0, 80, 24);
        let mut p = PickerState::new_searchable(catalog(10), None);
        let original = p.modal_area("Pick", area);
        p.push_query('x');
        assert_eq!(p.visible_count(), 0);
        assert_eq!(p.selected(), None);
        p.move_up();
        p.move_down();
        p.page_up();
        p.page_down();
        p.first();
        p.last();
        assert_eq!(p.selected(), None);
        let buffer = draw_picker(&mut p, area);
        assert_eq!(p.modal_area("Pick", area), original);
        let geometry = p.geometry("Pick", area).unwrap();
        assert!(
            row_text(&buffer, geometry.list, geometry.list.y)
                .contains(&crate::i18n::t("zc-picker-no-results"))
        );
        for row in geometry.modal.y..geometry.modal.bottom() {
            assert!(!p.select_at(geometry.list.x, row, "Pick", area));
        }
        p.pop_query();
        p.push_query('9');
        let geometry = p.geometry("Pick", area).unwrap();
        assert!(!p.select_at(geometry.list.x, geometry.list.y + 1, "Pick", area));
        assert_eq!(p.selected(), Some("model-0009"));
    }

    #[test]
    fn clicks_follow_rendered_scroll_filter_and_resize() {
        let mut p = PickerState::new_searchable(catalog(100), None);
        let area = Rect::new(3, 2, 60, 10);
        p.last();
        draw_picker(&mut p, area);
        let old_offset = p.scroll_offset;
        p.move_up();
        draw_picker(&mut p, area);
        assert_eq!(p.scroll_offset, old_offset);
        let geometry = p.geometry("Pick", area).unwrap();
        assert!(!p.select_at(geometry.modal.x, geometry.list.y, "Pick", area));
        assert!(!p.select_at(geometry.modal.right() - 1, geometry.list.y, "Pick", area));
        assert!(!p.select_at(geometry.list.x, geometry.search.unwrap().y, "Pick", area));
        let expected = p.catalog.items[p.filtered[geometry.rows.start]].clone();
        assert!(p.select_at(geometry.list.x, geometry.list.y, "Pick", area));
        assert_eq!(p.selected(), Some(expected.as_str()));

        p.push_query('9');
        for area in [area, Rect::new(1, 1, 45, 6), Rect::new(0, 0, 90, 18)] {
            let buffer = draw_picker(&mut p, area);
            let geometry = p.geometry("Pick", area).unwrap();
            let row = geometry.rows.len() - 1;
            let expected = p.catalog.items[p.filtered[geometry.rows.start + row]].clone();
            let y = geometry.list.y + row as u16;
            assert!(row_text(&buffer, geometry.list, y).starts_with(&expected));
            assert!(p.select_at(geometry.list.x, y, "Pick", area));
            assert_eq!(p.selected(), Some(expected.as_str()));
        }
    }

    #[test]
    fn paging_uses_last_rendered_height_and_movement_saturates() {
        let mut p = PickerState::new_searchable(catalog(100), None);
        draw_picker(&mut p, Rect::new(0, 0, 80, 10));
        p.page_down();
        assert_eq!(p.cursor, 7);
        draw_picker(&mut p, Rect::new(0, 0, 80, 6));
        p.page_down();
        assert_eq!(p.cursor, 10);
        p.page_up();
        assert_eq!(p.cursor, 7);
        p.move_by(isize::MIN);
        assert_eq!(p.cursor, 0);
        p.move_by(isize::MAX);
        assert_eq!(p.cursor, 99);
    }

    #[test]
    fn tiny_areas_are_safe_and_only_rendered_rows_are_clickable() {
        for width in 0..=14 {
            for height in 0..=6 {
                let area = Rect::new(0, 0, width, height);
                for searchable in [false, true] {
                    let mut p = if searchable {
                        PickerState::new_searchable(catalog(3), None)
                    } else {
                        PickerState::new(catalog(3), None)
                    };
                    draw_picker(&mut p, area);
                    let geometry = p.geometry("Pick", area);
                    assert_eq!(geometry.is_some(), width > 0 && height > 0);
                    for y in 0..height {
                        for x in 0..width {
                            let expected = geometry.as_ref().is_some_and(|g| {
                                x >= g.list.x
                                    && x < g.list.right()
                                    && y >= g.list.y
                                    && usize::from(y - g.list.y) < g.rows.len()
                            });
                            assert_eq!(p.select_at(x, y, "Pick", area), expected);
                        }
                    }
                    // The legacy API must also be safe before a terminal is sized.
                    let mut terminal =
                        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                            .unwrap();
                    terminal
                        .draw(|frame| PickerModal::new("Pick", &p.items, 0).render(frame, area))
                        .unwrap();
                }
            }
        }
    }

    #[test]
    fn large_catalog_draw_range_is_bounded_by_viewport() {
        let mut p = PickerState::new_searchable(catalog(10_000), None);
        let area = Rect::new(0, 0, 80, 12);
        p.last();
        let buffer = draw_picker(&mut p, area);
        let geometry = p.geometry("Pick", area).unwrap();
        assert_eq!(geometry.rows.len(), 9);
        assert_eq!(geometry.rows.end, 10_000);
        assert_eq!(p.last_list_height, 9);
        for (row, position) in geometry.rows.enumerate() {
            assert!(
                row_text(&buffer, geometry.list, geometry.list.y + row as u16)
                    .starts_with(&p.items[p.filtered[position]])
            );
        }
    }

    #[test]
    fn area_for_uses_display_width_for_wide_items() {
        let items = vec!["界界界界界界".to_string()];

        let area = PickerModal::area_for("Pick", &items, Rect::new(0, 0, 80, 24)).unwrap();

        assert_eq!(area.width, 16);
    }

    #[test]
    fn area_for_uses_display_width_for_wide_title() {
        let items = vec!["one".to_string()];

        let area =
            PickerModal::area_for("界界界界界界界", &items, Rect::new(0, 0, 80, 24)).unwrap();

        assert_eq!(area.width, 18);
    }

    #[test]
    fn new_defaults_to_first_when_no_default() {
        let p = PickerState::new(vec!["a".into(), "b".into()], None);
        assert_eq!(p.cursor, 0);
        assert_eq!(p.selected(), Some("a"));
    }

    #[test]
    fn new_preselects_default_when_present() {
        let p = PickerState::new(vec!["a".into(), "b".into(), "c".into()], Some("b"));
        assert_eq!(p.cursor, 1);
        assert_eq!(p.selected(), Some("b"));
    }

    #[test]
    fn new_default_absent_falls_back_to_first() {
        let p = PickerState::new(vec!["a".into(), "b".into()], Some("zzz"));
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn movement_clamps_at_bounds() {
        let mut p = PickerState::new(vec!["a".into(), "b".into()], None);
        p.move_up(); // already at top
        assert_eq!(p.cursor, 0);
        p.move_down();
        assert_eq!(p.cursor, 1);
        p.move_down(); // already at bottom
        assert_eq!(p.cursor, 1);
    }

    #[test]
    fn empty_picker_has_no_selection() {
        let p = PickerState::default();
        assert!(p.is_empty());
        assert_eq!(p.selected(), None);
        let area = Rect::new(0, 0, 80, 24);
        assert!(p.modal_area("Pick", area).is_none());

        let mut searchable = PickerState::new_searchable(Vec::new(), Some("absent"));
        draw_picker(&mut searchable, area);
        let geometry = searchable.geometry("Pick", area).unwrap();
        assert!(geometry.search.is_some());
        assert_eq!(searchable.current, None);
        assert_eq!(searchable.selected(), None);
        assert!(!searchable.select_at(geometry.list.x, geometry.list.y, "Pick", area));
    }

    #[test]
    fn new_marks_the_default_as_the_current_row() {
        let p = PickerState::new(vec!["a".into(), "b".into()], Some("b"));
        assert_eq!(p.current, Some(1));

        let p = PickerState::new(vec!["a".into(), "b".into()], None);
        assert_eq!(p.current, None);

        let p = PickerState::new(vec!["a".into(), "b".into()], Some("zzz"));
        assert_eq!(p.current, None);
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn area_widens_for_the_current_row_suffix() {
        let items = vec!["low".to_string(), "high".to_string()];
        let area = Rect::new(0, 0, 80, 24);

        let plain = PickerModal::area_for("Pick", &items, area).unwrap();
        assert_eq!(plain.width, 12, "short rows sit at the minimum width");

        let marked = PickerModal::new("Pick", &items, 0)
            .with_current(Some(1), "current")
            .area(area)
            .unwrap();
        // "high" + two cells + "current" is the longest row (13), plus one
        // cell of padding and one border on each side.
        assert_eq!(marked.width, 17);
        assert_eq!(marked.height, plain.height);

        let unmarked = PickerModal::new("Pick", &items, 0)
            .with_current(None, "current")
            .area(area)
            .unwrap();
        assert_eq!(unmarked, plain, "no current row leaves the geometry alone");
    }

    #[test]
    fn current_suffix_geometry_uses_display_width() {
        let items = vec!["界界界界界界".to_string()];

        let marked = PickerModal::new("P", &items, 0)
            .with_current(Some(0), "現在")
            .area(Rect::new(0, 0, 80, 24))
            .unwrap();

        // 12 cells of label + 2 gap + 4 cells of suffix, then padding and border.
        assert_eq!(marked.width, 22);
    }

    #[test]
    fn current_row_renders_the_suffix_only_on_that_row() {
        use ratatui::{Terminal, backend::TestBackend};

        let items = vec!["low".to_string(), "high".to_string()];
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                PickerModal::new("Pick", &items, 0)
                    .with_current(Some(1), "current")
                    .render(frame, frame.area());
            })
            .expect("draw picker");

        let buffer = terminal.backend().buffer();
        let width = usize::from(buffer.area.width);
        let rows: Vec<String> = buffer
            .content()
            .chunks(width)
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect())
            .collect();
        assert!(
            rows.iter().any(|row| row.contains("high  current")),
            "the current row carries the suffix: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("low  current")),
            "other rows stay plain: {rows:?}"
        );
    }
}
