//! Sessions section renderer for the shell-owned conversation dock.
//!
//! The app owns dock visibility, side, width, and the Sessions/Plan split.
//! This module owns only Sessions widget state, row hit targets, scrolling, and
//! the agent picker. Session rows are derived per frame from the active pane.

use std::collections::HashSet;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use tokio::sync::mpsc;

use crate::chat::{PaneKind, SidebarSessionSummary, SidebarStatus};
use crate::client::RpcClient;
use crate::i18n::{t, t_args};
use crate::keymap::ModalAction;
use crate::{mouse, theme, widgets};

/// Minimum columns the main content keeps; below this the sidebar auto-skips
/// for the frame instead of squeezing the pane.
pub(crate) const CONTENT_MIN_COLS: u16 = 40;

fn truncate_to_cells(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if crate::display_width::display_width(text) <= width {
        return text.to_string();
    }

    let budget = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for (_, grapheme, grapheme_width) in crate::display_width::grapheme_widths(text) {
        if used + grapheme_width > budget {
            break;
        }
        out.push_str(grapheme);
        used += grapheme_width;
    }
    out.push('\u{2026}');
    out
}

/// Per-frame shell context the sidebar renders against.
pub(crate) struct SidebarCtx {
    /// The chat-like pane the current mode maps to, if any.
    pub active_pane: Option<PaneKind>,
    pub connected: bool,
}

/// A user action the shell must route (mode switch + pane call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SidebarEvent {
    ToggleVisibility,
    FocusSession { pane: PaneKind, session_id: String },
    CloseSession { pane: PaneKind, session_id: String },
    OpenPicker,
    PickAgent { pane: PaneKind, alias: String },
}

/// The `+` agent picker modal.
struct SidebarPicker {
    /// Pane a pick adds to, captured when the picker opened.
    target: PaneKind,
    /// Display labels (alias + open-marker suffix), parallel to `aliases`.
    state: widgets::PickerState,
    aliases: Vec<String>,
    /// Informational only: selecting an open alias still creates a new session.
    open_aliases: HashSet<String>,
    loading: bool,
    opened_at: Instant,
    error: Option<String>,
    rx: mpsc::UnboundedReceiver<Result<Vec<String>, String>>,
    double_click: mouse::DoubleClickTracker,
    /// Recorded at draw for click routing.
    modal_rect: Rect,
}

impl SidebarPicker {
    fn selectable(&self) -> bool {
        !self.loading && self.error.is_none() && !self.aliases.is_empty()
    }

    /// The rows the modal shows: real aliases, or a single status row.
    fn display_items(&self) -> Vec<String> {
        let mut items = if self.loading && !self.aliases.is_empty() {
            self.state.items.clone()
        } else if self.loading {
            vec![if self.opened_at.elapsed() >= Duration::from_millis(150) {
                t("zc-sidebar-picker-loading")
            } else {
                String::new()
            }]
        } else if let Some(ref e) = self.error {
            vec![t_args("zc-sidebar-picker-error", &[("error", e)])]
        } else if self.aliases.is_empty() {
            vec![t("zc-sidebar-picker-empty")]
        } else {
            self.state.items.clone()
        };
        // Reserve the same modest footprint for loading, empty and populated lists.
        let width = self
            .state
            .items
            .iter()
            .map(|label| crate::display_width::display_width(label))
            .max()
            .unwrap_or(0)
            .max(48);
        items.resize(
            items.len().max(self.state.items.len()).max(8),
            String::new(),
        );
        for item in &mut items {
            item.push_str(
                &" ".repeat(width.saturating_sub(crate::display_width::display_width(item))),
            );
        }
        items
    }

    fn set_aliases(&mut self, aliases: Vec<String>) {
        let selected = self.aliases.get(self.state.cursor).cloned();
        let open_suffix = t("zc-sidebar-picker-open-suffix");
        let labels = aliases
            .iter()
            .map(|alias| {
                if self.open_aliases.contains(alias) {
                    format!("{alias} {open_suffix}")
                } else {
                    alias.clone()
                }
            })
            .collect();
        let cursor = selected
            .as_ref()
            .and_then(|alias| aliases.iter().position(|item| item == alias))
            .unwrap_or(0);
        self.aliases = aliases;
        self.state = widgets::PickerState::new(labels, None);
        self.state.cursor = cursor;
        self.double_click = mouse::DoubleClickTracker::new();
    }
}

pub(crate) struct AgentSidebar {
    /// Scroll offset into the session rows.
    scroll: u16,
    // Geometry recorded by draw, read by the mouse handler (repo convention:
    // draw records, mouse reads). All `Rect::default()` while hidden.
    area: Rect,
    sessions_close_rect: Rect,
    minus_rect: Rect,
    minus_target: Option<(PaneKind, String)>,
    plus_rect: Rect,
    row_rects: Vec<(PaneKind, String, Rect)>,
    row_close_rects: Vec<(PaneKind, String, Rect)>,
    picker: Option<SidebarPicker>,
    /// Last successful display list, valid only for this RPC connection.
    cached_aliases: Vec<String>,
    cache_rpc: Weak<RpcClient>,
}

