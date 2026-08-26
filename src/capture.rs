//! Full-output capture of a terminal session.
//!
//! `rows` and `cols` describe the PTY *viewport*: they decide where lines soft
//! wrap, what a full-screen program is told the terminal size is, and how the
//! shell draws its prompt. They deliberately do **not** decide how much of the
//! output a screenshot shows. A command that prints 200 lines into a 40-row
//! viewport scrolls 160 rows out of view, and those rows are usually exactly
//! the ones the screenshot was wanted for.
//!
//! So the terminal parser is given a real scrollback buffer, and
//! [`CapturedScreen`] snapshots *every physical row it retained* - scrollback
//! first, then the rows still on screen - as one flat, uniformly indexed grid.
//! Everything downstream (rendering, width/height trimming, redaction, ANSI
//! text, PNG `Description` metadata) works on that single view, so there is no
//! separate "visible screen" and "scrollback" code path anywhere, and cell
//! contents, styles, soft-wrap flags and wide-character continuation cells all
//! mean the same thing in row 3 of the scrollback as they do in the last row on
//! screen.
//!
//! A full-screen (alternate-screen) program is the one exception: `vim`, `htop`
//! and friends paint a self-contained screen, and whatever scrolled past before
//! they started is not part of what they are showing. For those, only the
//! active screen is captured.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use vt100::{Cell, Parser, Screen};

/// Default number of scrolled-off lines retained per capture. High enough that
/// ordinary command output is kept whole, bounded so a runaway command cannot
/// grow the buffer without limit.
pub const DEFAULT_MAX_SCROLLBACK_LINES: usize = 10_000;

/// Hard ceiling on the configured scrollback capacity. Cell coordinates are
/// `u16` throughout (redaction maps, renderer loops), so the retained rows plus
/// the viewport must stay addressable; this leaves ample room for the largest
/// allowed viewport.
pub const MAX_SCROLLBACK_LIMIT: usize = 60_000;

/// Peak number of terminal cells a single capture may retain.
///
/// A line limit alone does not bound memory: 60,000 lines of a 500-column
/// terminal is 30 million cells, and a [`vt100::Cell`] is 32 bytes, so the
/// capture alone would be nearly a gigabyte - before the parser's own copy of
/// the same rows, or the pixels they render to. Counting cells instead makes
/// the bound independent of the terminal's width.
///
/// 2,000,000 cells is ~64 MB in the capture and about as much again in the
/// parser that fed it. At the default 120 columns that is still more than
/// 16,000 lines of retained output, comfortably above the default scrollback.
pub const MAX_RETAINED_CELLS: usize = 2_000_000;

/// How many scrolled-off lines a `rows` x `cols` capture can retain without
/// exceeding [`MAX_RETAINED_CELLS`], given that the viewport itself is
/// retained too.
///
/// Returns the smaller of the caller's `configured` limit, the hard
/// [`MAX_SCROLLBACK_LIMIT`], and what the cell budget allows - so the result is
/// never larger than what was asked for, and a caller can compare the two to
/// tell the user its configuration was capped.
pub fn effective_scrollback_lines(rows: u16, cols: u16, configured: usize) -> usize {
    let cols = usize::from(cols).max(1);
    let budget_rows = MAX_RETAINED_CELLS / cols;
    let room = budget_rows.saturating_sub(usize::from(rows));
    configured.min(MAX_SCROLLBACK_LIMIT).min(room)
}

/// How many rows a head capture may stage while it streams the beginning of a
/// session out of the terminal.
///
/// Deliberately **independent of the configured scrollback capacity**. That
/// setting says how much of the *end* of a session to keep once the terminal
/// starts evicting rows, which is the opposite of what a head selection wants:
/// selecting the head out of a tail buffer returns whatever survived eviction,
/// not the beginning of the output. So the staging area is bounded only by the
/// global cell budget, and `--head-lines 10` answers with lines 1..10 whether
/// the capacity is one line or ten thousand.
pub fn head_staging_lines(rows: u16, cols: u16) -> usize {
    effective_scrollback_lines(rows, cols, MAX_SCROLLBACK_LIMIT)
}

/// Which of the retained lines a screenshot should show.
///
/// Selection counts *logical* lines: a line the terminal soft-wrapped across
/// several physical rows counts once and is never cut in half. Trailing blank
/// rows below the last content are not lines, so `Tail(10)` shows the last ten
/// lines of output rather than ten empty rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineSelection {
    /// Every retained line: all scrollback plus the current screen.
    #[default]
    All,
    /// The first N logical lines of the capture.
    Head(usize),
    /// The last N logical lines of the capture.
    Tail(usize),
}

impl LineSelection {
    /// Build a selection from a mutually exclusive `head`/`tail` pair, as the
    /// CLI flags and MCP parameters supply it.
    pub fn from_head_tail(head: Option<usize>, tail: Option<usize>) -> Result<Self> {
        match (head, tail) {
            (Some(_), Some(_)) => anyhow::bail!(
                "head_lines and tail_lines are mutually exclusive: pass only one of them"
            ),
            (Some(0), None) | (None, Some(0)) => {
                anyhow::bail!("head_lines/tail_lines must be at least 1")
            }
            (Some(n), None) => Ok(Self::Head(n)),
            (None, Some(n)) => Ok(Self::Tail(n)),
            (None, None) => Ok(Self::All),
        }
    }

    /// True when nothing is being dropped, i.e. the whole capture is shown.
    pub fn is_all(self) -> bool {
        self == Self::All
    }
}

/// One physical row of a capture: its cells (cloned from the terminal buffer,
/// so styles, wide characters and their continuation cells are preserved
/// exactly) and the terminal's soft-wrap flag, which says the row's text
/// continues on the next one.
#[derive(Debug, Clone)]
pub struct CapturedRow {
    cells: Vec<Cell>,
    wrapped: bool,
}

/// An empty cell. [`vt100::Cell`] has no public constructor, so one is cloned
/// out of a fresh (blank) terminal.
fn blank_cell() -> Cell {
    Parser::new(1, 1, 0)
        .screen()
        .cell(0, 0)
        .expect("a 1x1 screen has a cell at the origin")
        .clone()
}

impl CapturedRow {
    fn from_screen(screen: &Screen, row: u16, cols: u16) -> Self {
        let cells = (0..cols)
            .map(|col| screen.cell(row, col).cloned().unwrap_or_else(blank_cell))
            .collect();
        Self {
            cells,
            wrapped: screen.row_wrapped(row),
        }
    }

    fn blank(cols: u16) -> Self {
        Self {
            cells: vec![blank_cell(); usize::from(cols)],
            wrapped: false,
        }
    }

    /// The cell at `col`, or `None` past the end of the row.
    pub fn cell(&self, col: u16) -> Option<&Cell> {
        self.cells.get(col as usize)
    }

    /// Whether this row's logical line continues on the next row.
    pub fn wrapped(&self) -> bool {
        self.wrapped
    }

    /// The row's text over `[start, start + width)`, rendered the way
    /// `vt100::Screen::rows` does it: a wide character's continuation cell is
    /// skipped, gaps between filled cells become spaces, and trailing blanks
    /// are dropped.
    pub fn text(&self, start: u16, width: u16) -> String {
        let mut out = String::new();
        let end = usize::from(start)
            .saturating_add(usize::from(width))
            .min(self.cells.len());
        let mut prev_col = usize::from(start);
        let mut prev_was_wide = false;
        for col in usize::from(start)..end {
            let cell = &self.cells[col];
            if prev_was_wide {
                prev_was_wide = false;
                continue;
            }
            prev_was_wide = cell.is_wide();
            if cell.has_contents() {
                for _ in prev_col..col {
                    out.push(' ');
                }
                prev_col = col + if cell.is_wide() { 2 } else { 1 };
                out.push_str(cell.contents());
            }
        }
        out
    }

