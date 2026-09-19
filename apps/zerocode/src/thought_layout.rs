// Adapted from Ratatui's MIT-licensed `reflow::WordWrapper` at
// 0a2a7c0363a4806b0cf05c1915bf7cdd438f756c. Keeping the row boundaries here
// avoids rerunning its private wrapper for an unchanged long thought.
//
// Copyright (c) 2016-2022 Florian Dehau
// Copyright (c) 2023-2025 The Ratatui Developers
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use std::collections::VecDeque;
use std::mem;
use std::ops::Range;
use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, StyledGrapheme};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::text_selection::{TextRowBreak, row_breaks_for_line};

const PREFIX: &str = "(thinking) ";

#[derive(Clone, Debug)]
struct Symbol {
    virtual_range: Range<usize>,
    span: usize,
    span_range: Range<usize>,
    width: u16,
    whitespace: bool,
}

#[derive(Clone, Debug)]
struct SourcePiece {
    virtual_range: Range<usize>,
    span: usize,
    span_range: Range<usize>,
    symbols: Range<usize>,
}

/// Cached physical-row geometry for one canonical styled transcript line.
///
/// The layout owns no text. `ChatState::cached_lines` remains the source of
/// truth, while this derived index lets steady draws visit only visible rows.
/// Exact grapheme ends and widths are retained so repaint never repeats the
/// Unicode segmentation already paid during cache construction.
#[derive(Clone, Debug)]
pub(crate) struct WrappedLineLayout {
    width: u16,
    pieces: Vec<SourcePiece>,
    symbol_ends: Vec<usize>,
    symbol_widths: Vec<u16>,
    row_ends: Vec<usize>,
    row_widths: Vec<u16>,
    alignment: Alignment,
    #[cfg(test)]
    generation: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WrappedRangeRun {
    pub(crate) row: u16,
    pub(crate) column: u16,
    pub(crate) width: u16,
    pub(crate) range_index: usize,
}

impl WrappedLineLayout {
    pub(crate) fn new(line: &Line<'_>, width: u16) -> Self {
        let (pieces, symbol_ends, symbol_widths, row_ends, row_widths) =
            wrap_symbols(symbols(line), width);
        #[cfg(test)]
        static GENERATION: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        Self {
            width,
            pieces,
            symbol_ends,
            symbol_widths,
            row_ends,
            row_widths,
            alignment: line.alignment.unwrap_or(Alignment::Left),
            #[cfg(test)]
            generation: GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }

    pub(crate) fn row_count(&self) -> u16 {
        u16::try_from(self.row_ends.len()).unwrap_or(u16::MAX)
    }

    pub(crate) fn render(
        &self,
        line: &Line<'_>,
        rows: Range<usize>,
        base_style: Style,
        area: Rect,
        buffer: &mut Buffer,
    ) {
        buffer.set_style(area, base_style);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buffer[(x, y)].set_symbol(" ");
            }
        }
        let end = rows.end.min(self.row_ends.len());
        for (screen_row, row) in (rows.start.min(end)..end).enumerate() {
            let start = if row == 0 { 0 } else { self.row_ends[row - 1] };
            let row_width = self.row_widths[row];
            let mut column = match self.alignment {
                Alignment::Center => (area.width / 2).saturating_sub(row_width / 2),
                Alignment::Right => area.width.saturating_sub(row_width),
                Alignment::Left => 0,
            };
            let line_style = base_style.patch(line.style);
            for piece in &self.pieces[start..self.row_ends[row]] {
                let Some(span) = line.spans.get(piece.span) else {
                    continue;
                };
                let style = line_style.patch(span.style);
                let mut symbol_start = piece.span_range.start;
                for symbol_index in piece.symbols.clone() {
                    let symbol_end = self.symbol_ends[symbol_index];
                    let symbol_width = self.symbol_widths[symbol_index];
                    if symbol_width > 0 {
                        if column >= area.width {
                            break;
                        }
                        let text = &span.content[symbol_start..symbol_end];
                        buffer[(area.x + column, area.y + screen_row as u16)]
                            .set_symbol(if text.is_empty() { " " } else { text })
                            .set_style(style);
                        column = column.saturating_add(symbol_width);
                    }
                    symbol_start = symbol_end;
                }
            }
        }
    }