impl AgentSidebar {
    pub(crate) fn from_config_dir(_config_dir: &std::path::Path) -> Self {
        Self {
            scroll: 0,
            area: Rect::default(),
            sessions_close_rect: Rect::default(),
            minus_rect: Rect::default(),
            minus_target: None,
            plus_rect: Rect::default(),
            row_rects: Vec::new(),
            row_close_rects: Vec::new(),
            picker: None,
            cached_aliases: Vec::new(),
            cache_rpc: Weak::new(),
        }
    }

    pub(crate) fn picker_open(&self) -> bool {
        self.picker.is_some()
    }

    pub(crate) fn close_picker(&mut self) {
        self.picker = None;
    }

    /// Whether `(col, row)` falls inside the sidebar panel (not the picker).
    pub(crate) fn contains(&self, col: u16, row: u16) -> bool {
        self.area.width > 0 && mouse::in_rect(col, row, self.area)
    }

    /// Clear geometry recorded by the previous frame.
    fn reset_geometry(&mut self) {
        self.area = Rect::default();
        self.sessions_close_rect = Rect::default();
        self.minus_rect = Rect::default();
        self.minus_target = None;
        self.plus_rect = Rect::default();
        self.row_rects.clear();
        self.row_close_rects.clear();
    }

    /// Render the Sessions section into the shell-computed rectangle.
    pub(crate) fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        rows: &[SidebarSessionSummary],
        ctx: &SidebarCtx,
    ) {
        self.reset_geometry();
        self.area = area;
        frame.render_widget(Clear, area);
        let header_controls = if area.width >= 14 {
            12
        } else if area.width >= 10 {
            8
        } else if area.width >= 6 {
            4
        } else {
            0
        };
        let title = truncate_to_cells(
            &t("zc-sidebar-title"),
            area.width.saturating_sub(2 + header_controls) as usize,
        );
        let block = theme::panel_block(&title).style(theme::fill_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // The collapse affordance stays active while disconnected because it
        // only changes local sidebar state and persistence.
        if area.width >= 14 {
            let close = Rect {
                x: area.x + area.width - 12,
                y: area.y,
                width: 3,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    t("zc-sidebar-collapse"),
                    theme::accent_style(),
                )),
                close,
            );
            self.sessions_close_rect = close;
        }

        // Session controls target the visibly focused session; panel hiding
        // remains a separate local action.
        self.minus_target = rows
            .iter()
            .find(|summary| summary.focused && ctx.active_pane == Some(summary.pane_kind))
            .map(|summary| (summary.pane_kind, summary.session_id.clone()));
        if area.width >= 10 {
            let minus = Rect {
                x: area.x + area.width - 8,
                y: area.y,
                width: 3,
                height: 1,
            };
            let style = if ctx.connected && self.minus_target.is_some() {
                theme::accent_style()
            } else {
                theme::dim_style()
            };
            frame.render_widget(Paragraph::new(Span::styled("[-]", style)), minus);
            self.minus_rect = minus;
        }
        if area.width >= 6 {
            let plus = Rect {
                x: area.x + area.width - 4,
                y: area.y,
                width: 3,
                height: 1,
            };
            let plus_style = if ctx.connected {
                theme::accent_style()
            } else {
                theme::dim_style()
            };
            frame.render_widget(Paragraph::new(Span::styled("[+]", plus_style)), plus);
            self.plus_rect = plus;
        }

        if inner.height == 0 || inner.width == 0 {
            return;
        }

        // The `(N)` on each row is a message count; say so once at the bottom
        // of the panel. Only rendered when the row list leaves a free line —
        // scrolling lists and tight heights keep their row geometry untouched.
        if !rows.is_empty()
            && rows.len() < usize::from(inner.height)
            && crate::display_width::display_width(&t("zc-sidebar-count-hint"))
                < usize::from(inner.width)
        {
            let hint_area = Rect {
                y: inner.y + inner.height - 1,
                height: 1,
                ..inner
            };
            frame.render_widget(
                Paragraph::new(Span::styled(t("zc-sidebar-count-hint"), theme::dim_style())),
                hint_area,
            );
        }

        self.draw_session_rows(frame, inner, rows, ctx);
    }

    fn draw_session_rows(
        &mut self,
        frame: &mut Frame,
        rows_area: Rect,
        rows: &[SidebarSessionSummary],
        ctx: &SidebarCtx,
    ) {
        if rows_area.height == 0 {
            self.scroll = 0;
            return;
        }
        if rows.is_empty() {
            self.scroll = 0;
            let hint = Rect {
                y: rows_area.y + rows_area.height / 2,
                height: 1,
                ..rows_area
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    truncate_to_cells(&t("zc-sidebar-empty"), hint.width as usize),
                    theme::dim_style(),
                ))
                .centered(),
                hint,
            );
            return;
        }

        let visible = rows_area.height as usize;
        let max_scroll = rows.len().saturating_sub(visible) as u16;
        self.scroll = self.scroll.min(max_scroll);

        for (i, summary) in rows
            .iter()
            .skip(self.scroll as usize)
            .take(visible)
            .enumerate()
        {
            let row_rect = Rect {
                y: rows_area.y + i as u16,
                height: 1,
                ..rows_area
            };
            let focused_here = summary.focused && ctx.active_pane == Some(summary.pane_kind);
            let row_style = if focused_here {
                theme::selection_highlight(true, true)
            } else if summary.focused {
                theme::selection_highlight(false, true)
            } else {
                Style::default()
            };

            let date = session_activity_date(summary.last_activity.as_deref());
            let date_width = crate::display_width::display_width(&date);
            let count = if summary.message_count > 999 {
                "999+".to_string()
            } else {
                summary.message_count.to_string()
            };
            // The sidebar count is the turn-terminal projected conversation length
            // (the daemon's turn-end count) plus an optimistic in-flight user
            // message; the switch picker's counts come from the daemon's
            // durable store. The two measure different things and can differ.
            let close_width = u16::from(row_rect.width >= 6) * 2;
            let content_rect = Rect {
                width: row_rect.width.saturating_sub(close_width),
                ..row_rect
            };
            let count_label = format!(" ({count})");
            let count_width = crate::display_width::display_width(&count_label);
            let available = (content_rect.width as usize).saturating_sub(2 + count_width);
            // Keep the name before optional date metadata.
            let show_date = available >= date_width + 5;
            let name_width = available.saturating_sub(if show_date { date_width + 1 } else { 0 });
            let name = truncate_to_cells(
                &format!("{} #{}", summary.agent_alias, summary.display_ordinal),
                name_width,
            );
            let pad = name_width.saturating_sub(crate::display_width::display_width(&name));

            let status_glyph = match summary.status {
                SidebarStatus::Running => "\u{25b6} ",
                _ => "\u{25cf} ",
            };
            let mut spans = vec![
                Span::styled(status_glyph, status_style(summary.status)),
                Span::styled(name, theme::body_style()),
                Span::styled(count_label, theme::dim_style()),
            ];
            spans.push(Span::raw(" ".repeat(pad)));
            if show_date {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(date, theme::dim_style()));
            }
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(row_style),
                content_rect,
            );
            self.row_rects
                .push((summary.pane_kind, summary.session_id.clone(), row_rect));
            if close_width > 0 {
                let close_rect = Rect {
                    x: content_rect.right(),
                    y: row_rect.y,
                    width: close_width,
                    height: 1,
                };
                let close_style = if focused_here {
                    theme::selection_highlight(true, true)
                } else {
                    theme::dim_style()
                };
                frame.render_widget(
                    Paragraph::new(Span::styled("\u{2715}", close_style)),
                    close_rect,
                );
                self.row_close_rects.push((
                    summary.pane_kind,
                    summary.session_id.clone(),
                    close_rect,
                ));
            }
        }
    }

    /// Render the `+` picker modal (drawn above the panes, below the help
    /// overlay). Records the modal rect for click routing.
    pub(crate) fn draw_picker(&mut self, frame: &mut Frame, screen: Rect) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        let title = t("zc-sidebar-picker-title");
        let items = picker.display_items();
        let cursor = if !picker.aliases.is_empty() && picker.error.is_none() {
            picker.state.cursor
        } else {
            usize::MAX // no highlighted row for status-only content
        };
        picker.modal_rect =
            widgets::PickerModal::area_for(&title, &items, screen).unwrap_or_default();
        widgets::PickerModal::new(&title, &items, cursor).render(frame, screen);
    }

    /// Open the picker targeting `pane`, spawning a background agent fetch.
    /// `open_aliases` marks aliases already open without changing launch behavior.
    pub(crate) fn open_picker(
        &mut self,
        target: PaneKind,
        open_aliases: HashSet<String>,
        rpc: &Arc<RpcClient>,
    ) {
        if !self
            .cache_rpc
            .upgrade()
            .is_some_and(|cached| Arc::ptr_eq(&cached, rpc))
        {
            self.cached_aliases.clear();
        }
        self.cache_rpc = Arc::downgrade(rpc);
        let (tx, rx) = mpsc::unbounded_channel();
        let rpc = Arc::clone(rpc);
        tokio::spawn(async move {
            let result = match rpc.agents_list().await {
                Ok(result) => Ok(result
                    .agents
                    .into_iter()
                    .filter(|a| a.enabled)
                    .map(|a| a.alias)
                    .collect::<Vec<_>>()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(result);
        });
        self.picker = Some(SidebarPicker {
            target,
            state: widgets::PickerState::default(),
            aliases: Vec::new(),
            open_aliases,
            loading: true,
            opened_at: Instant::now(),
            error: None,
            rx,
            double_click: mouse::DoubleClickTracker::new(),
            modal_rect: Rect::default(),
        });
        if let Some(picker) = self.picker.as_mut() {
            picker.set_aliases(self.cached_aliases.clone());
        }
    }

    /// Drain the background agent fetch, if one is pending. Called once per
    /// tick from the app loop.
    pub(crate) fn drain_picker_fetch(&mut self) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        if !picker.loading {
            return;
        }
        match picker.rx.try_recv() {
            Ok(Ok(aliases)) => {
                picker.loading = false;
                self.cached_aliases.clone_from(&aliases);
                picker.set_aliases(aliases);
            }
            Ok(Err(e)) => {
                self.cached_aliases.clear();
                picker.loading = false;
                picker.error = Some(e);
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                self.cached_aliases.clear();
                picker.loading = false;
                picker.error = Some(t("zc-sidebar-picker-disconnected"));
            }
        }
    }

    /// Keys while the picker is open. Returns an event on confirm; `Cancel`
    /// and confirms on status-only content close the picker.
    pub(crate) fn handle_picker_key(&mut self, key: &KeyEvent) -> Option<SidebarEvent> {
        let picker = self.picker.as_mut()?;
        match ModalAction::from_chord(key) {
            Some(ModalAction::Up) => {
                picker.state.move_up();
                None
            }
            Some(ModalAction::Down) => {
                picker.state.move_down();
                None
            }
            Some(ModalAction::Confirm) => {
                if picker.loading {
                    return None;
                }
                let event = Self::picker_confirm(picker);
                self.picker = None;
                event
            }
            Some(ModalAction::Cancel) => {
                self.picker = None;
                None
            }
            _ => None,
        }
    }

    fn picker_confirm(picker: &SidebarPicker) -> Option<SidebarEvent> {
        if !picker.selectable() {
            return None;
        }
        let alias = picker.aliases.get(picker.state.cursor)?.clone();
        Some(SidebarEvent::PickAgent {
            pane: picker.target,
            alias,
        })
    }

    /// Mouse routing. The app forwards events here when the picker is open
    /// or the click falls inside the sidebar area.
    pub(crate) fn handle_mouse(&mut self, mouse: &MouseEvent) -> Option<SidebarEvent> {
        if self.picker.is_some() {
            return self.handle_picker_mouse(mouse);
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let (col, row) = (mouse.column, mouse.row);
                if mouse::in_rect(col, row, self.sessions_close_rect) {
                    return Some(SidebarEvent::ToggleVisibility);
                }
                if mouse::in_rect(col, row, self.plus_rect) {
                    return Some(SidebarEvent::OpenPicker);
                }
                if mouse::in_rect(col, row, self.minus_rect)
                    && let Some((pane, session_id)) = self.minus_target.clone()
                {
                    return Some(SidebarEvent::CloseSession { pane, session_id });
                }
                for (pane, sid, rect) in &self.row_close_rects {
                    if mouse::in_rect(col, row, *rect) {
                        return Some(SidebarEvent::CloseSession {
                            pane: *pane,
                            session_id: sid.clone(),
                        });
                    }
                }
                for (pane, sid, rect) in &self.row_rects {
                    if mouse::in_rect(col, row, *rect) {
                        return Some(SidebarEvent::FocusSession {
                            pane: *pane,
                            session_id: sid.clone(),
                        });
                    }
                }
                None
            }
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(1);
                None
            }
            MouseEventKind::ScrollDown => {
                // Clamped against the row count on the next draw.
                self.scroll = self.scroll.saturating_add(1);
                None
            }
            _ => None,
        }
    }

    fn handle_picker_mouse(&mut self, mouse: &MouseEvent) -> Option<SidebarEvent> {
        let picker = self.picker.as_mut()?;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let (col, row) = (mouse.column, mouse.row);
                if !mouse::in_rect(col, row, picker.modal_rect) {
                    self.picker = None;
                    return None;
                }
                if !picker.selectable() {
                    return None;
                }
                if let Some(idx) = mouse::list_click_index(
                    row,
                    picker.modal_rect,
                    picker
                        .state
                        .cursor
                        .saturating_add(1)
                        .saturating_sub(picker.modal_rect.height.saturating_sub(2) as usize),
                    picker.aliases.len(),
                ) {
                    picker.state.cursor = idx;
                    if picker.double_click.click(col, row) {
                        let event = Self::picker_confirm(picker);
                        self.picker = None;
                        return event;
                    }
                }
                None
            }
            MouseEventKind::ScrollUp => {
                picker.state.move_up();
                None
            }
            MouseEventKind::ScrollDown => {
                picker.state.move_down();
                None
            }
            _ => None,
        }
    }
}