    /// Whether this row holds anything worth showing: visible text, or a styled
    /// cell such as a colored background block. Used to find where the output
    /// really ends, so line selection and height trimming never count the blank
    /// rows below it.
    pub fn has_content(&self) -> bool {
        self.cells.iter().any(cell_has_content)
    }
}

/// Whether a single cell holds anything worth showing. Shared by row-level
/// content detection and the renderer's width trimming so both agree.
pub fn cell_has_content(cell: &Cell) -> bool {
    let contents = cell.contents();
    let has_text = !contents.is_empty() && contents != " ";
    has_text || cell.bgcolor() != vt100::Color::Default || cell.inverse()
}

/// The grid API the redaction pass reads a terminal through.
///
/// Implemented by both [`vt100::Screen`] and [`CapturedScreen`], so a caller
/// that already has a parsed screen - as everything did before captures gained
/// scrollback - can still hand it straight to
/// [`crate::redaction::RedactionEngine::redact_screen`].
pub trait ScreenView {
    /// `(rows, cols)` of the grid.
    fn size(&self) -> (u16, u16);
    /// The cell at `(row, col)`, or `None` outside the grid.
    fn cell(&self, row: u16, col: u16) -> Option<&Cell>;
    /// Whether the text in `row` soft-wraps into the next row.
    fn row_wrapped(&self, row: u16) -> bool;
}

impl ScreenView for Screen {
    fn size(&self) -> (u16, u16) {
        Screen::size(self)
    }

    fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        Screen::cell(self, row, col)
    }

    fn row_wrapped(&self, row: u16) -> bool {
        Screen::row_wrapped(self, row)
    }
}

impl ScreenView for CapturedScreen {
    fn size(&self) -> (u16, u16) {
        CapturedScreen::size(self)
    }

    fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        CapturedScreen::cell(self, row, col)
    }

    fn row_wrapped(&self, row: u16) -> bool {
        CapturedScreen::row_wrapped(self, row)
    }
}

/// Every physical row a terminal session retained, as one flat grid.
///
/// Row 0 is the oldest retained line (the top of the scrollback, or the top of
/// the screen when nothing scrolled off); the last rows are the ones still on
/// screen. The accessors mirror [`vt100::Screen`] so the rest of the pipeline
/// reads a capture exactly the way it used to read a single screenful.
#[derive(Debug, Clone)]
pub struct CapturedScreen {
    rows: Vec<CapturedRow>,
    cols: u16,
    cursor: (u16, u16),
    alternate: bool,
    /// Number of scrolled-off rows this capture retained.
    scrollback_rows: usize,
    /// True when output was dropped before it could be captured, because more
    /// lines scrolled off than the configured capacity could hold.
    truncated: bool,
}

impl CapturedScreen {
    /// Parse `data` in a viewport of `rows` x `cols` that retains up to
    /// `max_scrollback_lines` scrolled-off lines, and capture everything it
    /// kept.
    pub fn parse(data: &[u8], rows: u16, cols: u16, max_scrollback_lines: usize) -> Self {
        Self::parse_selected(data, rows, cols, max_scrollback_lines, LineSelection::All)
    }

    /// Parse `data` and capture only the lines `selection` asks for.
    ///
    /// The selection is applied *during* capture rather than afterwards, which
    /// matters twice over:
    ///
    /// * `Head(n)` is collected as the first `n` lines leave the viewport, so
    ///   it is the true beginning of the output even when the session went on
    ///   to print a hundred times more than the scrollback can hold. Selecting
    ///   the head of an already-evicted tail buffer would silently return
    ///   whatever survived instead. Because of that, a head capture ignores
    ///   `max_scrollback_lines` - a *tail*-retention setting - and streams into
    ///   a staging buffer sized by the global cell budget alone
    ///   ([`head_staging_lines`]), so it works at any configured capacity, down
    ///   to a single line.
    /// * `Tail(n)` never clones the rows it is about to discard, so a long
    ///   capture costs one selection's worth of memory rather than a whole
    ///   scrollback's.
    pub fn parse_selected(
        data: &[u8],
        rows: u16,
        cols: u16,
        max_scrollback_lines: usize,
        selection: LineSelection,
    ) -> Self {
        if let LineSelection::Head(n) = selection {
            let staging = head_staging_lines(rows, cols);
            let mut parser = Parser::new(rows, cols, staging.saturating_add(1));
            return match Self::capture_head(&mut parser, data, n, cols, staging) {
                Some(head) => head,
                // The head could not be streamed, which only happens when
                // nothing was ever evicted from the staging buffer (output
                // shorter than the viewport, or than `n` lines) or when the
                // session ended in a full-screen program. Either way the rows
                // still in the parser really are the beginning of the output,
                // so selecting from them is safe.
                None => Self::from_parser_selected(&mut parser, staging, selection),
            };
        }

        let capacity = effective_scrollback_lines(rows, cols, max_scrollback_lines);
        // One row of headroom past the configured capacity: if the parser ever
        // holds `capacity + 1` scrolled-off rows then output was definitely
        // evicted, and if it holds exactly `capacity` then nothing was. That
        // sentinel is exact where "the buffer looks full" is only a guess - and
        // it stays exact for a capacity of zero.
        let mut parser = Parser::new(rows, cols, capacity.saturating_add(1));
        parser.process(data);
        Self::from_parser_selected(&mut parser, capacity, selection)
    }

    /// Capture everything a parser retained.
    ///
    /// The scrollback is read through [`vt100::Screen::set_scrollback`], one
    /// row at a time, rather than by re-parsing the bytes into a taller
    /// terminal: a taller viewport changes cursor addressing, scroll regions
    /// and full-screen redraws, so it would not show the same session.
    ///
    /// `capacity` is the configured scrollback length; the parser is expected
    /// to have been built with one row more than that (see
    /// [`Self::parse_selected`]) so overflow can be detected exactly.
    pub fn from_parser(parser: &mut Parser, capacity: usize) -> Self {
        Self::from_parser_selected(parser, capacity, LineSelection::All)
    }

    /// [`Self::from_parser`], cloning only the rows `selection` keeps.
    fn from_parser_selected(
        parser: &mut Parser,
        capacity: usize,
        selection: LineSelection,
    ) -> Self {
        let (rows, cols) = parser.screen().size();
        let cursor = parser.screen().cursor_position();

        // A full-screen program owns the whole viewport and repaints it; the
        // lines that scrolled past before it started are not part of what it is
        // showing, so only the active screen is captured.
        if parser.screen().alternate_screen() {
            let mut captured = Self::from_visible(parser.screen());
            captured.alternate = true;
            return captured.select(selection);
        }

        // Clamped to the buffer's real length, so this reports how many rows
        // actually scrolled off (up to the sentinel row past `capacity`).
        parser.screen_mut().set_scrollback(usize::MAX);
        let scrolled = parser.screen().scrollback();
        parser.screen_mut().set_scrollback(0);

        // More rows scrolled off than the configured capacity could hold, so
        // the oldest output is gone. Exactly `capacity` rows is a full buffer
        // that lost nothing.
        let truncated = scrolled > capacity;
        let scrollback_rows = scrolled.min(capacity);
        // Absolute index of the oldest row this capture keeps: the sentinel
        // row, when there is one, is not part of the configured capacity.
        let base = scrolled - scrollback_rows;
        let total = scrollback_rows + usize::from(rows);

        let range = Self::selected_range(parser, base, scrolled, total, cols, selection);
        let mut captured: Vec<CapturedRow> = range
            .clone()
            .map(|idx| row_at(parser, scrolled, base + idx, cols))
            .collect();
        parser.screen_mut().set_scrollback(0);
        // An empty selection would render as a zero-height image; keep one
        // blank row so the output is a small empty tile instead.
        if captured.is_empty() {
            captured.push(CapturedRow::blank(cols));
        }

        // The cursor sits on the current screen, which follows the scrollback
        // in the flat grid, and then moves with the rows the selection kept.
        let cursor_row = scrollback_rows
            .saturating_add(usize::from(cursor.0))
            .saturating_sub(range.start)
            .min(captured.len() - 1);

        Self {
            rows: captured,
            cols,
            cursor: (u16::try_from(cursor_row).unwrap_or(u16::MAX), cursor.1),
            alternate: false,
            scrollback_rows,
            truncated,
        }
    }