    pub(crate) fn range_runs(
        &self,
        line: &Line<'_>,
        ranges: &[(usize, usize, String)],
    ) -> Vec<WrappedRangeRun> {
        let mut runs: Vec<WrappedRangeRun> = Vec::new();
        let mut range_index = 0;
        // Transcript coordinates are `u16`; never wrap unreachable rows back
        // into the visible range when one pathological line exceeds the cap.
        for row in 0..usize::from(self.row_count()) {
            let start = if row == 0 { 0 } else { self.row_ends[row - 1] };
            let mut column = match self.alignment {
                Alignment::Center => (self.width / 2).saturating_sub(self.row_widths[row] / 2),
                Alignment::Right => self.width.saturating_sub(self.row_widths[row]),
                Alignment::Left => 0,
            };
            for piece in &self.pieces[start..self.row_ends[row]] {
                let Some(span) = line.spans.get(piece.span) else {
                    continue;
                };
                let text = &span.content[piece.span_range.clone()];
                for (relative, symbol) in text.grapheme_indices(true) {
                    let symbol_width = symbol.width() as u16;
                    if symbol_width == 0 || column >= self.width {
                        continue;
                    }
                    let source = piece.virtual_range.start + relative;
                    while range_index < ranges.len() && ranges[range_index].1 <= source {
                        range_index += 1;
                    }
                    if let Some((lo, hi, _)) = ranges.get(range_index)
                        && source >= *lo
                        && source < *hi
                    {
                        let width = symbol_width.min(self.width - column);
                        if let Some(previous) = runs.last_mut()
                            && previous.row == row as u16
                            && previous.column + previous.width == column
                            && previous.range_index == range_index
                        {
                            previous.width += width;
                        } else {
                            runs.push(WrappedRangeRun {
                                row: row as u16,
                                column,
                                width,
                                range_index,
                            });
                        }
                    }
                    column = column.saturating_add(symbol_width);
                }
            }
        }
        runs
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> usize {
        self.generation
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ThoughtLayout {
    text: Arc<str>,
    layout: WrappedLineLayout,
    row_breaks: Vec<TextRowBreak>,
    urls: Vec<(usize, usize, String)>,
    url_runs: Vec<WrappedRangeRun>,
}

impl ThoughtLayout {
    #[cfg(test)]
    pub(crate) fn new(text: Arc<str>, width: u16) -> Self {
        Self::with_urls(text, width, Vec::new())
    }

    pub(crate) fn with_urls(text: Arc<str>, width: u16, urls: Vec<(usize, usize, String)>) -> Self {
        let (layout, url_runs) = {
            let line = Line::from(vec![Span::raw(PREFIX), Span::raw(text.as_ref())]);
            let layout = WrappedLineLayout::new(&line, width);
            let url_runs = layout.range_runs(&line, &urls);
            (layout, url_runs)
        };
        // Keep the existing selection/copy separator contract; layout changes
        // must not turn soft wrapping into newlines in copied text.
        let row_breaks = row_breaks_for_line(
            &Line::from(vec![Span::raw(PREFIX), Span::raw(text.to_string())]),
            width,
        );
        Self {
            text,
            layout,
            row_breaks,
            urls,
            url_runs,
        }
    }

    pub(crate) fn url_runs(
        &self,
        rows: Range<u16>,
    ) -> impl Iterator<Item = (u16, u16, u16, usize, &str)> {
        let start = self.url_runs.partition_point(|run| run.row < rows.start);
        self.url_runs[start..]
            .iter()
            .take_while(move |run| run.row < rows.end)
            .map(|run| {
                let (byte_start, _, url) = &self.urls[run.range_index];
                (run.row, run.column, run.width, *byte_start, url.as_str())
            })
    }

    pub(crate) fn matches(&self, text: &Arc<str>, width: u16) -> bool {
        self.layout.width == width && Arc::ptr_eq(&self.text, text)
    }

    pub(crate) fn row_count(&self) -> u16 {
        self.layout.row_count()
    }

    pub(crate) fn width(&self) -> u16 {
        self.layout.width
    }

    pub(crate) fn row_breaks(&self) -> &[TextRowBreak] {
        &self.row_breaks
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> usize {
        self.layout.generation()
    }

    pub(crate) fn render(
        &self,
        rows: Range<usize>,
        prefix_style: Style,
        body_style: Style,
        area: Rect,
        buffer: &mut Buffer,
    ) {
        let line = Line::from(vec![
            Span::styled(PREFIX, prefix_style),
            Span::styled(self.text.as_ref(), body_style),
        ]);
        self.layout
            .render(&line, rows, Style::default(), area, buffer);
    }
}

fn symbols(line: &Line<'_>) -> Vec<Symbol> {
    let mut virtual_offset = 0;
    line.spans
        .iter()
        .enumerate()
        .flat_map(|(span, source)| {
            let offset = virtual_offset;
            virtual_offset += source.content.len();
            UnicodeSegmentation::grapheme_indices(source.content.as_ref(), true)
                .map(move |(start, symbol)| (offset, span, start, symbol))
        })
        .filter_map(|(offset, span, start, symbol)| {
            if symbol.contains(char::is_control) {
                return None;
            }
            let styled = StyledGrapheme {
                symbol,
                style: Style::default(),
            };
            Some(Symbol {
                virtual_range: offset + start..offset + start + symbol.len(),
                span,
                span_range: start..start + symbol.len(),
                width: symbol.width() as u16,
                whitespace: styled.is_whitespace(),
            })
        })
        .collect()
}

fn wrap_symbols(
    symbols: Vec<Symbol>,
    width: u16,
) -> (Vec<SourcePiece>, Vec<usize>, Vec<u16>, Vec<usize>, Vec<u16>) {
    if width == 0 {
        return (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    }

    let mut pieces = Vec::new();
    let mut symbol_ends = Vec::new();
    let mut symbol_widths = Vec::new();
    let mut row_ends = Vec::new();
    let mut row_widths = Vec::new();
    let mut pending_line: Vec<Symbol> = Vec::new();
    let mut pending_word: Vec<Symbol> = Vec::new();
    let mut pending_whitespace: VecDeque<Symbol> = VecDeque::new();
    let mut line_width = 0u16;
    let mut word_width = 0u16;
    let mut whitespace_width = 0u16;
    let mut non_whitespace_previous = false;

    for symbol in symbols {
        if symbol.width > width {
            continue;
        }
        let word_found = non_whitespace_previous && symbol.whitespace;
        let untrimmed_overflow = pending_line.is_empty()
            && word_width
                .saturating_add(whitespace_width)
                .saturating_add(symbol.width)
                > width;
        if word_found || untrimmed_overflow {
            pending_line.extend(pending_whitespace.drain(..));
            line_width = line_width.saturating_add(whitespace_width);
            pending_line.append(&mut pending_word);
            line_width = line_width.saturating_add(word_width);
            whitespace_width = 0;
            word_width = 0;
        }

        let line_full = line_width >= width;
        let pending_word_overflow = symbol.width > 0
            && line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width)
                >= width;
        if line_full || pending_word_overflow {
            let mut remaining_width = width.saturating_sub(line_width);
            append_row(
                mem::take(&mut pending_line),
                line_width,
                &mut pieces,
                &mut symbol_ends,
                &mut symbol_widths,
                &mut row_ends,
                &mut row_widths,
            );
            line_width = 0;
            while let Some(candidate) = pending_whitespace.front() {
                if candidate.width > remaining_width {
                    break;
                }
                whitespace_width = whitespace_width.saturating_sub(candidate.width);
                remaining_width = remaining_width.saturating_sub(candidate.width);
                pending_whitespace.pop_front();
            }
            if symbol.whitespace && pending_whitespace.is_empty() {
                continue;
            }
        }

        non_whitespace_previous = !symbol.whitespace;
        if symbol.whitespace {
            whitespace_width = whitespace_width.saturating_add(symbol.width);
            pending_whitespace.push_back(symbol);
        } else {
            word_width = word_width.saturating_add(symbol.width);
            pending_word.push(symbol);
        }
    }

    pending_line.extend(pending_whitespace);
    pending_line.append(&mut pending_word);
    if !pending_line.is_empty() {
        let final_width = line_width
            .saturating_add(whitespace_width)
            .saturating_add(word_width);
        append_row(
            pending_line,
            final_width,
            &mut pieces,
            &mut symbol_ends,
            &mut symbol_widths,
            &mut row_ends,
            &mut row_widths,
        );
    }
    if row_ends.is_empty() {
        row_ends.push(0);
        row_widths.push(0);
    }
    (pieces, symbol_ends, symbol_widths, row_ends, row_widths)
}

fn append_row(
    symbols: Vec<Symbol>,
    width: u16,
    pieces: &mut Vec<SourcePiece>,
    symbol_ends: &mut Vec<usize>,
    symbol_widths: &mut Vec<u16>,
    row_ends: &mut Vec<usize>,
    row_widths: &mut Vec<u16>,
) {
    let row_start = pieces.len();
    for symbol in symbols {
        let symbol_index = symbol_ends.len();
        symbol_ends.push(symbol.span_range.end);
        symbol_widths.push(symbol.width);
        if pieces.len() > row_start
            && let Some(last) = pieces.last_mut()
            && last.span == symbol.span
            && last.span_range.end == symbol.span_range.start
        {
            last.virtual_range.end = symbol.virtual_range.end;
            last.span_range.end = symbol.span_range.end;
            last.symbols.end = symbol_index + 1;
        } else {
            pieces.push(SourcePiece {
                virtual_range: symbol.virtual_range,
                span: symbol.span,
                span_range: symbol.span_range,
                symbols: symbol_index..symbol_index + 1,
            });
        }
    }
    row_ends.push(pieces.len());
    row_widths.push(width);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Alignment;
    use ratatui::style::Color;
    use ratatui::widgets::{Paragraph, Wrap};

    use super::*;

    fn symbols_from(terminal: &Terminal<TestBackend>) -> Vec<Vec<(String, Style)>> {
        let area = terminal.backend().buffer().area;
        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| {
                        let cell = &terminal.backend().buffer()[(column, row)];
                        (cell.symbol().to_owned(), cell.style())
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn cached_rows_match_paragraph_cells() {
        let prefix_style = Style::default().bold();
        let body_style = Style::default().italic();
        for text in [
            "alpha beta  gamma delta",
            "wide 界 and é Unicode",
            "alpha beta  界 é trailing words alpha beta  界 é trailing words alpha beta  界 é trailing words",
            "averylongunbrokenword",
            "\u{301}combining at the style boundary",
            "  \t\nspaces\u{200b}and\u{a0}nonbreaking\u{200b}space   ",
            "",
        ] {
            for width in [1, 2, 5, 7, 8, 19, 20, 80] {
                let original = Line::from(vec![
                    Span::styled(PREFIX, prefix_style),
                    Span::styled(text, body_style),
                ]);
                let layout = ThoughtLayout::new(Arc::<str>::from(text), width);
                let height = layout.row_count();
                assert_eq!(
                    usize::from(height),
                    Paragraph::new(original.clone())
                        .wrap(Wrap { trim: false })
                        .line_count(width)
                );
                let mut expected = Terminal::new(TestBackend::new(width, height)).unwrap();
                expected
                    .draw(|frame| {
                        frame.render_widget(
                            Paragraph::new(original.clone()).wrap(Wrap { trim: false }),
                            frame.area(),
                        );
                    })
                    .unwrap();
                let mut actual = Terminal::new(TestBackend::new(width, height)).unwrap();
                actual
                    .draw(|frame| {
                        let area = frame.area();
                        layout.render(
                            0..usize::from(height),
                            prefix_style,
                            body_style,
                            area,
                            frame.buffer_mut(),
                        );
                    })
                    .unwrap();
                assert_eq!(
                    symbols_from(&actual),
                    symbols_from(&expected),
                    "{text:?} at {width}"
                );
            }
        }
    }

    #[test]
    fn generic_cached_rows_match_paragraph_for_styles_unicode_and_alignment() {
        let lines = vec![
            Line::from(vec![
                Span::styled("alpha ", Style::default().fg(Color::Red)),
                Span::styled("beta  界 e\u{301}", Style::default().bold()),
                Span::raw("\u{200b} tail\u{a0}space"),
            ])
            .style(Style::default().italic()),
            Line::from(vec![
                Span::styled("  leading whitespace and ", Style::default().underlined()),
                Span::styled("averylongunbrokenword", Style::default().fg(Color::Blue)),
            ])
            .alignment(Alignment::Center),
            Line::from(vec![Span::raw("right aligned words")]).alignment(Alignment::Right),
            Line::default(),
        ];
        let base_style = Style::default().bg(Color::Black);

        for line in lines {
            for width in [1, 2, 5, 9, 19, 40] {
                let layout = WrappedLineLayout::new(&line, width);
                let height = layout.row_count();
                assert_eq!(
                    usize::from(height),
                    Paragraph::new(line.clone())
                        .style(base_style)
                        .wrap(Wrap { trim: false })
                        .line_count(width)
                );

                let mut expected = Terminal::new(TestBackend::new(width, height)).unwrap();
                expected
                    .draw(|frame| {
                        frame.render_widget(
                            Paragraph::new(line.clone())
                                .style(base_style)
                                .wrap(Wrap { trim: false }),
                            frame.area(),
                        );
                    })
                    .unwrap();
                let mut actual = Terminal::new(TestBackend::new(width, height)).unwrap();
                actual
                    .draw(|frame| {
                        layout.render(
                            &line,
                            0..usize::from(height),
                            base_style,
                            frame.area(),
                            frame.buffer_mut(),
                        );
                    })
                    .unwrap();
                assert_eq!(
                    symbols_from(&actual),
                    symbols_from(&expected),
                    "{line:?} at {width}"
                );
            }
        }
    }

    #[test]
    fn generic_cached_rows_paint_only_requested_huge_line_window() {
        let line = Line::from(vec![
            Span::styled("prefix ", Style::default().bold()),
            Span::styled(
                "alpha beta  界 e\u{301} ".repeat(2_000),
                Style::default().italic(),
            ),
        ])
        .alignment(Alignment::Center);
        let width = 23;
        let height = 7;
        let layout = WrappedLineLayout::new(&line, width);
        let max_scroll = layout.row_count().saturating_sub(height);

        for scroll in [0, max_scroll / 2, max_scroll] {
            let mut expected = Terminal::new(TestBackend::new(width, height)).unwrap();
            expected
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new(line.clone())
                            .wrap(Wrap { trim: false })
                            .scroll((scroll, 0)),
                        frame.area(),
                    );
                })
                .unwrap();
            let mut actual = Terminal::new(TestBackend::new(width, height)).unwrap();
            actual
                .draw(|frame| {
                    layout.render(
                        &line,
                        usize::from(scroll)..usize::from(scroll + height),
                        Style::default(),
                        frame.area(),
                        frame.buffer_mut(),
                    );
                })
                .unwrap();
            assert_eq!(
                symbols_from(&actual),
                symbols_from(&expected),
                "scroll {scroll}"
            );
        }
    }

    #[test]
    fn range_runs_do_not_wrap_past_the_transcript_row_limit() {
        let text = "x".repeat(70_000);
        let line = Line::raw(text);
        let layout = WrappedLineLayout::new(&line, 1);
        assert_eq!(layout.row_count(), u16::MAX);

        let ranges = vec![
            (0, 1, "first".to_owned()),
            (69_999, 70_000, "unreachable".to_owned()),
        ];
        let runs = layout.range_runs(&line, &ranges);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].range_index, 0);
        assert_eq!(runs[0].row, 0);
    }
}
