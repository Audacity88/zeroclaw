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
        let mut layout = Self::empty(width, line.alignment.unwrap_or(Alignment::Left));
        let mut wrapper = WrapState::default();
        wrapper.feed(symbols(line), &mut layout);
        wrapper.finish(&mut layout);
        layout
    }

    fn empty(width: u16, alignment: Alignment) -> Self {
        #[cfg(test)]
        static GENERATION: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        Self {
            width,
            pieces: Vec::new(),
            symbol_ends: Vec::new(),
            symbol_widths: Vec::new(),
            row_ends: Vec::new(),
            row_widths: Vec::new(),
            alignment,
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

    pub(crate) fn range_runs(&self, ranges: &[(usize, usize, String)]) -> Vec<WrappedRangeRun> {
        self.range_runs_rows(ranges, 0..self.row_count())
    }

    fn range_runs_rows(
        &self,
        ranges: &[(usize, usize, String)],
        rows: Range<u16>,
    ) -> Vec<WrappedRangeRun> {
        let mut runs: Vec<WrappedRangeRun> = Vec::new();
        // Transcript coordinates are `u16`; never wrap unreachable rows back
        // into the visible range when one pathological line exceeds the cap.
        for row in usize::from(rows.start)..usize::from(rows.end.min(self.row_count())) {
            let start = if row == 0 { 0 } else { self.row_ends[row - 1] };
            let mut column = match self.alignment {
                Alignment::Center => (self.width / 2).saturating_sub(self.row_widths[row] / 2),
                Alignment::Right => self.width.saturating_sub(self.row_widths[row]),
                Alignment::Left => 0,
            };
            for piece in &self.pieces[start..self.row_ends[row]] {
                let mut source = piece.virtual_range.start;
                let mut span_start = piece.span_range.start;
                for symbol in piece.symbols.clone() {
                    let symbol_width = self.symbol_widths[symbol];
                    let symbol_start = source;
                    source += self.symbol_ends[symbol] - span_start;
                    span_start = self.symbol_ends[symbol];
                    if symbol_width == 0 || column >= self.width {
                        continue;
                    }
                    let range_index = ranges.partition_point(|range| range.1 <= symbol_start);
                    if let Some((lo, hi, _)) = ranges.get(range_index)
                        && symbol_start >= *lo
                        && symbol_start < *hi
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
    urls: Vec<(usize, usize, String)>,
    url_runs: Vec<WrappedRangeRun>,
    streaming: Option<StreamingThought>,
}

#[derive(Clone, Debug, Default)]
struct StreamingThought {
    wrapper: WrapState,
    /// State before the provisional EOF tail was painted.
    settled: (usize, usize, usize),
    next_byte: usize,
    text_len: usize,
    url_frontier: usize,
    #[cfg(test)]
    scanned_bytes: usize,
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
            let url_runs = layout.range_runs(&urls);
            (layout, url_runs)
        };
        Self {
            text,
            layout,
            urls,
            url_runs,
            streaming: None,
        }
    }

    pub(crate) fn streaming(width: u16) -> Self {
        let mut layout = WrappedLineLayout::empty(width, Alignment::Left);
        let mut stream = StreamingThought::default();
        stream
            .wrapper
            .feed(symbols(&Line::from(PREFIX)), &mut layout);
        stream.settled = (
            layout.pieces.len(),
            layout.symbol_ends.len(),
            layout.row_ends.len(),
        );
        Self {
            text: Arc::from(""),
            layout,
            urls: Vec::new(),
            url_runs: Vec::new(),
            streaming: Some(stream),
        }
    }

    pub(crate) fn append_streaming(
        &mut self,
        text: &str,
        recognize: fn(&str) -> Vec<(usize, usize, String)>,
    ) {
        let stream = self.streaming.as_mut().expect("streaming layout");
        if stream.text_len == text.len() && !self.layout.row_ends.is_empty() {
            return;
        }
        debug_assert!(text.len() >= stream.text_len);
        let (pieces, symbols_len, rows) = stream.settled;
        self.layout.pieces.truncate(pieces);
        self.layout.symbol_ends.truncate(symbols_len);
        self.layout.symbol_widths.truncate(symbols_len);
        self.layout.row_ends.truncate(rows);
        self.layout.row_widths.truncate(rows);

        // Appending can extend the last grapheme (combining marks, ZWJ, RI).
        // Keep that grapheme outside the committed wrapper state.
        let mut graphemes = text[stream.next_byte..].grapheme_indices(true).peekable();
        let offset = stream.next_byte;
        let mut tail = None;
        while let Some((start, grapheme)) = graphemes.next() {
            #[cfg(test)]
            {
                stream.scanned_bytes += grapheme.len();
            }
            let start = offset + start;
            let symbol = thought_symbol(start, grapheme);
            if graphemes.peek().is_none() {
                stream.next_byte = start;
                tail = symbol;
            } else {
                stream.wrapper.feed(symbol, &mut self.layout);
            }
        }
        stream.settled = (
            self.layout.pieces.len(),
            self.layout.symbol_ends.len(),
            self.layout.row_ends.len(),
        );
        let mut preview = stream.wrapper.clone();
        #[cfg(test)]
        {
            stream.scanned_bytes += preview
                .pending_line
                .iter()
                .chain(&preview.pending_word)
                .chain(&preview.pending_whitespace)
                .map(|symbol| symbol.span_range.len())
                .sum::<usize>();
        }
        preview.feed(tail, &mut self.layout);
        preview.finish(&mut self.layout);

        let frontier = stream.url_frontier;
        #[cfg(test)]
        {
            stream.scanned_bytes += text[frontier..].len();
        }
        self.urls.truncate(
            self.urls
                .partition_point(|url| url.0 < PREFIX.len() + frontier),
        );
        self.urls.extend(
            recognize(&text[frontier..])
                .into_iter()
                .map(|(lo, hi, url)| {
                    (
                        PREFIX.len() + frontier + lo,
                        PREFIX.len() + frontier + hi,
                        url,
                    )
                }),
        );
        stream.url_frontier = text[frontier..]
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map_or(frontier, |(i, ch)| frontier + i + ch.len_utf8());
        stream.text_len = text.len();
    }

    pub(crate) fn streaming_url_runs(&self, rows: Range<u16>) -> Vec<(u16, u16, u16, usize, &str)> {
        self.layout
            .range_runs_rows(&self.urls, rows)
            .into_iter()
            .map(|run| {
                let (start, _, url) = &self.urls[run.range_index];
                (run.row, run.column, run.width, *start, url.as_str())
            })
            .collect()
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

    #[cfg(test)]
    pub(crate) fn generation(&self) -> usize {
        self.layout.generation()
    }

    #[cfg(test)]
    pub(crate) fn scanned_bytes(&self) -> usize {
        self.streaming
            .as_ref()
            .map_or(0, |stream| stream.scanned_bytes)
    }

    pub(crate) fn render(
        &self,
        rows: Range<usize>,
        prefix_style: Style,
        body_style: Style,
        area: Rect,
        buffer: &mut Buffer,
    ) {
        self.render_text(&self.text, rows, prefix_style, body_style, area, buffer);
    }

    pub(crate) fn render_text(
        &self,
        text: &str,
        rows: Range<usize>,
        prefix_style: Style,
        body_style: Style,
        area: Rect,
        buffer: &mut Buffer,
    ) {
        let line = Line::from(vec![
            Span::styled(PREFIX, prefix_style),
            Span::styled(text, body_style),
        ]);
        self.layout
            .render(&line, rows, Style::default(), area, buffer);
    }
}

fn thought_symbol(start: usize, grapheme: &str) -> Option<Symbol> {
    if grapheme.contains(char::is_control) {
        return None;
    }
    Some(Symbol {
        virtual_range: PREFIX.len() + start..PREFIX.len() + start + grapheme.len(),
        span: 1,
        span_range: start..start + grapheme.len(),
        width: grapheme.width() as u16,
        whitespace: StyledGrapheme {
            symbol: grapheme,
            style: Style::default(),
        }
        .is_whitespace(),
    })
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

#[derive(Clone, Debug, Default)]
struct WrapState {
    pending_line: Vec<Symbol>,
    pending_word: Vec<Symbol>,
    pending_whitespace: VecDeque<Symbol>,
    line_width: u16,
    word_width: u16,
    whitespace_width: u16,
    non_whitespace_previous: bool,
}

impl WrapState {
    fn feed(&mut self, symbols: impl IntoIterator<Item = Symbol>, layout: &mut WrappedLineLayout) {
        let width = layout.width;
        if width == 0 {
            return;
        }

        for symbol in symbols {
            if symbol.width > width {
                continue;
            }
            let word_found = self.non_whitespace_previous && symbol.whitespace;
            let untrimmed_overflow = self.pending_line.is_empty()
                && self
                    .word_width
                    .saturating_add(self.whitespace_width)
                    .saturating_add(symbol.width)
                    > width;
            if word_found || untrimmed_overflow {
                self.pending_line.extend(self.pending_whitespace.drain(..));
                self.line_width = self.line_width.saturating_add(self.whitespace_width);
                self.pending_line.append(&mut self.pending_word);
                self.line_width = self.line_width.saturating_add(self.word_width);
                self.whitespace_width = 0;
                self.word_width = 0;
            }

            let line_full = self.line_width >= width;
            let pending_word_overflow = symbol.width > 0
                && self
                    .line_width
                    .saturating_add(self.whitespace_width)
                    .saturating_add(self.word_width)
                    >= width;
            if line_full || pending_word_overflow {
                let mut remaining_width = width.saturating_sub(self.line_width);
                append_row(
                    mem::take(&mut self.pending_line),
                    self.line_width,
                    &mut layout.pieces,
                    &mut layout.symbol_ends,
                    &mut layout.symbol_widths,
                    &mut layout.row_ends,
                    &mut layout.row_widths,
                );
                self.line_width = 0;
                while let Some(candidate) = self.pending_whitespace.front() {
                    if candidate.width > remaining_width {
                        break;
                    }
                    self.whitespace_width = self.whitespace_width.saturating_sub(candidate.width);
                    remaining_width = remaining_width.saturating_sub(candidate.width);
                    self.pending_whitespace.pop_front();
                }
                if symbol.whitespace && self.pending_whitespace.is_empty() {
                    continue;
                }
            }

            self.non_whitespace_previous = !symbol.whitespace;
            if symbol.whitespace {
                self.whitespace_width = self.whitespace_width.saturating_add(symbol.width);
                self.pending_whitespace.push_back(symbol);
            } else {
                self.word_width = self.word_width.saturating_add(symbol.width);
                self.pending_word.push(symbol);
            }
        }
    }

    fn finish(mut self, layout: &mut WrappedLineLayout) {
        if layout.width == 0 {
            return;
        }
        self.pending_line.extend(self.pending_whitespace);
        self.pending_line.append(&mut self.pending_word);
        if !self.pending_line.is_empty() {
            let final_width = self
                .line_width
                .saturating_add(self.whitespace_width)
                .saturating_add(self.word_width);
            append_row(
                self.pending_line,
                final_width,
                &mut layout.pieces,
                &mut layout.symbol_ends,
                &mut layout.symbol_widths,
                &mut layout.row_ends,
                &mut layout.row_widths,
            );
        }
        if layout.row_ends.is_empty() {
            layout.row_ends.push(0);
            layout.row_widths.push(0);
        }
    }
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
        let runs = layout.range_runs(&ranges);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].range_index, 0);
        assert_eq!(runs[0].row, 0);
    }
}