    /// The half-open range of retained row indices `selection` keeps, computed
    /// by reading the parser's rows *without* cloning their cells.
    fn selected_range(
        parser: &mut Parser,
        base: usize,
        scrolled: usize,
        total: usize,
        cols: u16,
        selection: LineSelection,
    ) -> std::ops::Range<usize> {
        let count = match selection {
            LineSelection::All => return 0..total,
            LineSelection::Head(n) | LineSelection::Tail(n) => n,
        };

        // Trailing blank rows below the output are not lines, so a tail
        // selection must not count them and a head selection must not run past
        // them.
        let mut end = 0;
        for idx in (0..total).rev() {
            if row_has_content_at(parser, scrolled, base + idx, cols) {
                end = idx + 1;
                break;
            }
        }

        // Row `idx` starts a logical line unless the row above it soft-wrapped
        // into it.
        let is_start = |parser: &mut Parser, idx: usize| {
            idx == 0 || !row_wrapped_at(parser, scrolled, base + idx - 1)
        };

        match selection {
            LineSelection::All => unreachable!("returned above"),
            LineSelection::Head(_) => {
                let mut starts = 0;
                for idx in 0..end {
                    if is_start(parser, idx) {
                        starts += 1;
                        if starts == count + 1 {
                            return 0..idx;
                        }
                    }
                }
                0..end
            }
            LineSelection::Tail(_) => {
                let mut starts = 0;
                for idx in (0..end).rev() {
                    if is_start(parser, idx) {
                        starts += 1;
                        if starts == count {
                            return idx..end;
                        }
                    }
                }
                0..end
            }
        }
    }

    /// Collect the first `n` logical lines as they scroll out of the viewport.
    ///
    /// The terminal's scrollback is a *tail* buffer: once it fills, the oldest
    /// rows are evicted, so by the end of a long session the beginning of the
    /// output is simply gone. Head selection therefore cannot be a filter
    /// applied to the finished capture - it has to snapshot the rows on their
    /// way past. `data` is fed in chunks small enough that no un-harvested row
    /// can be evicted between checks, and parsing stops as soon as line `n + 1`
    /// has been seen, so `--head-lines 10` on a hundred thousand lines of
    /// output costs ten lines of work.
    ///
    /// `room` is the staging capacity from [`head_staging_lines`], not the
    /// configured scrollback: the beginning of the output is streamed the same
    /// way whether the user asked to retain one line or sixty thousand.
    ///
    /// Returns `None` - having processed all of `data`, so the parser is ready
    /// for a normal capture - when the head cannot be settled this way: output
    /// shorter than `n` lines, a session that never leaves the alternate
    /// screen, or a viewport so large that the cell budget cannot stage even
    /// one screenful. A session that *does* start a full-screen program, after
    /// printing enough to settle the head, keeps the head it printed: those
    /// first lines are the beginning of the output, and the screen the program
    /// is painting is not. That holds even when the last of those lines and the
    /// program's startup sequence arrive together, because feeding stops at the
    /// entry sequence itself (see [`next_alt_screen_entry`]) rather than at an
    /// arbitrary chunk boundary.
    fn capture_head(
        parser: &mut Parser,
        data: &[u8],
        n: usize,
        cols: u16,
        room: usize,
    ) -> Option<Self> {
        let viewport = usize::from(parser.screen().size().0).max(1);

        let mut head: Vec<CapturedRow> = Vec::new();
        let mut starts = 0usize;
        let mut end = None;
        let mut pos = 0;
        let mut budget_spent = false;

        while pos < data.len() && end.is_none() {
            let scrolled = scrollback_len(parser);
            let free = room.saturating_sub(scrolled);
            if free <= viewport {
                budget_spent = true;
                break;
            }
            // vt100 scrolls at most one viewport per escape sequence, and the
            // shortest sequence that scrolls anything is three bytes, so this
            // many bytes cannot push more than `free` rows out of the buffer -
            // nothing harvested below can be evicted before the next check.
            let chunk = (free / viewport).clamp(1, MAX_HEAD_CHUNK_BYTES);
            let mut chunk = chunk.min(data.len() - pos);

            // Entering the alternate screen swaps the active grid: the normal
            // screen's rows stop being readable and its scrollback stops
            // growing, so any normal output still sitting in the viewport - a
            // short prefix that never scrolled, typically - would be lost the
            // moment the entry sequence is fed. Feeding therefore stops *at*
            // the sequence, and the head is settled from the normal screen
            // while it is still the one in front. The scan runs over the whole
            // remaining slice, so a sequence that would have straddled a chunk
            // boundary is still found whole.
            match next_alt_screen_entry(&data[pos..], chunk) {
                Some((0, seq_end)) => {
                    if !parser.screen().alternate_screen()
                        && let Some(settled) = Self::settled_head(parser, n, room)
                    {
                        return Some(settled);
                    }
                    // The head is not complete yet, so the full-screen program
                    // is what the capture will show. Feed only the entry
                    // sequence, so a program that leaves the alternate screen
                    // again inside this chunk gets the same treatment at its
                    // next entry.
                    chunk = seq_end;
                }
                Some((start, _)) => chunk = start,
                None => {}
            }

            parser.process(&data[pos..pos + chunk]);
            pos += chunk;

            let scrolled = scrollback_len(parser);
            if scrolled > room {
                // Belt and braces: rows may have been evicted, so this pass can
                // no longer promise the true beginning.
                budget_spent = true;
                break;
            }
            let first_new = head.len();
            for idx in first_new..scrolled {
                head.push(read_scrollback_row(parser, scrolled, idx, cols));
            }
            parser.screen_mut().set_scrollback(0);

            for idx in first_new..head.len() {
                if idx == 0 || !head[idx - 1].wrapped() {
                    starts += 1;
                    if starts == n + 1 {
                        end = Some(idx);
                        break;
                    }
                }
            }
        }

        // A full-screen program repaints the viewport, and what scrolled past
        // before it started is not what it is showing; leave those captures to
        // the normal path.
        if parser.screen().alternate_screen() || head.is_empty() {
            parser.process(&data[pos..]);
            return None;
        }
        match end {
            Some(end) => {
                head.truncate(end);
                Some(Self::from_head_rows(head, cols, false))
            }
            // The first `n` lines do not fit in the retained-cell budget. What
            // was harvested is still the true beginning, so keep it and report
            // that the rest was dropped.
            None if budget_spent => Some(Self::from_head_rows(head, cols, true)),
            None => {
                parser.process(&data[pos..]);
                None
            }
        }
    }