fn status_style(status: SidebarStatus) -> Style {
    match status {
        SidebarStatus::Ready => theme::status_ready_style(),
        SidebarStatus::Running => theme::status_running_style(),
        SidebarStatus::NeedsHuman => theme::status_attention_style(),
        SidebarStatus::Errored => theme::status_error_style(),
    }
}

fn session_activity_date(last_activity: Option<&str>) -> String {
    last_activity
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| {
            value
                .with_timezone(&chrono::Local)
                .format("%m/%d")
                .to_string()
        })
        .unwrap_or_else(|| t("zc-sidebar-date-placeholder"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyModifiers, MouseEventKind};

    fn rendered_row(buffer: &ratatui::buffer::Buffer, rect: Rect) -> String {
        (rect.x..rect.x + rect.width)
            .map(|x| buffer[(x, rect.y)].symbol())
            .collect()
    }

    fn sidebar() -> AgentSidebar {
        AgentSidebar {
            scroll: 0,
            area: Rect::default(),
            sessions_close_rect: Rect::default(),
            minus_rect: Rect::default(),
            minus_target: None,
            plus_rect: Rect::default(),
            row_rects: Vec::new(),
            row_close_rects: Vec::new(),
            picker: None,
            cached_aliases: Vec::new(),
            cache_rpc: Weak::new(),
        }
    }

    fn summary(alias: &str, sid: &str, focused: bool) -> SidebarSessionSummary {
        summary_in_pane(alias, sid, PaneKind::Chat, focused)
    }

    fn summary_in_pane(
        alias: &str,
        sid: &str,
        pane_kind: PaneKind,
        focused: bool,
    ) -> SidebarSessionSummary {
        SidebarSessionSummary {
            session_id: sid.into(),
            agent_alias: alias.into(),
            message_count: 0,
            status: SidebarStatus::Ready,
            pane_kind,
            focused,
            display_ordinal: 1,
            last_activity: Some("2026-01-02T12:00:00Z".into()),
        }
    }

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn truncation_respects_terminal_cell_width_and_graphemes() {
        let title = truncate_to_cells("セッション", 8);
        assert!(crate::display_width::display_width(&title) <= 8);
        assert!(title.ends_with('\u{2026}'));

        let emoji = truncate_to_cells(
            "team \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} workspace",
            10,
        );
        assert!(crate::display_width::display_width(&emoji) <= 10);
        assert!(!emoji.ends_with('\u{200d}'));
    }

    #[test]
    fn draw_records_sessions_controls_and_routes() {
        let mut s = sidebar();
        let area = Rect::new(0, 1, 40, 12);
        let rows = vec![summary("alpha", "s1", true), summary("beta", "s2", false)];
        let ctx = SidebarCtx {
            active_pane: Some(PaneKind::Chat),
            connected: true,
        };
        let backend = ratatui::backend::TestBackend::new(100, 14);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|frame| s.draw(frame, area, &rows, &ctx)).unwrap();

        assert_eq!(s.row_rects.len(), 2);
        assert!(
            s.sessions_close_rect.width > 0,
            "sessions close affordance recorded"
        );
        assert!(s.plus_rect.width > 0, "plus affordance recorded");
        let rect = s.minus_rect;
        assert_eq!(s.minus_target, Some((PaneKind::Chat, "s1".into())));
        assert_eq!(s.row_close_rects.len(), 2);
        let first_row = rendered_row(term.backend().buffer(), s.row_rects[0].2);
        assert!(first_row.contains("alpha #1"));
        let expected_date = chrono::DateTime::parse_from_rfc3339("2026-01-02T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%m/%d")
            .to_string();
        assert!(first_row.contains(&expected_date));
        let rendered: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!rendered.contains(&t("zc-pane-quickstart")));

        // Click routing through the recorded rects.
        let (_, sid, row2) = s.row_rects[1].clone();
        assert_eq!(
            s.handle_mouse(&click(row2.x + 1, row2.y)),
            Some(SidebarEvent::FocusSession {
                pane: PaneKind::Chat,
                session_id: sid,
            })
        );
        let (_, _, row_close) = s.row_close_rects[1].clone();
        assert_eq!(
            s.handle_mouse(&click(row_close.x, row_close.y)),
            Some(SidebarEvent::CloseSession {
                pane: PaneKind::Chat,
                session_id: "s2".into(),
            })
        );
        assert_eq!(
            s.handle_mouse(&click(rect.x, rect.y)),
            Some(SidebarEvent::CloseSession {
                pane: PaneKind::Chat,
                session_id: "s1".into(),
            })
        );
        assert_eq!(
            s.handle_mouse(&click(s.plus_rect.x, s.plus_rect.y)),
            Some(SidebarEvent::OpenPicker)
        );
        assert_eq!(
            s.handle_mouse(&click(s.sessions_close_rect.x, s.sessions_close_rect.y)),
            Some(SidebarEvent::ToggleVisibility)
        );
    }

    #[test]
    fn duplicate_alias_rows_have_distinct_labels_and_session_targets() {
        let mut sidebar = sidebar();
        let area = Rect::new(0, 0, 40, 10);
        let mut rows = vec![summary("alpha", "s1", true), summary("alpha", "s2", false)];
        rows[1].display_ordinal = 2;
        let ctx = SidebarCtx {
            active_pane: Some(PaneKind::Chat),
            connected: true,
        };
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 10)).unwrap();
        term.draw(|frame| sidebar.draw(frame, area, &rows, &ctx))
            .unwrap();
        for (idx, (_, sid, rect)) in sidebar.row_rects.clone().into_iter().enumerate() {
            let text = rendered_row(term.backend().buffer(), rect);
            assert!(text.contains(&format!("alpha #{}", idx + 1)), "{text}");
            assert_eq!(
                sidebar.handle_mouse(&click(rect.x + 1, rect.y)),
                Some(SidebarEvent::FocusSession {
                    pane: PaneKind::Chat,
                    session_id: sid,
                })
            );
        }
    }

    #[test]
    fn running_row_keeps_count_and_both_close_targets() {
        let mut sidebar = sidebar();
        let area = Rect::new(0, 0, 40, 8);
        let mut row = summary("long-agent-name", "s1", true);
        row.status = SidebarStatus::Running;
        row.message_count = 42;
        let ctx = SidebarCtx {
            active_pane: Some(PaneKind::Chat),
            connected: true,
        };
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 8)).unwrap();
        term.draw(|frame| sidebar.draw(frame, area, &[row], &ctx))
            .unwrap();

        let (_, _, rect) = sidebar.row_rects[0].clone();
        let text: String = (rect.x..rect.right())
            .map(|x| term.backend().buffer()[(x, rect.y)].symbol())
            .collect();
        assert!(
            text.contains("\u{25b6}"),
            "running state must not rely on color: {text}"
        );
        assert!(
            text.contains("(42)"),
            "local message count must remain visible: {text}"
        );
        let close = sidebar.minus_rect;
        assert_eq!(
            sidebar.handle_mouse(&click(close.x, close.y)),
            Some(SidebarEvent::CloseSession {
                pane: PaneKind::Chat,
                session_id: "s1".into(),
            })
        );
        let (_, _, row_close) = sidebar.row_close_rects[0].clone();
        assert_eq!(
            sidebar.handle_mouse(&click(row_close.x, row_close.y)),
            Some(SidebarEvent::CloseSession {
                pane: PaneKind::Chat,
                session_id: "s1".into(),
            })
        );
    }

    #[test]
    fn draw_shows_open_sessions_header_and_count_hint() {
        let mut s = sidebar();
        let area = Rect::new(0, 1, 40, 10);
        let mut first = summary("alpha", "s1", true);
        first.message_count = 12;
        let mut second = summary("beta", "s2", false);
        second.message_count = 999;
        let rows = vec![first, second];
        let ctx = SidebarCtx {
            active_pane: Some(PaneKind::Chat),
            connected: true,
        };
        let backend = ratatui::backend::TestBackend::new(60, 14);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|frame| s.draw(frame, area, &rows, &ctx)).unwrap();

        let header = rendered_row(
            term.backend().buffer(),
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 1,
            },
        );
        assert!(
            header.contains(&t("zc-sidebar-title")),
            "header must carry the sidebar title: {header:?}"
        );

        // The hint explains the per-row `(N)`; it draws on the free line at
        // the bottom of the panel, below the last row.
        let hint_row = rendered_row(
            term.backend().buffer(),
            Rect {
                x: area.x,
                y: area.y + area.height - 2,
                width: area.width,
                height: 1,
            },
        );
        assert!(
            hint_row.contains(&t("zc-sidebar-count-hint")),
            "count hint must render on the free bottom line: {hint_row:?}"
        );

        let first_row = rendered_row(term.backend().buffer(), s.row_rects[0].2);
        assert!(first_row.contains("alpha #1"));
        assert!(first_row.contains("(12)"));
        let second_row = rendered_row(term.backend().buffer(), s.row_rects[1].2);
        assert!(second_row.contains("(999)"));
    }

    #[test]
    fn count_hint_is_dropped_when_rows_overflow_the_panel() {
        let mut s = sidebar();
        let area = Rect::new(0, 0, 40, 6);
        let rows: Vec<_> = (0..5)
            .map(|i| summary(&format!("a{i}"), &format!("s{i}"), false))
            .collect();
        let ctx = SidebarCtx {
            active_pane: None,
            connected: true,
        };
        let backend = ratatui::backend::TestBackend::new(100, 6);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|frame| s.draw(frame, area, &rows, &ctx)).unwrap();

        let rendered: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            !rendered.contains(&t("zc-sidebar-count-hint")),
            "a scrolling list must keep every line for rows"
        );
    }

    #[test]
    fn rows_show_only_agent_ordinal_and_honest_date() {
        let mut s = sidebar();
        let area = Rect::new(0, 0, 40, 8);
        let rows = vec![
            summary_in_pane("same-agent", "chat", PaneKind::Chat, true),
            summary_in_pane("same-agent", "code", PaneKind::Acp, false),
        ];
        let ctx = SidebarCtx {
            active_pane: Some(PaneKind::Chat),
            connected: true,
        };
        let backend = ratatui::backend::TestBackend::new(40, 8);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|frame| s.draw(frame, area, &rows, &ctx)).unwrap();

        let chat_row = rendered_row(term.backend().buffer(), s.row_rects[0].2);
        let code_row = rendered_row(term.backend().buffer(), s.row_rects[1].2);
        let expected_date = chrono::DateTime::parse_from_rfc3339("2026-01-02T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%m/%d")
            .to_string();
        assert!(chat_row.contains("same-agent #1"));
        assert!(chat_row.contains(&expected_date));
        assert!(!chat_row.contains(&t("zc-pane-chat")));
        assert!(chat_row.contains("\u{2715}"));
        assert!(code_row.contains("same-agent #1"));
        assert!(!code_row.contains(&t("zc-pane-code")));
        assert!(code_row.contains("\u{2715}"));
    }

    #[test]
    fn invalid_activity_uses_neutral_placeholder() {
        let mut s = sidebar();
        let area = Rect::new(0, 0, 40, 6);
        let mut row = summary("agent", "session", true);
        row.last_activity = Some("not-a-date".into());
        let rows = vec![row];

        let ctx = SidebarCtx {
            active_pane: Some(PaneKind::Chat),
            connected: true,
        };
        let backend = ratatui::backend::TestBackend::new(40, 6);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|frame| s.draw(frame, area, &rows, &ctx)).unwrap();

        let row = rendered_row(term.backend().buffer(), s.row_rects[0].2);
        assert!(row.contains("agent #1"));
        assert!(row.contains(&t("zc-sidebar-date-placeholder")));
        assert!(row.contains("\u{2715}"));
    }

    #[test]
    fn running_row_is_explicit_and_count_is_capped_without_losing_close_target() {
        let mut s = sidebar();
        let area = Rect::new(0, 0, crate::config::SIDEBAR_WIDTH_MIN, 8);
        let mut row = summary("long-agent-name", "s1", true);
        row.status = SidebarStatus::Running;
        row.message_count = 12_345;
        let rows = vec![row];
        let ctx = SidebarCtx {
            active_pane: Some(PaneKind::Chat),
            connected: true,
        };
        let backend = ratatui::backend::TestBackend::new(area.width, area.height);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|frame| s.draw(frame, area, &rows, &ctx)).unwrap();

        let (_, _, rect) = s.row_rects[0].clone();
        let text: String = (rect.x..rect.right())
            .map(|x| term.backend().buffer()[(x, rect.y)].symbol())
            .collect();
        assert!(
            text.contains("\u{25b6}"),
            "running state must not rely on color: {text}"
        );
        assert!(
            text.contains("(999+)"),
            "large counts stay width-bounded: {text}"
        );
        assert!(
            text.contains("long"),
            "the session name stays visible: {text}"
        );
        assert!(!text.contains(&t("zc-sidebar-date-placeholder")));
        let close = s.minus_rect;
        assert_eq!(
            term.backend().buffer()[(close.x + 1, close.y)].symbol(),
            "-"
        );
        assert_eq!(
            s.handle_mouse(&click(close.x + 1, close.y)),
            Some(SidebarEvent::CloseSession {
                pane: PaneKind::Chat,
                session_id: "s1".into(),
            })
        );
    }

    #[test]
    fn session_controls_are_distinct_and_rows_expose_close_targets() {
        for width in [crate::config::SIDEBAR_WIDTH_MIN, 40] {
            let mut s = sidebar();
            let area = Rect::new(0, 0, width, 8);
            let mut code = summary("code", "code-session", true);
            code.pane_kind = PaneKind::Acp;
            code.status = SidebarStatus::Running;
            let rows = vec![summary("chat", "chat-session", true), code];
            let ctx = SidebarCtx {
                active_pane: Some(PaneKind::Acp),
                connected: true,
            };
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 8)).unwrap();
            term.draw(|frame| s.draw(frame, area, &rows, &ctx)).unwrap();
            for (rect, label, action) in [
                (s.sessions_close_rect, "[x]", SidebarEvent::ToggleVisibility),
                (
                    s.minus_rect,
                    "[-]",
                    SidebarEvent::CloseSession {
                        pane: PaneKind::Acp,
                        session_id: "code-session".into(),
                    },
                ),
                (s.plus_rect, "[+]", SidebarEvent::OpenPicker),
            ] {
                assert_eq!(rendered_row(term.backend().buffer(), rect), label);
                for x in rect.x..rect.right() {
                    assert_eq!(s.handle_mouse(&click(x, rect.y)), Some(action.clone()));
                }
            }
            assert_eq!(s.row_close_rects.len(), s.row_rects.len());
            for (idx, (pane, session_id, rect)) in s.row_rects.clone().into_iter().enumerate() {
                let (close_pane, close_session_id, close_rect) = s.row_close_rects[idx].clone();
                assert_eq!((close_pane, &close_session_id), (pane, &session_id));
                for x in rect.x..close_rect.x {
                    assert_eq!(
                        s.handle_mouse(&click(x, rect.y)),
                        Some(SidebarEvent::FocusSession {
                            pane,
                            session_id: session_id.clone()
                        })
                    );
                }
                for x in close_rect.x..close_rect.right() {
                    assert_eq!(
                        s.handle_mouse(&click(x, close_rect.y)),
                        Some(SidebarEvent::CloseSession {
                            pane,
                            session_id: session_id.clone()
                        })
                    );
                }
            }
        }
    }

    #[test]
    fn scroll_clamps_to_row_overflow() {
        let mut s = sidebar();
        let area = Rect::new(0, 0, 40, 6);
        let rows: Vec<_> = (0..5)
            .map(|i| summary(&format!("a{i}"), &format!("s{i}"), false))
            .collect();
        let ctx = SidebarCtx {
            active_pane: None,
            connected: true,
        };
        s.scroll = 99;
        let backend = ratatui::backend::TestBackend::new(100, 6);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|frame| s.draw(frame, area, &rows, &ctx)).unwrap();
        assert_eq!(s.scroll, 1, "scroll clamps to rows.len() - visible");
        assert_eq!(s.row_rects.len(), 4);
        assert_eq!(s.row_rects[0].1, "s1", "clamped scroll shows the tail");
    }

    #[tokio::test]
    async fn picker_labels_mark_open_aliases_but_confirm_creates_a_session() {
        let mut s = sidebar();
        let (tx, rx) = mpsc::unbounded_channel();
        s.picker = Some(SidebarPicker {
            target: PaneKind::Chat,
            state: widgets::PickerState::default(),
            aliases: Vec::new(),
            open_aliases: HashSet::from(["alpha".to_string()]),
            loading: true,
            error: None,
            opened_at: Instant::now(),
            rx,
            double_click: mouse::DoubleClickTracker::new(),
            modal_rect: Rect::default(),
        });
        tx.send(Ok(vec!["alpha".to_string(), "beta".to_string()]))
            .unwrap();
        s.drain_picker_fetch();

        let picker = s.picker.as_ref().unwrap();
        assert!(picker.state.items[0].ends_with(&t("zc-sidebar-picker-open-suffix")));
        assert_eq!(picker.state.items[1], "beta");

        let confirm = KeyEvent::from(crossterm::event::KeyCode::Enter);
        let event = s.handle_picker_key(&confirm);
        assert_eq!(
            event,
            Some(SidebarEvent::PickAgent {
                pane: PaneKind::Chat,
                alias: "alpha".into(),
            })
        );
        assert!(!s.picker_open(), "confirm closes the picker");
    }

    #[tokio::test]
    async fn picker_refresh_keeps_cached_rows_and_selection_until_current_reply() {
        let (tx, mut requests) = mpsc::channel::<String>(16);
        let outbound = Arc::new(crate::jsonrpc::RpcOutbound::new(tx));
        let rpc = Arc::new(RpcClient::with_rpc(Arc::clone(&outbound)));
        let mut s = sidebar();
        s.cache_rpc = Arc::downgrade(&rpc);
        s.cached_aliases = vec!["alpha".into(), "beta".into()];
        s.open_picker(PaneKind::Chat, HashSet::new(), &rpc);
        let initial = s.picker.as_ref().unwrap().display_items();
        assert_eq!(initial[0].trim(), "alpha");
        assert_eq!(
            s.handle_picker_key(&KeyEvent::from(crossterm::event::KeyCode::Enter)),
            None
        );
        assert!(
            s.picker_open(),
            "pending refresh must not dismiss or launch"
        );
        s.handle_picker_key(&KeyEvent::from(crossterm::event::KeyCode::Down));

        let request: serde_json::Value =
            serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
        assert_eq!(request["method"], crate::client::method::AGENTS_LIST);
        outbound.dispatch_response(
            request["id"].as_str().unwrap(),
            Some(serde_json::json!({
                "agents": [
                    {"alias": "disabled", "enabled": false},
                    {"alias": "beta", "enabled": true},
                    {"alias": "gamma", "enabled": true}
                ]
            })),
            None,
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while s.picker.as_ref().unwrap().loading {
                tokio::task::yield_now().await;
                s.drain_picker_fetch();
            }
        })
        .await
        .unwrap();
        let picker = s.picker.as_ref().unwrap();
        assert_eq!(picker.aliases, ["beta", "gamma"]);
        assert_eq!(
            picker.state.cursor, 0,
            "selection follows beta, not row index"
        );
        let refreshed = picker.display_items();
        assert_eq!(
            widgets::PickerModal::area_for("Agents", &initial, Rect::new(0, 0, 80, 24)),
            widgets::PickerModal::area_for("Agents", &refreshed, Rect::new(0, 0, 80, 24))
        );

        s.close_picker();
        s.open_picker(PaneKind::Acp, HashSet::new(), &rpc);
        assert_eq!(s.picker.as_ref().unwrap().aliases, ["beta", "gamma"]);
        let abandoned: serde_json::Value =
            serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
        s.close_picker();
        s.open_picker(PaneKind::Chat, HashSet::new(), &rpc);
        outbound.dispatch_response(
            abandoned["id"].as_str().unwrap(),
            Some(serde_json::json!({"agents": [{"alias": "old", "enabled": true}]})),
            None,
        );
        tokio::task::yield_now().await;
        s.drain_picker_fetch();
        assert_eq!(s.picker.as_ref().unwrap().aliases, ["beta", "gamma"]);

        let (other_tx, _other_rx) = mpsc::channel::<String>(16);
        let other = Arc::new(RpcClient::with_rpc(Arc::new(
            crate::jsonrpc::RpcOutbound::new(other_tx),
        )));
        s.open_picker(PaneKind::Chat, HashSet::new(), &other);
        let cold = s.picker.as_mut().unwrap();
        assert!(
            cold.aliases.is_empty(),
            "reconnect invalidates the display cache"
        );
        assert!(cold.display_items().iter().all(|row| row.trim().is_empty()));
        cold.opened_at = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            cold.display_items()[0].trim(),
            t("zc-sidebar-picker-loading")
        );
    }

    #[tokio::test]
    async fn picker_error_row_is_not_selectable() {
        let mut s = sidebar();
        let (tx, rx) = mpsc::unbounded_channel();
        s.picker = Some(SidebarPicker {
            target: PaneKind::Acp,
            state: widgets::PickerState::default(),
            aliases: Vec::new(),
            open_aliases: HashSet::new(),
            loading: true,
            error: None,
            opened_at: Instant::now(),
            rx,
            double_click: mouse::DoubleClickTracker::new(),
            modal_rect: Rect::default(),
        });
        tx.send(Err("socket closed".to_string())).unwrap();
        s.drain_picker_fetch();

        let confirm = KeyEvent::from(crossterm::event::KeyCode::Enter);
        assert_eq!(s.handle_picker_key(&confirm), None);
        assert!(!s.picker_open(), "confirm on an error row closes");
    }

    #[test]
    fn disconnected_picker_fetch_becomes_a_terminal_error_row() {
        let mut s = sidebar();
        let (tx, rx) = mpsc::unbounded_channel::<Result<Vec<String>, String>>();
        s.picker = Some(SidebarPicker {
            target: PaneKind::Chat,
            state: widgets::PickerState::default(),
            aliases: Vec::new(),
            open_aliases: HashSet::new(),
            loading: true,
            error: None,
            opened_at: Instant::now(),
            rx,
            double_click: mouse::DoubleClickTracker::new(),
            modal_rect: Rect::default(),
        });
        drop(tx);

        s.drain_picker_fetch();

        let picker = s.picker.as_ref().unwrap();
        assert!(!picker.loading);
        assert_eq!(
            picker.error.as_deref(),
            Some(t("zc-sidebar-picker-disconnected").as_str())
        );
    }
}