    /// The first `n` logical lines of the parser's *normal* screen, if it
    /// already holds that many.
    ///
    /// Called at an alternate-screen entry, where the rows in front are the
    /// true beginning of the output and are about to become unreadable. When
    /// the head is complete here it is the answer, whatever the full-screen
    /// program goes on to paint; when it is not - a program that started before
    /// `n` lines had been printed - `None` leaves the capture to the ordinary
    /// alternate-screen path, which shows the active screen.
    ///
    /// Unlike the streaming harvest this reads the viewport as well as the
    /// scrollback, so it settles a short prefix that never scrolled at all.
    fn settled_head(parser: &mut Parser, n: usize, room: usize) -> Option<Self> {
        let head = Self::from_parser_selected(parser, room, LineSelection::Head(n));
        (head.logical_lines() >= n).then_some(head)
    }

    /// How many logical lines this capture holds, ignoring the blank rows
    /// below the output.
    fn logical_lines(&self) -> usize {
        self.logical_line_starts(self.content_end()).len()
    }

    /// Wrap harvested head rows in a capture. The cursor sits on the last row
    /// kept: everything after it is output the selection deliberately drops.
    fn from_head_rows(mut rows: Vec<CapturedRow>, cols: u16, truncated: bool) -> Self {
        if rows.is_empty() {
            rows.push(CapturedRow::blank(cols));
        }
        let scrollback_rows = rows.len();
        let last = u16::try_from(rows.len() - 1).unwrap_or(u16::MAX);
        Self {
            rows,
            cols,
            cursor: (last, 0),
            alternate: false,
            scrollback_rows,
            truncated,
        }
    }

    /// Capture only what is currently on screen, ignoring any scrollback.
    pub fn from_visible(screen: &Screen) -> Self {
        let (rows, cols) = screen.size();
        Self {
            rows: (0..rows)
                .map(|row| CapturedRow::from_screen(screen, row, cols))
                .collect(),
            cols,
            cursor: screen.cursor_position(),
            alternate: screen.alternate_screen(),
            scrollback_rows: 0,
            truncated: false,
        }
    }

    /// `(rows, cols)` of the capture. `rows` counts every retained row, so it
    /// is normally larger than the PTY viewport height.
    pub fn size(&self) -> (u16, u16) {
        (
            u16::try_from(self.rows.len()).unwrap_or(u16::MAX),
            self.cols,
        )
    }

    /// The cell at `(row, col)`, or `None` outside the capture.
    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        self.rows.get(usize::from(row)).and_then(|r| r.cell(col))
    }

    /// Whether the text in `row` soft-wraps into the next row.
    pub fn row_wrapped(&self, row: u16) -> bool {
        self.rows
            .get(usize::from(row))
            .is_some_and(CapturedRow::wrapped)
    }

    /// The rows of the capture as text, restricted to `[start, start + width)`
    /// columns - the capture's counterpart of [`vt100::Screen::rows`].
    pub fn rows(&self, start: u16, width: u16) -> impl Iterator<Item = String> + '_ {
        self.rows.iter().map(move |row| row.text(start, width))
    }

    /// Cursor position in capture coordinates.
    pub fn cursor_position(&self) -> (u16, u16) {
        self.cursor
    }

    /// Whether this capture came from a full-screen (alternate-screen) program,
    /// in which case it holds only the active screen.
    pub fn alternate_screen(&self) -> bool {
        self.alternate
    }

    /// How many scrolled-off rows this capture retained.
    pub fn scrollback_rows(&self) -> usize {
        self.scrollback_rows
    }

    /// True when output was lost: more lines scrolled off than the configured
    /// scrollback could hold, so the oldest ones were dropped before they could
    /// be captured.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// One past the last row holding content, ignoring the blank rows below the
    /// output.
    fn content_end(&self) -> usize {
        self.rows
            .iter()
            .rposition(CapturedRow::has_content)
            .map_or(0, |idx| idx + 1)
    }

    /// Row index of the first physical row of each logical line in
    /// `[0, end)`, i.e. every row that is not the continuation of a soft wrap.
    fn logical_line_starts(&self, end: usize) -> Vec<usize> {
        (0..end)
            .filter(|&idx| idx == 0 || !self.rows[idx - 1].wrapped())
            .collect()
    }

    /// Narrow an already-built capture to the requested lines, keeping whole
    /// logical lines so a soft-wrapped line is never cut in half.
    ///
    /// [`Self::parse_selected`] is the cheaper route for a fresh capture - it
    /// never builds the rows this would throw away, and its `Head` is the real
    /// beginning of the output rather than the beginning of what survived
    /// scrollback eviction. This remains for narrowing a capture in hand.
    pub fn select(mut self, selection: LineSelection) -> Self {
        let count = match selection {
            LineSelection::All => return self,
            LineSelection::Head(n) | LineSelection::Tail(n) => n,
        };

        let end = self.content_end();
        let starts = self.logical_line_starts(end);
        let range = match selection {
            LineSelection::All => unreachable!("returned above"),
            LineSelection::Head(_) => 0..starts.get(count).copied().unwrap_or(end),
            LineSelection::Tail(_) => {
                let first = starts.len().saturating_sub(count);
                starts.get(first).copied().unwrap_or(0)..end
            }
        };

        // Trimmed in place: `drain(..).collect()` would hold a second copy of
        // the kept rows while the originals were still alive.
        self.rows.truncate(range.end);
        self.rows.drain(..range.start);
        // An empty selection would render as a zero-height image; keep one
        // blank row so the output is a small empty tile instead.
        if self.rows.is_empty() {
            self.rows.push(CapturedRow::blank(self.cols));
        }
        let (rows, _) = self.size();
        self.cursor.0 = self
            .cursor
            .0
            .saturating_sub(u16::try_from(range.start).unwrap_or(u16::MAX))
            .min(rows.saturating_sub(1));
        self
    }
}

/// Largest chunk of input fed to the parser in one step while harvesting the
/// head of a capture. Small enough to keep the per-chunk row growth checkable,
/// large enough that the pass is not dominated by call overhead.
const MAX_HEAD_CHUNK_BYTES: usize = 64 * 1024;

/// DEC private mode numbers that switch a terminal to the alternate screen.
///
/// `47` is the original xterm mode, `1047` the same with an implicit clear on
/// exit, and `1049` the modern one that also saves the cursor. They are the
/// sequences a full-screen program emits on startup.
const ALT_SCREEN_MODES: [u32; 3] = [47, 1047, 1049];

/// Byte range of the first alternate-screen entry sequence *starting* within
/// `data[..limit]`, as `(start, end)` with `end` exclusive.
///
/// Only real escape sequences match: the scan looks for `ESC [`, then reads the
/// CSI parameter, intermediate and final bytes the way a terminal parser does,
/// so literal text that merely mentions `?1049h` is left alone. A sequence may
/// run past `limit` (`limit` bounds where it may *begin*, not where it may
/// end), so a sequence split across chunk boundaries is still recognised whole,
/// as long as the caller passes the rest of the buffer rather than one chunk.
///
/// Only the 7-bit `ESC [` form is recognised, because that is the only form
/// [`vt100`] acts on; treating a bare `0x9B` as an 8-bit CSI would misread a
/// UTF-8 continuation byte as the start of a mode switch.
fn next_alt_screen_entry(data: &[u8], limit: usize) -> Option<(usize, usize)> {
    let limit = limit.min(data.len());
    for start in 0..limit {
        if data[start] != 0x1b || data.get(start + 1) != Some(&b'[') {
            continue;
        }
        if let Some(end) = alt_screen_entry_at(data, start + 2) {
            return Some((start, end));
        }
    }
    None
}

/// End offset of the CSI sequence whose parameter bytes begin at `from`, if it
/// is an alternate-screen entry (`CSI ? <mode> h`).
fn alt_screen_entry_at(data: &[u8], from: usize) -> Option<usize> {
    let mut idx = from;
    while data.get(idx).is_some_and(|b| (0x30..=0x3f).contains(b)) {
        idx += 1;
    }
    let params = &data[from..idx];
    // Intermediate bytes, which no alternate-screen sequence uses but which a
    // terminal would still skip before the final byte.
    while data.get(idx).is_some_and(|b| (0x20..=0x2f).contains(b)) {
        idx += 1;
    }
    // A sequence the buffer cut short never took effect.
    let final_byte = *data.get(idx)?;
    if final_byte != b'h' {
        return None;
    }
    // `h` is DECSET only with the `?` private prefix; without it the same final
    // byte is SM, which has nothing to do with the alternate screen.
    let modes = params.strip_prefix(b"?")?;
    modes
        .split(|&b| b == b';')
        .filter_map(csi_param)
        .any(|mode| ALT_SCREEN_MODES.contains(&mode))
        .then_some(idx + 1)
}

/// One CSI parameter as a number, ignoring any `:`-separated sub-parameters.
fn csi_param(param: &[u8]) -> Option<u32> {
    let digits = param.split(|&b| b == b':').next()?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    // Saturating, so an absurdly long run of digits is simply not one of the
    // modes above rather than a parse failure.
    digits
        .iter()
        .try_fold(0u32, |acc, &b| {
            acc.checked_mul(10)?.checked_add(u32::from(b - b'0'))
        })
        .or(Some(u32::MAX))
}

/// Number of rows currently in the parser's scrollback, leaving the view where
/// it found it.
fn scrollback_len(parser: &mut Parser) -> usize {
    parser.screen_mut().set_scrollback(usize::MAX);
    let len = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(0);
    len
}

/// Scroll the parser's view so that absolute row `abs` - counted from the
/// oldest row still in the scrollback, with the visible screen following it -
/// is reachable, and return where it landed.
fn view_row(parser: &mut Parser, scrolled: usize, abs: usize) -> u16 {
    if abs < scrolled {
        parser.screen_mut().set_scrollback(scrolled - abs);
        0
    } else {
        parser.screen_mut().set_scrollback(0);
        u16::try_from(abs - scrolled).unwrap_or(u16::MAX)
    }
}

/// Clone absolute row `abs` out of the parser.
fn row_at(parser: &mut Parser, scrolled: usize, abs: usize, cols: u16) -> CapturedRow {
    let row = view_row(parser, scrolled, abs);
    CapturedRow::from_screen(parser.screen(), row, cols)
}

/// Whether absolute row `abs` soft-wraps into the next one, without cloning it.
fn row_wrapped_at(parser: &mut Parser, scrolled: usize, abs: usize) -> bool {
    let row = view_row(parser, scrolled, abs);
    parser.screen().row_wrapped(row)
}

/// Whether absolute row `abs` holds anything worth showing, without cloning it.
fn row_has_content_at(parser: &mut Parser, scrolled: usize, abs: usize, cols: u16) -> bool {
    let row = view_row(parser, scrolled, abs);
    let screen = parser.screen();
    (0..cols).any(|col| screen.cell(row, col).is_some_and(cell_has_content))
}

/// Clone scrollback row `idx` (0 = oldest) out of a parser whose scrollback
/// currently holds `scrolled` rows.
fn read_scrollback_row(parser: &mut Parser, scrolled: usize, idx: usize, cols: u16) -> CapturedRow {
    parser.screen_mut().set_scrollback(scrolled - idx);
    CapturedRow::from_screen(parser.screen(), 0, cols)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a capture from a CRLF-separated script, in a viewport far shorter
    /// than the output, the way a long-running command produces it.
    fn capture(lines: &[&str], rows: u16, cols: u16) -> CapturedScreen {
        let data = lines
            .iter()
            .map(|l| format!("{}\r\n", l))
            .collect::<String>();
        CapturedScreen::parse(data.as_bytes(), rows, cols, DEFAULT_MAX_SCROLLBACK_LINES)
    }

    fn texts(screen: &CapturedScreen) -> Vec<String> {
        let (_, cols) = screen.size();
        screen.rows(0, cols).collect()
    }

    #[test]
    fn captures_every_line_that_scrolled_off_the_viewport() {
        let lines: Vec<String> = (1..=200).map(|i| format!("line {}", i)).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let screen = capture(&refs, 10, 40);

        let rendered = texts(&screen);
        assert_eq!(rendered[0], "line 1");
        assert_eq!(rendered[199], "line 200");
        // 200 printed lines plus the row the cursor rests on, of which only the
        // last 10 were ever on screen.
        assert_eq!(rendered.len(), 201);
        assert_eq!(screen.scrollback_rows(), 191);
        assert!(!screen.truncated());
    }

    #[test]
    fn styles_survive_from_the_oldest_scrollback_rows() {
        let mut data = String::from("\x1b[31mred first line\x1b[0m\r\n");
        for i in 2..=100 {
            data.push_str(&format!("line {}\r\n", i));
        }
        let screen = CapturedScreen::parse(data.as_bytes(), 10, 40, DEFAULT_MAX_SCROLLBACK_LINES);
        let cell = screen.cell(0, 0).expect("first cell");
        assert_eq!(cell.contents(), "r");
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(1));
    }

    #[test]
    fn soft_wrap_flags_survive_in_scrollback() {
        // 30 characters in a 20-column viewport wraps onto a second row.
        let mut data = String::from("abcdefghijklmnopqrstuvwxyz1234\r\n");
        for i in 0..50 {
            data.push_str(&format!("filler {}\r\n", i));
        }
        let screen = CapturedScreen::parse(data.as_bytes(), 5, 20, DEFAULT_MAX_SCROLLBACK_LINES);
        assert!(screen.row_wrapped(0), "wrapped row lost its flag");
        assert!(!screen.row_wrapped(1));
        let rendered = texts(&screen);
        assert_eq!(rendered[0], "abcdefghijklmnopqrst");
        assert_eq!(rendered[1], "uvwxyz1234");
    }

    #[test]
    fn wide_characters_keep_their_continuation_cells_in_scrollback() {
        let mut data = String::from("日本語テスト\r\n");
        for i in 0..40 {
            data.push_str(&format!("filler {}\r\n", i));
        }
        let screen = CapturedScreen::parse(data.as_bytes(), 5, 20, DEFAULT_MAX_SCROLLBACK_LINES);
        assert_eq!(screen.cell(0, 0).unwrap().contents(), "日");
        assert!(screen.cell(0, 0).unwrap().is_wide());
        assert!(screen.cell(0, 1).unwrap().is_wide_continuation());
        assert_eq!(texts(&screen)[0], "日本語テスト");
    }

    #[test]
    fn head_selection_keeps_the_first_lines_only() {
        let lines: Vec<String> = (1..=50).map(|i| format!("line {}", i)).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let screen = capture(&refs, 10, 40).select(LineSelection::Head(10));

        let rendered = texts(&screen);
        assert_eq!(rendered.len(), 10);
        assert_eq!(rendered[0], "line 1");
        assert_eq!(rendered[9], "line 10");
    }

    #[test]
    fn tail_selection_keeps_the_last_lines_only() {
        let lines: Vec<String> = (1..=50).map(|i| format!("line {}", i)).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let screen = capture(&refs, 10, 40).select(LineSelection::Tail(10));

        let rendered = texts(&screen);
        assert_eq!(rendered.len(), 10);
        assert_eq!(rendered[0], "line 41");
        assert_eq!(rendered[9], "line 50");
    }

    /// Selection counts logical lines, so a soft-wrapped line is kept whole
    /// rather than being cut at the wrap.
    #[test]
    fn selection_keeps_wrapped_lines_whole() {
        let mut data = String::new();
        for i in 1..=10 {
            data.push_str(&format!("line {}\r\n", i));
        }
        // A 30-character line wraps onto two physical rows at 20 columns.
        data.push_str("wrapped-value-0123456789abcd\r\n");
        let screen = CapturedScreen::parse(data.as_bytes(), 5, 20, DEFAULT_MAX_SCROLLBACK_LINES);

        let tail = screen.clone().select(LineSelection::Tail(1));
        let rendered = texts(&tail);
        assert_eq!(rendered.len(), 2, "wrapped line was cut: {:?}", rendered);
        assert_eq!(rendered[0], "wrapped-value-012345");
        assert_eq!(rendered[1], "6789abcd");

        let head = screen.select(LineSelection::Head(11));
        assert_eq!(texts(&head).len(), 12);
    }

    /// Tail selection must not count the blank rows below the output.
    #[test]
    fn tail_selection_ignores_trailing_blank_rows() {
        let screen = capture(&["alpha", "beta", "gamma"], 40, 40).select(LineSelection::Tail(2));
        assert_eq!(
            texts(&screen),
            vec!["beta".to_string(), "gamma".to_string()]
        );
    }

    /// A full-screen program repaints the whole viewport; scrollback from
    /// before it started is not part of what it shows.
    #[test]
    fn alternate_screen_captures_only_the_active_screen() {
        let mut data = String::new();
        for i in 1..=50 {
            data.push_str(&format!("scrollback {}\r\n", i));
        }
        data.push_str("\x1b[?1049h\x1b[H\x1b[2Jtui view\r\n");
        let screen = CapturedScreen::parse(data.as_bytes(), 5, 40, DEFAULT_MAX_SCROLLBACK_LINES);

        assert!(screen.alternate_screen());
        assert_eq!(screen.size().0, 5);
        let rendered = texts(&screen);
        assert_eq!(rendered[0], "tui view");
        assert!(
            !rendered.iter().any(|r| r.contains("scrollback")),
            "alternate screen leaked scrollback: {:?}",
            rendered
        );
    }

    /// Leaving a full-screen program restores the normal screen, and with it
    /// everything that scrolled off before it ran.
    #[test]
    fn leaving_the_alternate_screen_restores_the_scrollback() {
        let mut data = String::new();
        for i in 1..=30 {
            data.push_str(&format!("line {}\r\n", i));
        }
        data.push_str("\x1b[?1049htui view\x1b[?1049l");
        let screen = CapturedScreen::parse(data.as_bytes(), 5, 40, DEFAULT_MAX_SCROLLBACK_LINES);

        assert!(!screen.alternate_screen());
        let rendered = texts(&screen);
        assert_eq!(rendered[0], "line 1");
        assert!(!rendered.iter().any(|r| r.contains("tui view")));
    }

    #[test]
    fn a_full_scrollback_buffer_is_reported_as_truncated() {
        let mut data = String::new();
        for i in 1..=60 {
            data.push_str(&format!("line {}\r\n", i));
        }
        let screen = CapturedScreen::parse(data.as_bytes(), 5, 40, 10);
        assert!(screen.truncated());
        assert_eq!(screen.scrollback_rows(), 10);
        // 60 lines plus the cursor row is 61 rows; only the last 5 were on
        // screen and only 10 of the other 56 fit in the buffer.
        assert_eq!(texts(&screen)[0], "line 47");
    }

    /// Short output that never scrolled must look exactly like a plain
    /// single-screen capture.
    #[test]
    fn short_output_is_unchanged_by_the_scrollback_capture() {
        let screen = capture(&["one", "two"], 24, 80);
        assert_eq!(screen.size(), (24, 80));
        assert_eq!(screen.scrollback_rows(), 0);
        assert!(!screen.truncated());
        let rendered = texts(&screen);
        assert_eq!(rendered[0], "one");
        assert_eq!(rendered[1], "two");
        assert!(rendered[2].is_empty());
    }

    #[test]
    fn head_and_tail_are_mutually_exclusive() {
        assert!(LineSelection::from_head_tail(Some(5), Some(5)).is_err());
        assert!(LineSelection::from_head_tail(Some(0), None).is_err());
        assert_eq!(
            LineSelection::from_head_tail(Some(5), None).unwrap(),
            LineSelection::Head(5)
        );
        assert_eq!(
            LineSelection::from_head_tail(None, Some(5)).unwrap(),
            LineSelection::Tail(5)
        );
        assert_eq!(
            LineSelection::from_head_tail(None, None).unwrap(),
            LineSelection::All
        );
    }

    /// The cursor keeps pointing at the same cell once scrollback is in front
    /// of it, and follows the rows a selection kept.
    #[test]
    fn cursor_position_is_mapped_into_capture_coordinates() {
        let mut data = String::new();
        for i in 1..=30 {
            data.push_str(&format!("line {}\r\n", i));
        }
        data.push_str("tail");
        let screen = CapturedScreen::parse(data.as_bytes(), 5, 40, DEFAULT_MAX_SCROLLBACK_LINES);
        let (row, col) = screen.cursor_position();
        assert_eq!(col, 4);
        assert_eq!(texts(&screen)[usize::from(row)], "tail");
    }

    // ---------------------------------------------------------------------
    // Head selection is the true beginning
    // ---------------------------------------------------------------------

    /// The point of the whole head path: the scrollback is a *tail* buffer, so
    /// by the end of a long run the first lines are long gone from it. Asking
    /// for the head must still answer with lines 1..N.
    #[test]
    fn head_returns_the_first_lines_of_output_far_longer_than_the_scrollback() {
        // Fifty thousand lines through a buffer that holds two hundred.
        let data: String = (1..=50_000).map(|i| format!("line {}\r\n", i)).collect();
        let screen =
            CapturedScreen::parse_selected(data.as_bytes(), 24, 80, 200, LineSelection::Head(10));

        let rendered = texts(&screen);
        assert_eq!(rendered.len(), 10);
        assert_eq!(rendered[0], "line 1");
        assert_eq!(rendered[9], "line 10");
        assert!(
            !screen.truncated(),
            "nothing the head shows was dropped, so it must not claim otherwise"
        );

        // The equivalent tail-buffer capture has lost those lines entirely,
        // which is exactly why head cannot be a filter applied to it.
        let whole = CapturedScreen::parse(data.as_bytes(), 24, 80, 200);
        assert!(whole.truncated());
        assert!(!texts(&whole)[0].starts_with("line 1\u{0}"));
        assert_ne!(texts(&whole)[0], "line 1");
    }

    /// Head selection follows the same rules as everything else: styles, soft
    /// wraps, wide characters, and hard newlines all survive it.
    #[test]
    fn head_preserves_styles_wraps_and_wide_characters() {
        let mut data = String::from("\x1b[1;31mALERT\x1b[0m\r\n");
        // A 30-character line wraps onto two of the 20 columns' rows.
        data.push_str("wrapped-value-0123456789abcd\r\n");
        data.push_str("\u{65e5}\u{672c}\u{8a9e}\r\n");
        for i in 1..=5_000 {
            data.push_str(&format!("filler {}\r\n", i));
        }
        let screen =
            CapturedScreen::parse_selected(data.as_bytes(), 10, 20, 100, LineSelection::Head(3));

        let rendered = texts(&screen);
        // Three logical lines, the second of which occupies two physical rows.
        assert_eq!(rendered.len(), 4, "unexpected rows: {:?}", rendered);
        assert_eq!(rendered[0], "ALERT");
        assert_eq!(rendered[1], "wrapped-value-012345");
        assert_eq!(rendered[2], "6789abcd");
        assert_eq!(rendered[3], "\u{65e5}\u{672c}\u{8a9e}");

        let alert = screen.cell(0, 0).expect("first cell");
        assert_eq!(alert.contents(), "A");
        assert_eq!(alert.fgcolor(), vt100::Color::Idx(1));
        assert!(alert.bold());

        assert!(screen.row_wrapped(1), "the wrapped row lost its flag");
        assert!(
            !screen.row_wrapped(2),
            "a hard newline was treated as a wrap"
        );
        assert!(screen.cell(3, 0).unwrap().is_wide());
        assert!(screen.cell(3, 1).unwrap().is_wide_continuation());
    }

    /// The staging buffer is independent of the configured capacity: retaining
    /// a single line of scrollback is a *tail* setting, and it must not turn
    /// `Head(10)` into "the head of whatever survived eviction".
    #[test]
    fn head_works_at_the_smallest_configured_capacity() {
        let data: String = (1..=1_000).map(|i| format!("line {}\r\n", i)).collect();

        for capacity in [1usize, 2, 5, 39, 40, 41, 100] {
            let screen = CapturedScreen::parse_selected(
                data.as_bytes(),
                40,
                80,
                capacity,
                LineSelection::Head(10),
            );
            let rendered = texts(&screen);
            let expected: Vec<String> = (1..=10).map(|i| format!("line {}", i)).collect();
            assert_eq!(
                rendered, expected,
                "capacity {capacity} did not return the true head"
            );
            assert!(
                !screen.truncated(),
                "capacity {capacity}: the head is complete, so nothing it shows was dropped"
            );
        }
    }

    /// The same run through the ordinary (tail) path with a one-line capacity
    /// really has lost the beginning, which is what the head path exists to
    /// avoid.
    #[test]
    fn a_one_line_capacity_loses_the_beginning_on_the_tail_path() {
        let data: String = (1..=1_000).map(|i| format!("line {}\r\n", i)).collect();
        let whole = CapturedScreen::parse(data.as_bytes(), 40, 80, 1);
        assert!(whole.truncated());
        assert_ne!(texts(&whole)[0], "line 1");
        assert_eq!(whole.scrollback_rows(), 1);
    }

    /// Output shorter than the requested head is returned whole, through the
    /// ordinary capture path.
    #[test]
    fn head_of_output_shorter_than_the_selection_is_the_whole_output() {
        let screen = CapturedScreen::parse_selected(
            b"alpha\r\nbeta\r\n",
            24,
            80,
            DEFAULT_MAX_SCROLLBACK_LINES,
            LineSelection::Head(10),
        );
        assert_eq!(
            texts(&screen),
            vec!["alpha".to_string(), "beta".to_string()]
        );
    }

    /// A session that spends its life in a full-screen program never scrolls
    /// anything off, so head selection falls back to the ordinary capture and
    /// shows the first lines of the screen the program painted.
    #[test]
    fn head_of_a_full_screen_program_is_the_top_of_its_screen() {
        let data = b"\x1b[?1049h\x1b[H\x1b[2Jtui view\r\nsecond row\r\nthird row\r\n";
        let screen = CapturedScreen::parse_selected(data, 10, 40, 100, LineSelection::Head(2));

        assert!(screen.alternate_screen());
        assert_eq!(
            texts(&screen),
            vec!["tui view".to_string(), "second row".to_string()]
        );
    }

    /// A session that printed for a long time before starting a full-screen
    /// program *did* have a beginning, and that is what head means: the first
    /// lines of the output, not the top of the screen that happens to be up
    /// when the capture ends.
    #[test]
    fn head_is_the_start_of_the_output_even_when_a_tui_follows() {
        let mut data = String::new();
        for i in 1..=2_000 {
            data.push_str(&format!("line {}\r\n", i));
        }
        data.push_str("\x1b[?1049h\x1b[H\x1b[2Jtui view\r\n");
        let screen =
            CapturedScreen::parse_selected(data.as_bytes(), 5, 40, 100, LineSelection::Head(2));

        assert_eq!(
            texts(&screen),
            vec!["line 1".to_string(), "line 2".to_string()]
        );
    }

    /// A prefix short enough to arrive in the same internal chunk as the
    /// full-screen program's startup sequence is still the beginning of the
    /// output. It never scrolled, so nothing was harvested on the way past; the
    /// head has to be settled from the normal screen before the entry sequence
    /// swaps the active grid.
    #[test]
    fn head_is_the_normal_prefix_when_a_tui_starts_in_the_same_chunk() {
        for entry in ["\x1b[?47h", "\x1b[?1047h", "\x1b[?1049h"] {
            let data = format!(
                "first line\r\nsecond line\r\nthird line\r\n{}\x1b[H\x1b[2Jtui view\r\nmore tui\r\n",
                entry
            );
            let screen = CapturedScreen::parse_selected(
                data.as_bytes(),
                10,
                40,
                100,
                LineSelection::Head(2),
            );

            assert!(
                !screen.alternate_screen(),
                "{entry}: the head is normal-screen output, not the program's screen"
            );
            assert_eq!(
                texts(&screen),
                vec!["first line".to_string(), "second line".to_string()],
                "{entry}: the head must be the output printed before the program started"
            );
        }
    }

    /// The whole prefix, when the selection asks for exactly as many lines as
    /// it printed.
    #[test]
    fn head_takes_the_entire_prefix_when_it_is_exactly_the_selection() {
        let data = "alpha\r\nbeta\r\n\x1b[?1049h\x1b[H\x1b[2Jtui view\r\n";
        let screen =
            CapturedScreen::parse_selected(data.as_bytes(), 10, 40, 100, LineSelection::Head(2));

        assert!(!screen.alternate_screen());
        assert_eq!(
            texts(&screen),
            vec!["alpha".to_string(), "beta".to_string()]
        );
    }

    /// A prefix too short to satisfy the selection leaves the capture to the
    /// alternate screen, exactly as before: the program's screen is all there
    /// is to show.
    #[test]
    fn a_prefix_shorter_than_the_head_still_shows_the_full_screen_program() {
        for entry in ["\x1b[?47h", "\x1b[?1049h"] {
            let data = format!(
                "only line\r\n{}\x1b[H\x1b[2Jtui view\r\nsecond row\r\n",
                entry
            );
            let screen = CapturedScreen::parse_selected(
                data.as_bytes(),
                10,
                40,
                100,
                LineSelection::Head(3),
            );

            assert!(
                screen.alternate_screen(),
                "{entry}: an unsatisfied head falls back to the active screen"
            );
            assert_eq!(
                texts(&screen)[..2],
                ["tui view".to_string(), "second row".to_string()]
            );
        }
    }

    /// Literal text that merely looks like a mode switch is text: without an
    /// ESC there is no sequence, so nothing about the capture changes.
    #[test]
    fn text_that_looks_like_an_alternate_screen_switch_is_not_one() {
        let data = "[?1049h and ?47h\r\nsecond line\r\nthird line\r\n";
        let screen =
            CapturedScreen::parse_selected(data.as_bytes(), 10, 40, 100, LineSelection::Head(2));

        assert!(!screen.alternate_screen());
        assert_eq!(
            texts(&screen),
            vec!["[?1049h and ?47h".to_string(), "second line".to_string()]
        );
    }

    /// `CSI ... h` without the `?` private prefix is SM, not DECSET, and a
    /// private mode that is not one of the alternate-screen numbers is not one
    /// either. Neither may end the normal-screen prefix early.
    #[test]
    fn only_alternate_screen_modes_end_the_normal_prefix() {
        for sequence in ["\x1b[47h", "\x1b[?1000h", "\x1b[?25h", "\x1b[?10490h"] {
            let data = format!("alpha\r\n{}beta\r\ngamma\r\n", sequence);
            let screen = CapturedScreen::parse_selected(
                data.as_bytes(),
                10,
                40,
                100,
                LineSelection::Head(3),
            );

            assert_eq!(
                texts(&screen),
                vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
                "{sequence:?} is not an alternate-screen entry"
            );
        }
    }

    /// An entry sequence is recognised whichever internal chunk boundary it
    /// would otherwise have straddled: the scan reads the rest of the buffer,
    /// not one chunk at a time. Driven through a capacity small enough that the
    /// head path really does feed the session a few bytes at a time.
    #[test]
    fn an_entry_sequence_split_across_chunks_is_still_found() {
        let mut data = String::new();
        for i in 1..=200 {
            data.push_str(&format!("line {}\r\n", i));
        }
        data.push_str("\x1b[?1049h\x1b[H\x1b[2Jtui view\r\n");
        for rows in [2u16, 3, 5, 8] {
            let screen = CapturedScreen::parse_selected(
                data.as_bytes(),
                rows,
                40,
                10,
                LineSelection::Head(2),
            );
            assert!(!screen.alternate_screen(), "rows {rows}");
            assert_eq!(
                texts(&screen),
                vec!["line 1".to_string(), "line 2".to_string()],
                "rows {rows}"
            );
        }
    }

    /// The head keeps counting across a full-screen program that comes and
    /// goes: what it painted is not output, and the normal-screen lines
    /// printed after it are.
    #[test]
    fn head_spanning_a_finished_full_screen_program_uses_normal_output() {
        let data = "alpha\r\n\x1b[?1049h\x1b[H\x1b[2Jtui view\r\n\x1b[?1049lbeta\r\ngamma\r\n";
        let screen =
            CapturedScreen::parse_selected(data.as_bytes(), 10, 40, 100, LineSelection::Head(3));

        assert!(!screen.alternate_screen());
        assert_eq!(
            texts(&screen),
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
        );
    }

    #[test]
    fn alternate_screen_entry_scan_matches_only_real_sequences() {
        for entry in [
            &b"\x1b[?47h"[..],
            b"\x1b[?1047h",
            b"\x1b[?1049h",
            b"\x1b[?1;1049h",
            b"\x1b[?1049;25h",
        ] {
            let mut data = b"before".to_vec();
            data.extend_from_slice(entry);
            data.extend_from_slice(b"after");
            assert_eq!(
                next_alt_screen_entry(&data, data.len()),
                Some((6, 6 + entry.len())),
                "{:?} should be recognised",
                String::from_utf8_lossy(entry)
            );
            // The start bound is exclusive of the sequence itself, and a
            // sequence reaching past it is still read whole.
            assert_eq!(next_alt_screen_entry(&data, 7), Some((6, 6 + entry.len())));
            assert_eq!(next_alt_screen_entry(&data, 6), None);
        }

        for other in [
            &b"[?1049h"[..],
            b"\x1b[47h",
            b"\x1b[?1049l",
            b"\x1b[?1049",
            b"\x1b[?25h",
            b"\x9b?1049h",
        ] {
            assert_eq!(
                next_alt_screen_entry(other, other.len()),
                None,
                "{:?} should not be recognised",
                String::from_utf8_lossy(other)
            );
        }
    }

    // ---------------------------------------------------------------------
    // Exact truncation reporting
    // ---------------------------------------------------------------------

    /// Build a capture of exactly `lines` lines through a `capacity`-line
    /// scrollback and report whether it says output was lost.
    fn truncation_at(lines: usize, rows: u16, capacity: usize) -> (bool, usize, String) {
        let data: String = (1..=lines).map(|i| format!("line {}\r\n", i)).collect();
        let screen = CapturedScreen::parse(data.as_bytes(), rows, 40, capacity);
        let first = texts(&screen)[0].clone();
        (screen.truncated(), screen.scrollback_rows(), first)
    }

    /// The boundary itself: filling the scrollback to the last row is not
    /// truncation, and the very next line is.
    #[test]
    fn truncation_is_exact_at_the_scrollback_boundary() {
        // A 5-row viewport, so `n` printed lines scroll `n + 1 - 5` rows off.
        // 14 lines scroll exactly 10 - a full buffer that lost nothing.
        let (truncated, retained, first) = truncation_at(14, 5, 10);
        assert!(
            !truncated,
            "a scrollback filled to the last row has dropped nothing"
        );
        assert_eq!(retained, 10);
        assert_eq!(first, "line 1");

        // One more line evicts one row, and that is truncation.
        let (truncated, retained, first) = truncation_at(15, 5, 10);
        assert!(truncated, "an evicted line must be reported");
        assert_eq!(retained, 10);
        assert_eq!(first, "line 2");

        // Well short of the boundary: nothing scrolled off at all.
        let (truncated, retained, first) = truncation_at(4, 5, 10);
        assert!(!truncated);
        assert_eq!(retained, 0);
        assert_eq!(first, "line 1");
    }

    /// A zero-line scrollback keeps only the screen - and must say so the
    /// moment anything scrolls past it, rather than reporting a buffer that
    /// "is not full" because it can never be.
    #[test]
    fn a_zero_line_scrollback_reports_overflow() {
        let (truncated, retained, _) = truncation_at(4, 5, 0);
        assert!(!truncated, "nothing scrolled off a 5-row viewport");
        assert_eq!(retained, 0);

        let (truncated, retained, first) = truncation_at(20, 5, 0);
        assert!(truncated, "16 rows scrolled off and none were kept");
        assert_eq!(retained, 0);
        assert_eq!(first, "line 17");
    }

    // ---------------------------------------------------------------------
    // Memory bounds
    // ---------------------------------------------------------------------

    /// A line limit is not a memory limit: the same 60,000 lines cost twenty
    /// times more in a 500-column terminal than in a 25-column one. The
    /// effective capacity therefore falls as the terminal widens.
    #[test]
    fn scrollback_capacity_is_bounded_by_the_cell_budget() {
        for (rows, cols) in [
            (1u16, 1u16),
            (24, 80),
            (40, 120),
            (500, 500),
            (24, u16::MAX),
        ] {
            let effective = effective_scrollback_lines(rows, cols, usize::MAX);
            let cells = effective
                .saturating_add(usize::from(rows))
                .saturating_mul(usize::from(cols));
            assert!(
                cells <= MAX_RETAINED_CELLS.saturating_add(usize::from(rows) * usize::from(cols)),
                "{rows}x{cols} would retain {cells} cells"
            );
            assert!(effective <= MAX_SCROLLBACK_LIMIT);
        }

        // The default configuration is comfortably inside the budget, so the
        // bound never quietly shrinks ordinary captures.
        assert_eq!(
            effective_scrollback_lines(40, 120, DEFAULT_MAX_SCROLLBACK_LINES),
            DEFAULT_MAX_SCROLLBACK_LINES
        );
        // A very wide terminal is capped below it.
        assert!(effective_scrollback_lines(40, 500, DEFAULT_MAX_SCROLLBACK_LINES) < 10_000);
    }

    /// The bound is applied before the rows are built, not after: a capture of
    /// a huge terminal asking for a huge scrollback allocates what the budget
    /// allows and no more.
    #[test]
    fn a_huge_request_is_bounded_before_the_rows_are_built() {
        let rows = 500u16;
        let cols = 500u16;
        let data: String = (1..=6_000).map(|i| format!("line {}\r\n", i)).collect();
        let screen = CapturedScreen::parse(data.as_bytes(), rows, cols, usize::MAX);

        let retained = usize::from(screen.size().0) * usize::from(cols);
        assert!(
            retained <= MAX_RETAINED_CELLS + usize::from(rows) * usize::from(cols),
            "capture retained {retained} cells"
        );
        assert!(screen.truncated(), "the oldest lines had to be dropped");
    }
}
