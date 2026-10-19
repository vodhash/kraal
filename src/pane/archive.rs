//! A pane's history in two parts. The emulator keeps the newest rows, a couple of screens of
//! them, at full width; the archive keeps the older lines, compact (see `history.rs`). Output
//! moves rows from the first to the second as it pushes them up: the parser hands them to a
//! thread that encodes them (see `store.rs`). Resizing reflows only what the emulator holds.
//!
//! Rows move only above everything in use: the view, the selection and the copy-mode cursor, with
//! a screen to spare. Both parts together hold the pane's scrollback, as the emulator alone did:
//! until the next move they may hold more, by the rows the emulator gains meanwhile at most (a
//! couple of screens and a batch), rather than let go of rows the emulator alone would keep.
//! Rows move only while the emulator cannot fill up before the next move: it would then drop
//! rows newer than the archive's. It holds the whole history instead, the archive's rows brought
//! back.
//!
//! A flood of output, more rows within a second than the scrollback holds (or than the archive
//! keeps up with), stays in the emulator as it would without the archive (most of it goes before
//! anyone looks), and moves to the archive once the output pauses.
//!
//! When the view, the copy-mode cursor or a search goes above what the emulator holds, archived
//! lines are laid out again, at the pane's width then, above the emulator's rows: a [`Lens`] on
//! the archive, which keeps them. It holds the lines around the view only: far back (the top of
//! the history), the lines between it and the emulator's rows are laid out once the view reaches
//! them. It goes when the view comes back down. Command marks are found on the history whole: the
//! lines between are laid out for them, and become the emulator's own.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Grid, Row, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, Color, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, NamedColor, NamedPrivateMode, PrivateMode, Rgb,
    ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode, cursor_icon::CursorIcon,
};

use super::history;
use super::pty::TermPane;
use super::store::{Batch, Lines, Piece, Store};

/// The emulator allocates the rows of its history a thousand at a time: the history stays
/// within the first thousand.
const EMULATOR_ROWS: usize = 1000;
/// Rows of output between two moves while what is in use keeps rows in the emulator.
const BATCH: usize = 256;
/// Rows brought back at least at a time, so that scrolling line by line does not do it each line.
const PAGE: usize = 256;
/// Output that would move more rows than the scrollback holds within this is a flood.
const FLOOD_WINDOW: Duration = Duration::from_secs(1);
/// A pause in the output this long ends a flood.
const CALM: Duration = Duration::from_millis(100);
/// A longer line moves in pieces of this many rows (as when it is saved).
const MAX_LINE_ROWS: usize = 1000;
/// Batches handed to the store and not stored yet beyond which output is a flood.
const MAX_BACKLOG: usize = 8;
/// Rows of output between two looks at the scrollback's bounds while there is a lens.
const LENS_BATCH: usize = 64;
/// Rows moved at once beyond which the emulator's rows go with them (see `compact`).
const MANY: usize = 2 * BATCH;
/// No character before this one is wide.
const WIDE_FROM: char = '\u{1100}';

pub(super) struct Archive {
    store: Arc<Store>,
    /// Rows to put in the emulator's grid in place of those handed to the store.
    spare: Vec<Row<Cell>>,
    /// The most rows of history, both parts together: the pane's scrollback.
    limit: usize,
    /// Rows of history the emulator holds before the oldest move out, and how many stay then.
    high: usize,
    keep: usize,
    /// Above how many rows of history the next move is due: higher than `high` while rows in
    /// use stay in the emulator.
    due: usize,
    /// Output floods the pane: rows stay in the emulator, as many as the scrollback holds, until
    /// it stops. And when the rows moved recently started being counted, and how many.
    flood: bool,
    window: (Instant, usize),
    /// When output was last parsed.
    last: Instant,
    /// A flood's rows moved here, freeing the emulator's.
    freed: bool,
    /// Since the output stopped, the emulator keeps no history of its own.
    rested: bool,
    lens: Option<Lens>,
    /// The line the view showed at its top before a resize dropped the lens, to show again.
    reopen: Option<u64>,
}

/// Archived lines laid out at the top of the emulator's history, above the rows it holds of its
/// own, for the view, the copy-mode cursor and a selection to be on. They stay in the archive:
/// these rows are a copy, and count in none of the history's lengths (the emulator's history may
/// hold `limit` rows of its own below them). The lines between them and the emulator's own, if
/// any, are laid out only when the view reaches them.
struct Lens {
    /// The numbers of the lines shown, from `start` to `end` (not included), and the rows each
    /// takes; and of the archive's newest line and one: the lines from `end` to `last` are
    /// between the lens and the emulator's own (the archive gains none while there is a lens).
    start: u64,
    end: u64,
    last: u64,
    sizes: VecDeque<usize>,
    rows: usize,
}

impl Archive {
    pub(super) fn new(limit: usize, screen: usize) -> Archive {
        let mut archive = Archive {
            store: Arc::default(),
            spare: Vec::new(),
            limit,
            high: 0,
            keep: 0,
            due: 0,
            flood: false,
            window: (Instant::now(), 0),
            last: Instant::now(),
            freed: false,
            rested: false,
            lens: None,
            reopen: None,
        };
        archive.fit(screen);
        archive
    }

    /// The archived lines, once those handed over are stored.
    pub(super) fn lines(&self) -> Lines<'_> {
        self.store.lines()
    }

    /// Whether the rows handed over are stored: reading the lines stores none.
    pub(super) fn stored(&self) -> bool {
        self.store.backlog() == 0
    }

    /// Rows of the emulator's history the lens shows.
    pub(super) fn lens_rows(&self) -> usize {
        self.lens.as_ref().map_or(0, |l| l.rows)
    }

    /// The most rows the emulator's history holds: its own, and the lens's.
    fn max(&self) -> usize {
        self.limit + self.lens_rows()
    }

    /// The screen has `screen` rows now.
    pub(super) fn fit(&mut self, screen: usize) {
        // The emulator keeps a couple of screens of history, to scroll back to without the
        // archive (and all a resize reflows), and up to a batch more before the oldest move out.
        // Output adds up to a screen of rows before the next look at the history's length: with
        // it, the history stays within the rows the emulator allocates first, and within the
        // scrollback (see `room_beside`).
        let high = EMULATOR_ROWS.saturating_sub(screen).max(BATCH).min(2 * screen.max(32) + BATCH);
        self.high = high.min(self.limit.saturating_sub(screen));
        self.keep = self.high.saturating_sub(BATCH).max(self.high / 2);
        self.due = self.high;
    }

    /// The length of the emulator's history at the next look at it, when it holds `own` rows of
    /// its own now: `high`, or a batch more while rows in use stay.
    fn next_look(&self, own: usize) -> usize {
        if own > self.high { own + BATCH } else { self.high }
    }

    /// The rows the archive keeps while the emulator holds `own` rows of its own, and may hold
    /// `next` and a screen of them at the next look: all the scrollback leaves beside them (both
    /// parts may hold more than the scrollback meanwhile, by the rows the emulator gains). None
    /// when the emulator may fill up before then: it would drop rows of its own newer than the
    /// archive's.
    fn room_beside(&self, own: usize, next: usize, screen: usize) -> usize {
        if next + screen <= self.limit { self.limit - own } else { 0 }
    }

    /// The output about to be parsed may push up to a screen of rows into the history: move the
    /// oldest out if the emulator holds too many.
    fn make_room<T: EventListener>(&mut self, term: &mut Term<T>, seen: &mut i64) {
        let screen = term.screen_lines();
        if self.lens.is_some() {
            // The emulator keeps the history as it does alone below the lens; the oldest lines
            // go as rows come, as they would from the emulator alone, a few at a time.
            let history = term.grid().history_size();
            self.due = history + LENS_BATCH;
            let own = history - self.lens_rows();
            let trim = self.room_beside(own, own + LENS_BATCH, screen);
            let first = {
                let mut lines = self.store.lines();
                lines.trim(trim, false);
                lines.span().start
            };
            self.lens_cut(term, first);
            return;
        }
        if !self.flood {
            let moved = self.compact(term, seen, self.keep);
            let now = Instant::now();
            if now.duration_since(self.window.0) > FLOOD_WINDOW {
                self.window = (now, 0);
            }
            self.window.1 += moved;
            if self.window.1 <= self.limit && self.store.backlog() <= MAX_BACKLOG {
                return;
            }
            // More than the scrollback holds within the window: most of it would be kept only
            // to be dropped. Or more than the store keeps up with. Until the output pauses (see
            // `calm`).
            self.flood = true;
        }
        // The emulator keeps the history as it does alone: what is older goes first, or comes
        // back to it once it may fill up (as in `compact`).
        let history = term.grid().history_size();
        match self.room_beside(history, history + BATCH, screen) {
            0 => {
                self.page_in(term, seen, usize::MAX, (0, 0));
            }
            room => self.store.send(Batch::trim(room)),
        }
        self.due = term.grid().history_size() + BATCH;
    }

    /// Hand the oldest lines of the emulator's history to the store, as long as the rows of each
    /// are all above those in use and `keep` rows stay. Returns the rows moved; `seen` (the
    /// history's length as last counted for the marks, see `commands.rs`) goes down by as many.
    pub(super) fn compact<T: EventListener>(&mut self, term: &mut Term<T>, seen: &mut i64, keep: usize) -> usize {
        if self.limit == 0 || self.lens.is_some() || term.mode().contains(TermMode::ALT_SCREEN) {
            return 0;
        }
        let grid = term.grid();
        let history = grid.history_size();
        let top = -(history as i32);
        let bound = first_in_use(term).min(-(keep as i32));
        let columns = grid.columns();
        let screen = grid.screen_lines();
        let wraps = |l: i32| grid[Line(l)][Column(columns - 1)].flags.contains(Flags::WRAPLINE);
        let mut lines = Vec::new();
        let mut line = top;
        while line < bound {
            let mut end = line;
            while wraps(end) && end + 1 < screen as i32 && end + 1 - line < MAX_LINE_ROWS as i32 {
                end += 1;
            }
            if end >= bound {
                break;
            }
            lines.push(Piece { rows: (end + 1 - line) as u32, wraps: wraps(end) });
            line = end + 1;
        }
        let moved = (line - top) as usize;
        let left = history - moved;
        let next = self.next_look(left);
        let trim = self.room_beside(left, next, screen);
        if trim == 0 {
            // The emulator may fill up before the next look: rows moved would go at once. It
            // holds the whole history instead, as it does alone, the archive's rows too.
            self.page_in(term, seen, usize::MAX, (0, 0));
            self.due = self.next_look(term.grid().history_size());
            return 0;
        }
        self.due = next;
        if moved == 0 && left <= self.high {
            return 0;
        }
        let mut batch = Batch { rows: Vec::with_capacity(moved), lines, trim };
        if moved > MANY {
            // Many rows (after a flood, or a look far back): the emulator lets go of the rows it
            // keeps ahead too, rather than get as many new ones.
            let grid = term.grid_mut();
            batch.rows.extend((top..line).map(|l| std::mem::take(&mut grid[Line(l)])));
            grid.update_history(left);
            grid.truncate();
            grid.update_history(self.limit);
            *seen -= moved as i64;
        } else if moved > 0 {
            if self.spare.len() < moved {
                self.store.take_spare(columns, moved - self.spare.len(), &mut self.spare);
            }
            let grid = term.grid_mut();
            for l in top..line {
                let spare = match self.spare.pop() {
                    Some(row) if row.len() == columns => row,
                    _ => Row::new(columns),
                };
                batch.rows.push(std::mem::replace(&mut grid[Line(l)], spare));
            }
            grid.update_history(left);
            grid.update_history(self.limit);
            *seen -= moved as i64;
        }
        self.store.send(batch);
        moved
    }

    /// Bring back at least `wanted` rows (whole lines, the newest first) above the emulator's
    /// history, laid out at its width, and lines that take at least `rows` rows at width `at`.
    /// Returns the rows brought back; `seen` goes up by as many.
    pub(super) fn page_in<T: EventListener>(
        &mut self,
        term: &mut Term<T>,
        seen: &mut i64,
        wanted: usize,
        (at, rows): (usize, usize),
    ) -> usize {
        if (wanted == 0 && rows == 0) || self.lens.is_some() || term.mode().contains(TermMode::ALT_SCREEN) {
            return 0;
        }
        let mut lines = self.store.lines();
        if lines.is_empty() {
            return 0;
        }
        let grid = term.grid();
        let width = grid.columns();
        lines.set_width(width);
        // The oldest lines that no longer fit in the scrollback at this width go, once their rows
        // are counted at it: not for a few rows (a resize), they go when the top is shown.
        lines.trim(self.limit.saturating_sub(grid.history_size()), false);
        let room = self.limit.saturating_sub(grid.history_size());
        // The newest lines, with the rows each takes.
        let mut picked = Vec::new();
        let (mut count, mut counted) = (0, 0);
        for line in lines.newest_first() {
            let taken = line.rows_at(width);
            if (count >= wanted && counted >= rows) || count + taken > room {
                break;
            }
            count += taken;
            counted += if counted < rows { line.rows_at(at) } else { 0 };
            picked.push((line, taken));
        }
        if count == 0 {
            return 0;
        }
        picked.reverse();
        let blank = grid.cursor.template.bg == Color::Named(NamedColor::Background);
        prepend(term.grid_mut(), count, |rows| lay_out(&picked, width, rows, blank));
        let n = picked.len();
        drop(picked);
        for _ in 0..n {
            lines.drop_newest();
        }
        drop(lines);
        *seen += count as i64;
        // They stay while output runs, until a batch of it.
        self.due = self.due.max(term.grid().history_size() + BATCH);
        count
    }

    /// Lay out the archived lines from the one numbered `at` (the oldest without it) at the top of
    /// the emulator's history, two screens of them: a lens on them. Whether line `at` is the
    /// grid's top line now.
    fn open_lens<T>(&mut self, term: &mut Term<T>, at: Option<u64>) -> bool {
        if self.limit == 0 || term.mode().contains(TermMode::ALT_SCREEN) {
            return false;
        }
        if let Some(start) = self.lens.as_ref().map(|l| l.start) {
            let first = self.lines().span().start;
            if at.unwrap_or(first).max(first) == start {
                return true;
            }
            // Its rows are those the new one is laid out in.
            self.drop_lens(term);
        }
        let store = self.store.clone();
        let mut lines = store.lines();
        let grid = term.grid();
        let (width, screen, history) = (grid.columns(), grid.screen_lines(), grid.history_size());
        lines.set_width(width);
        // The oldest lines that no longer fit in the scrollback at this width go first.
        lines.trim(self.limit.saturating_sub(history), true);
        let span = lines.span();
        let at = at.map_or(span.start, |at| at.max(span.start));
        if at >= span.end {
            return false;
        }
        let mut picked = Vec::new();
        let mut count = 0;
        for line in lines.from(at) {
            if count >= 2 * screen {
                break;
            }
            let rows = line.rows_at(width);
            count += rows;
            picked.push((line, rows));
        }
        let blank = grid.cursor.template.bg == Color::Named(NamedColor::Background);
        let sizes = picked.iter().map(|(_, rows)| *rows).collect();
        let end = at + picked.len() as u64;
        self.lens = Some(Lens { start: at, end, last: span.end, sizes, rows: count });
        let grid = term.grid_mut();
        grid.update_history(self.max());
        prepend(grid, count, |rows| lay_out(&picked, width, rows, blank));
        true
    }

    /// Take the lens's rows out of the emulator's history, leaving its own.
    fn drop_lens<T>(&mut self, term: &mut Term<T>) {
        let Some(lens) = self.lens.take() else { return };
        let grid = term.grid_mut();
        let own = grid.history_size().saturating_sub(lens.rows);
        grid.update_history(own);
        grid.update_history(self.limit);
        keep_on_grid(term);
        if lens.rows > EMULATOR_ROWS {
            // Many rows that showed a long history went: their memory goes back.
            self.store.release();
        }
    }

    /// Lay out at least `rows` rows more of the archived lines above the lens (or above the
    /// emulator's history: a lens on the newest lines). Whether there were.
    fn lens_up<T>(&mut self, term: &mut Term<T>, rows: usize) -> bool {
        if self.limit == 0 || term.mode().contains(TermMode::ALT_SCREEN) {
            return false;
        }
        let store = self.store.clone();
        let mut lines = store.lines();
        let grid = term.grid();
        let width = grid.columns();
        let start = match &self.lens {
            Some(lens) => lens.start,
            None => {
                lines.set_width(width);
                lines.trim(self.limit.saturating_sub(grid.history_size()), false);
                lines.span().end
            }
        };
        let mut picked = Vec::new();
        let mut count = 0;
        for line in lines.before(start) {
            if count >= rows {
                break;
            }
            let rows = line.rows_at(width);
            count += rows;
            picked.push((line, rows));
        }
        if count == 0 {
            return false;
        }
        picked.reverse();
        let blank = grid.cursor.template.bg == Color::Named(NamedColor::Background);
        let lens =
            self.lens.get_or_insert_with(|| Lens { start, end: start, last: start, sizes: VecDeque::new(), rows: 0 });
        lens.start -= picked.len() as u64;
        for (_, rows) in picked.iter().rev() {
            lens.sizes.push_front(*rows);
        }
        lens.rows += count;
        let grid = term.grid_mut();
        grid.update_history(self.max());
        prepend(grid, count, |rows| lay_out(&picked, width, rows, blank));
        true
    }

    /// Make the lens show at least `rows` rows below `line`, laying out the lines between it and
    /// the emulator's own: what it shows moves up as many rows, and the view, the copy-mode
    /// cursor and the selection with it.
    fn lens_down<T>(&mut self, term: &mut Term<T>, line: i32, rows: usize) {
        let Some(lens) = &self.lens else { return };
        if lens.end >= lens.last {
            return;
        }
        let (end, shown) = (lens.end, lens.rows);
        let grid = term.grid();
        let history = grid.history_size();
        let own = (history - shown) as i32;
        let below = (i64::from(-own - 1) - i64::from(line)).max(0) as usize;
        if below >= rows {
            return;
        }
        let wanted = (rows - below).max(PAGE);
        let store = self.store.clone();
        let lines = store.lines();
        let width = grid.columns();
        let mut picked = Vec::new();
        let mut count = 0;
        for line in lines.from(end) {
            if count >= wanted {
                break;
            }
            let rows = line.rows_at(width);
            count += rows;
            picked.push((line, rows));
        }
        if count == 0 {
            return;
        }
        let blank = grid.cursor.template.bg == Color::Named(NamedColor::Background);
        let lens = self.lens.as_mut().expect("there is a lens");
        lens.end += picked.len() as u64;
        lens.sizes.extend(picked.iter().map(|(_, rows)| *rows));
        lens.rows += count;
        let max = self.limit + lens.rows;
        let grid = term.grid_mut();
        grid.update_history(max);
        prepend(grid, count, |rows| lay_out(&picked, width, rows, blank));
        drop(picked);
        drop(lines);
        // Below the lens's rows.
        let top = -((history + count) as i32);
        let mut moved: Vec<Row<Cell>> =
            (top..top + (count + shown) as i32).map(|l| std::mem::take(&mut grid[Line(l)])).collect();
        moved.rotate_left(count);
        for (l, row) in (top..).zip(moved) {
            grid[Line(l)] = row;
        }
        if grid.display_offset() > own as usize {
            grid.scroll_display(Scroll::Delta(count as i32));
        }
        let cursor = &mut term.vi_mode_cursor.point;
        if cursor.line.0 < -own {
            cursor.line = Line(cursor.line.0 - count as i32);
        }
        match term.selection.take() {
            Some(selection) => term.selection = selection.rotate(&*term, &(Line(top)..Line(-own)), count as i32),
            None => self.lens_trim(term),
        }
    }

    /// Make the lens's rows the emulator's own, once it shows all the lines down to them: the
    /// archive keeps the lines above.
    fn own_lens<T>(&mut self, term: &mut Term<T>, seen: &mut i64) {
        let Some(lens) = self.lens.take_if(|l| l.end >= l.last) else { return };
        let mut lines = self.store.lines();
        for _ in lens.start..lens.end {
            lines.drop_newest();
        }
        drop(lines);
        *seen += lens.rows as i64;
        term.grid_mut().update_history(self.limit);
        keep_on_grid(term);
        self.due = self.due.max(term.grid().history_size() + BATCH);
    }

    /// Lines the lens shows far below the view go (a page of rows stays below it), unless a
    /// selection may be on them: what it shows above moves down as many rows, and the view and
    /// the copy-mode cursor with it.
    fn lens_trim_below<T>(&mut self, term: &mut Term<T>) {
        if term.selection.is_some() {
            return;
        }
        let Some(lens) = &mut self.lens else { return };
        let grid = term.grid();
        let (history, offset) = (grid.history_size(), grid.display_offset());
        let own = history - lens.rows;
        let below = offset.saturating_sub(own + grid.screen_lines());
        let mut cut = 0;
        while lens.sizes.len() > 1 && below - cut >= PAGE + lens.sizes[lens.sizes.len() - 1] {
            cut += lens.sizes.pop_back().expect("more than one");
            lens.end -= 1;
        }
        if cut == 0 {
            return;
        }
        lens.rows -= cut;
        let max = self.limit + lens.rows;
        let grid = term.grid_mut();
        // They go to the top of the grid, and go from there.
        let top = -(history as i32);
        let mut rows: Vec<Row<Cell>> = (top..-(own as i32)).map(|l| std::mem::take(&mut grid[Line(l)])).collect();
        rows.rotate_right(cut);
        for (l, row) in (top..).zip(rows) {
            grid[Line(l)] = row;
        }
        grid.update_history(history - cut);
        grid.update_history(max);
        grid.scroll_display(Scroll::Delta((offset - cut) as i32 - grid.display_offset() as i32));
        let cursor = &mut term.vi_mode_cursor.point;
        if cursor.line.0 < -(own as i32) {
            cursor.line = Line(cursor.line.0 + cut as i32);
        }
    }

    /// Lines the lens shows far above the view and the copy-mode cursor go (a page of rows stays
    /// above them).
    fn lens_trim<T>(&mut self, term: &mut Term<T>) {
        let Some(lens) = &mut self.lens else { return };
        let mut top = -(term.grid().display_offset() as i32);
        if term.mode().contains(TermMode::VI) {
            top = top.min(term.vi_mode_cursor.point.line.0);
        }
        let grid = term.grid_mut();
        let history = grid.history_size();
        let above = (history as i64 + i64::from(top)).max(0) as usize;
        let mut cut = 0;
        while lens.sizes.len() > 1 && above - cut >= PAGE + lens.sizes[0] {
            cut += lens.sizes.pop_front().expect("more than one");
            lens.start += 1;
        }
        if cut > 0 {
            lens.rows -= cut;
            grid.update_history(history - cut);
            grid.update_history(self.limit + lens.rows);
        }
    }

    /// The archive's lines before the one numbered `first` went: so do the lens's.
    fn lens_cut<T>(&mut self, term: &mut Term<T>, first: u64) {
        let Some(lens) = &mut self.lens else { return };
        let mut cut = 0;
        while lens.start < first
            && let Some(rows) = lens.sizes.pop_front()
        {
            cut += rows;
            lens.start += 1;
        }
        if cut == 0 {
            return;
        }
        lens.rows -= cut;
        let rows = lens.rows;
        let grid = term.grid_mut();
        let history = grid.history_size();
        grid.update_history(history - cut);
        if rows < grid.screen_lines() {
            // Too little left to show.
            self.drop_lens(term);
        } else {
            grid.update_history(self.limit + rows);
            keep_on_grid(term);
        }
    }
}

/// The topmost line in use: the view, the selection, the copy-mode cursor, and a screen above.
fn first_in_use<T>(term: &Term<T>) -> i32 {
    let grid = term.grid();
    let mut line = -(grid.display_offset() as i32);
    if let Some(range) = term.selection.as_ref().and_then(|s| s.to_range(term)) {
        line = line.min(range.start.line.0);
    }
    if term.mode().contains(TermMode::VI) {
        line = line.min(term.vi_mode_cursor.point.line.0);
    }
    line - grid.screen_lines() as i32
}

/// A region below the top line that scrolls takes a view scrolled back further up, as if its
/// rows went into the history (alacritty's grid), though none did: the view goes no higher than
/// the top of the history, where rows can be read.
pub(super) fn keep_view_in_history<T: EventListener>(term: &mut Term<T>) {
    if term.grid().display_offset() > term.grid().history_size() {
        term.scroll_display(Scroll::Top);
    }
}

/// The length of the UTF-8 character `lead` starts (1 for any other byte).
pub(super) fn utf8_len(lead: u8) -> usize {
    match lead {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    }
}

/// Whether `c` is a zero-width character its cell has no room left for. alacritty 0.26 keeps
/// every one, so a flood of them would grow the pane and each frame without limit. The cell is
/// the one it picks: before the cursor, or the first half of a wide character.
pub(super) fn zerowidth_full<T>(term: &Term<T>, c: char) -> bool {
    use unicode_width::UnicodeWidthChar;
    if c.width() != Some(0) {
        return false;
    }
    let cursor = &term.grid().cursor;
    let mut column = cursor.point.column;
    if !cursor.input_needs_wrap {
        column.0 = column.saturating_sub(1);
    }
    let row = &term.grid()[cursor.point.line];
    if row[column].flags.contains(Flags::WIDE_CHAR_SPACER) {
        column.0 = column.saturating_sub(1);
    }
    row[column].zerowidth().is_some_and(|z| z.len() >= super::scrollback::MAX_ZEROWIDTH)
}

/// Make `edit` to the cursor's row (deleting, inserting or erasing characters, erasing part of
/// the line or the screen, a character inserted), then blank the half it left of a wide
/// character. alacritty 0.26 keeps it whole on one side: a cursor on a second half alone at the
/// first column is drawn from the cell before the row's start (a panic), and its search, going
/// on from a match ending with a first half alone, finds that match again for ever.
pub(super) fn mended<T>(term: &mut Term<T>, edit: impl FnOnce(&mut Term<T>)) {
    edit(term);
    let line = term.grid().cursor.point.line;
    let row = &mut term.grid_mut()[line];
    let last = row.len() - 1;
    for c in 0..=last {
        let flags = row[Column(c)].flags;
        let half = if flags.contains(Flags::WIDE_CHAR) {
            c < last && !row[Column(c + 1)].flags.contains(Flags::WIDE_CHAR_SPACER)
        } else if flags.contains(Flags::WIDE_CHAR_SPACER) {
            c == 0 || !row[Column(c - 1)].flags.contains(Flags::WIDE_CHAR)
        } else {
            // It says a wide character did not fit at the end of the row: anywhere else, nothing.
            flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) && c < last
        };
        if half {
            let cell = &mut row[Column(c)];
            cell.clear_wide();
            cell.flags.remove(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
        }
    }
}

/// How many bytes at the end of `bytes` start a UTF-8 character without ending it.
pub(super) fn incomplete_char(bytes: &[u8]) -> usize {
    let start = bytes.len().saturating_sub(3);
    match (start..bytes.len()).rev().find(|i| bytes[*i] & 0xc0 != 0x80) {
        Some(i) if matches!(std::str::from_utf8(&bytes[i..]), Err(e) if e.valid_up_to() == 0 && e.error_len().is_none()) => {
            bytes.len() - i
        }
        _ => 0,
    }
}

/// Rows went from the top of the grid: the copy-mode cursor stays on it, and a selection that
/// was on them goes.
fn keep_on_grid<T>(term: &mut Term<T>) {
    let top = term.topmost_line();
    let cursor = &mut term.vi_mode_cursor.point;
    cursor.line = cursor.line.max(top);
    if term.selection.as_ref().is_some_and(|s| s.intersects_range(..top)) {
        term.selection = None;
    }
}

/// Write `lines` (oldest first, with the rows each takes at `width`) into `rows`.
fn lay_out(lines: &[(history::Line<'_>, usize)], width: usize, mut rows: &mut [Row<Cell>], blank: bool) {
    for (line, n) in lines {
        let (these, rest) = std::mem::take(&mut rows).split_at_mut(*n);
        line.decode_into(width, these, blank);
        rows = rest;
    }
}

/// Put `added` rows, written by `fill`, above the grid's history, keeping every line where it is
/// (lines are counted from the top of the screen) and the view on the text it shows. The grid
/// must have room for them.
fn prepend(grid: &mut Grid<Cell>, added: usize, fill: impl FnOnce(&mut [Row<Cell>])) {
    let screen = grid.screen_lines() as i32;
    let history = grid.history_size();
    let offset = grid.display_offset();
    // The grid grows by `added` rows, blank (cleared with the cursor's colours), at its bottom:
    // what was there moves up. Put them above the rest, and write them.
    grid.scroll_up::<Color>(&(Line(0)..Line(screen)), added);
    let top = -((history + added) as i32);
    let mut rows: Vec<Row<Cell>> = (top..screen).map(|l| std::mem::take(&mut grid[Line(l)])).collect();
    rows.rotate_right(added);
    fill(&mut rows[..added]);
    for (l, row) in (top..).zip(rows) {
        grid[Line(l)] = row;
    }
    let now = grid.display_offset();
    if now != offset {
        grid.scroll_display(Scroll::Delta(offset as i32 - now as i32));
    }
}

impl TermPane {
    /// Feed the emulator (see [`Feed`]).
    pub(super) fn parse(&mut self, bytes: &[u8]) {
        let now = Instant::now();
        if now.duration_since(self.archive.last) > CALM {
            // Output that comes after a pause is no flood any more.
            self.calm();
        }
        self.archive.last = now;
        self.archive.rested = false;
        // The parser (vte 0.15) drops a character that follows one cut by the end of the last
        // read, when the next holds the rest of it, that character, and one cut or invalid after:
        // the rest of the cut character goes to it alone.
        let need = self.cut.first().map_or(0, |b| utf8_len(*b).saturating_sub(self.cut.len()));
        let alone =
            if need > 0 && bytes.len() > need && bytes[..need].iter().all(|b| b & 0xc0 == 0x80) { need } else { 0 };
        let mut tail = std::mem::take(&mut self.cut);
        tail.extend_from_slice(&bytes[bytes.len().saturating_sub(3)..]);
        self.cut = tail.split_off(tail.len() - incomplete_char(&tail));
        let mut feed = Feed {
            term: &mut self.term,
            archive: &mut self.archive,
            seen: &mut self.commands.history_seen,
            lifted: &mut self.commands.lifted,
        };
        if alone > 0 {
            self.parser.advance(&mut feed, &bytes[..alone]);
        }
        self.parser.advance(&mut feed, &bytes[alone..]);
        self.parsed();
    }

    /// Give the emulator the output a synchronized update holds.
    pub(super) fn stop_sync(&mut self) {
        let mut feed = Feed {
            term: &mut self.term,
            archive: &mut self.archive,
            seen: &mut self.commands.history_seen,
            lifted: &mut self.commands.lifted,
        };
        self.parser.stop_sync(&mut feed);
        self.parsed();
    }

    /// Output was parsed: the view is brought back within the history, and rows that moved
    /// under it are told.
    fn parsed(&mut self) {
        keep_view_in_history(&mut self.term);
        self.view_moved();
    }

    /// Rows move on the grid: while the view shows history, what it shows may change without
    /// the emulator's damage saying so, and the screen is looked at whole again (see
    /// `ScreenCache::refresh`). At the bottom, the screen's rows stay where they are.
    fn view_moved(&mut self) {
        if self.term.grid().display_offset() > 0 {
            self.moved = self.moved.wrapping_add(1);
        }
    }

    /// The rows of the emulator's history that are its own: those below the lens.
    pub(super) fn own_history(&self) -> usize {
        self.term.grid().history_size() - self.archive.lens_rows()
    }

    /// Make the emulator's history hold at least `rows` rows, bringing rows back from the archive
    /// (a page at least). Whether any came back.
    pub(super) fn reveal(&mut self, rows: usize) -> bool {
        self.view_moved();
        let history = self.term.grid().history_size();
        if rows <= history {
            return false;
        }
        let wanted = (rows - history).max(PAGE);
        if self.archive.lens.as_ref().is_some_and(|l| l.end < l.last)
            && self.term.grid().display_offset() <= self.own_history()
        {
            // The view left the lens for the emulator's own rows: the lines between come first.
            self.archive.drop_lens(&mut self.term);
        }
        self.archive.lens_up(&mut self.term, wanted)
    }

    /// The same, the rows brought back the emulator's own (where the marks are found).
    pub(super) fn bring_back_rows(&mut self, rows: usize) {
        self.view_moved();
        self.fill_lens();
        let history = self.term.grid().history_size();
        if rows > history {
            let wanted = (rows - history).max(PAGE);
            self.archive.page_in(&mut self.term, &mut self.commands.history_seen, wanted, (0, 0));
        }
    }

    /// Make room for `rows` rows below `line` before going down: with a lens, the lines between
    /// it and the emulator's own come as they are reached.
    pub(super) fn reveal_below(&mut self, line: Line, rows: usize) {
        self.view_moved();
        self.archive.lens_down(&mut self.term, line.0, rows);
    }

    /// The same below the view.
    pub(super) fn reveal_below_view(&mut self, rows: usize) {
        let grid = self.term.grid();
        let bottom = Line(grid.screen_lines() as i32 - 1 - grid.display_offset() as i32);
        self.reveal_below(bottom, rows);
    }

    /// Whether `line` is past the lens: the lines between are still to come.
    pub(super) fn beyond_lens(&self, line: Line) -> bool {
        self.archive.lens.as_ref().is_some_and(|l| l.end < l.last) && line.0 >= -(self.own_history() as i32)
    }

    /// The view went up: the lines laid out far below it go (not while a search may have
    /// matched there).
    pub(super) fn went_up(&mut self, searching: bool) {
        self.view_moved();
        if !searching {
            self.archive.lens_trim_below(&mut self.term);
        }
    }

    /// Show the top of the history: its oldest line at the grid's top.
    pub(super) fn reveal_top(&mut self) {
        self.view_moved();
        self.archive.open_lens(&mut self.term, None);
    }

    /// Lay out all the lines between the lens and the emulator's own rows: the history is whole
    /// on the grid again, where it was.
    pub(super) fn fill_lens(&mut self) {
        self.view_moved();
        if self.archive.lens.is_some() {
            self.archive.lens_down(&mut self.term, i32::MIN, usize::MAX);
            self.archive.own_lens(&mut self.term, &mut self.commands.history_seen);
        }
    }

    /// Take the lens out if lines between it and the emulator's own are still to come: it
    /// returns the line it showed at the view's top, to show again (see
    /// [`TermPane::show_line`]).
    pub(super) fn close_lens(&mut self) -> Option<u64> {
        if self.archive.lens.as_ref().is_some_and(|l| l.end < l.last) { self.drop_lens() } else { None }
    }

    /// Take the lens out; returns the line it showed at the view's top, to show again (see
    /// [`TermPane::show_line`]).
    pub(super) fn drop_lens(&mut self) -> Option<u64> {
        self.view_moved();
        let lens = self.archive.lens.as_ref()?;
        let grid = self.term.grid();
        let mut above = grid.history_size().saturating_sub(grid.display_offset());
        let mut at = None;
        for (i, rows) in lens.sizes.iter().enumerate() {
            if above < *rows {
                at = Some(lens.start + i as u64);
                break;
            }
            above -= rows;
        }
        self.archive.drop_lens(&mut self.term);
        at
    }

    /// Show the archived line numbered `at` at the view's top, the copy-mode cursor on it.
    pub(super) fn show_line(&mut self, at: u64) {
        self.view_moved();
        if self.archive.open_lens(&mut self.term, Some(at)) {
            // The copy-mode cursor comes along into the view.
            self.term.scroll_display(Scroll::Top);
        }
    }

    /// Bring back the newest `lines` archived lines above the grid.
    pub(super) fn reveal_lines(&mut self, lines: usize) {
        self.view_moved();
        let width = self.term.columns();
        let rows: usize = {
            let store = self.archive.lines();
            let end = self.archive.lens.as_ref().map_or(store.span().end, |l| l.start);
            store.before(end).take(lines).map(|l| l.rows_at(width)).sum()
        };
        self.archive.lens_up(&mut self.term, rows);
    }

    /// How many of the archived lines above the grid to bring back for the nearest match of
    /// `regex` there: the newest line that matches when going to older lines, the oldest
    /// otherwise (going round to the top).
    pub(super) fn archived_match(&self, regex: &regex::Regex, older: bool) -> Option<usize> {
        let lines = self.archive.lines();
        let span = lines.span();
        let end = self.archive.lens.as_ref().map_or(span.end, |l| l.start);
        let hit = |line: history::Line| regex.is_match(&line.searched());
        if older {
            lines.before(end).position(hit).map(|i| i + 1)
        } else {
            let n = (end - span.start) as usize;
            lines.oldest_first().take(n).position(hit).map(|i| n - i)
        }
    }

    /// Move what nothing uses any more to the archive: rows brought back, once the view left
    /// them, and the lens.
    pub(super) fn settle(&mut self) {
        self.view_moved();
        if self.archive.lens.is_some() {
            let grid = self.term.grid();
            if grid.display_offset() > self.own_history() {
                return;
            }
            let lens = Line(-(grid.history_size() as i32))..Line(-(self.own_history() as i32));
            if self.term.selection.as_ref().is_some_and(|s| s.intersects_range(lens)) {
                // A selection stays on its rows: they are the emulator's own now, with those
                // between.
                self.fill_lens();
            } else {
                self.archive.drop_lens(&mut self.term);
            }
        }
        if self.term.grid().history_size() > self.archive.high && !self.archive.flood {
            let keep = self.archive.keep;
            let moved = self.archive.compact(&mut self.term, &mut self.commands.history_seen, keep);
            // Many rows brought back for a look at the history went again.
            self.archive.freed |= moved > EMULATOR_ROWS;
        }
    }

    /// The pane's output paused: after a flood, the history moves to the archive now.
    fn calm(&mut self) {
        if self.archive.flood {
            self.archive.flood = false;
            self.archive.window = (Instant::now(), 0);
            self.settle();
            self.archive.freed = true;
        }
    }

    /// The pane's output stopped: as [`TermPane::calm`], and the emulator keeps no history of
    /// its own until output comes again, nor the rows it allocates ahead for it. Whether memory
    /// was freed since last asked.
    pub(super) fn rest(&mut self) -> bool {
        self.calm();
        if !self.archive.rested && !self.term.mode().contains(TermMode::ALT_SCREEN) {
            self.archive.rested = true;
            self.view_moved();
            self.archive.compact(&mut self.term, &mut self.commands.history_seen, 0);
            self.term.grid_mut().truncate();
            self.archive.spare = Vec::new();
            self.archive.store.free_spare();
            self.archive.store.release();
            self.archive.freed = true;
        }
        std::mem::take(&mut self.archive.freed)
    }

    /// The screen changes size to `size`: rows it gains come from the history, brought back
    /// first. A wider screen takes fewer rows for the lines it wrapped, and the emulator fills it
    /// from its history as it rewraps them: those are brought back too, counted at the new width.
    /// A lens goes, to show the same line again after.
    pub(super) fn before_resize(&mut self, size: super::pty::Size) {
        self.view_moved();
        self.archive.reopen = self.drop_lens();
        let grid = self.term.grid();
        let (screen, history, width) = (grid.screen_lines(), grid.history_size(), grid.columns());
        let (rows, cols) = (usize::from(size.rows), usize::from(size.cols));
        let gained = rows.saturating_sub(screen);
        let wraps = |l: usize| grid[Line(l as i32)][Column(width - 1)].flags.contains(Flags::WRAPLINE);
        // The emulator's own history rewraps into fewer rows too: it is not counted on.
        let wider = if cols > width { gained + (0..screen).filter(|l| wraps(*l)).count() } else { 0 };
        if gained > history || wider > 0 {
            // Those rows only, not a page: every pane grows at once when the window does.
            let wanted = gained.saturating_sub(history);
            self.archive.page_in(&mut self.term, &mut self.commands.history_seen, wanted, (cols, wider));
        }
    }

    /// The archived lines are rewrapped only when they come back; their rows at the new width are
    /// counted then, and the oldest that no longer fit go (as the emulator's own do when it
    /// rewraps them).
    pub(super) fn after_resize(&mut self) {
        self.view_moved();
        self.archive.fit(self.term.screen_lines());
        self.archive.lines().set_width(self.term.columns());
        self.archive.spare.clear();
        match self.archive.reopen.take() {
            Some(at) => self.show_line(at),
            None => self.settle(),
        }
    }

    /// The text of the archive's last `rows` rows at the pane's width, as grid rows read.
    pub(super) fn archived_rows(&self, rows: usize) -> Vec<String> {
        if rows == 0 {
            // Without waiting for the rows handed to the store.
            return Vec::new();
        }
        let width = self.term.columns();
        let mut lines = Vec::new();
        let mut count = 0;
        for line in self.archive.lines().newest_first() {
            if count >= rows {
                break;
            }
            let mut texts = Vec::new();
            line.row_texts(width, &mut texts);
            count += texts.len();
            lines.push(texts);
        }
        let mut texts: Vec<String> = lines.into_iter().rev().flatten().collect();
        texts.drain(..texts.len().saturating_sub(rows));
        texts
    }
}

/// The emulator as the parser drives it, with room made in its history before output pushes rows
/// into it, and the archive cleared with it.
pub(super) struct Feed<'a, T> {
    pub(super) term: &'a mut Term<T>,
    pub(super) archive: &'a mut Archive,
    pub(super) seen: &'a mut i64,
    /// The primary screen's view as kraal lifted it to count the lines that scroll (see
    /// `commands.rs`).
    pub(super) lifted: &'a mut Option<(usize, bool)>,
}

impl<T: EventListener> Feed<'_, T> {
    #[inline]
    fn room(&mut self) {
        if self.term.grid().history_size() > self.archive.due {
            self.archive.make_room(self.term, self.seen);
        }
    }
}

/// Handler methods passed on as they are.
macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*);)*) => {
        $(
            #[inline]
            fn $name(&mut self, $($arg: $ty),*) {
                Handler::$name(&mut *self.term, $($arg),*)
            }
        )*
    };
}

/// Handler methods that may push rows into the history: room is made first.
macro_rules! pushing {
    ($($name:ident($($arg:ident: $ty:ty),*);)*) => {
        $(
            #[inline]
            fn $name(&mut self, $($arg: $ty),*) {
                self.room();
                Handler::$name(&mut *self.term, $($arg),*)
            }
        )*
    };
}

impl<T: EventListener> Handler for Feed<'_, T> {
    fn clear_screen(&mut self, mode: ClearMode) {
        match mode {
            ClearMode::All => self.room(),
            ClearMode::Saved if !self.term.mode().contains(TermMode::ALT_SCREEN) => {
                self.archive.lines().clear();
                self.archive.lens = None;
                self.term.clear_screen(mode);
                self.term.grid_mut().update_history(self.archive.limit);
                return;
            }
            _ => {}
        }
        mended(self.term, |t| t.clear_screen(mode));
    }

    fn reset_state(&mut self) {
        self.archive.lines().clear();
        self.archive.lens = None;
        self.term.reset_state();
        self.term.grid_mut().update_history(self.archive.limit);
    }

    fn set_private_mode(&mut self, mode: PrivateMode) {
        // The lens is laid out in the primary screen's history: it goes before the alternate
        // screen comes up, which has none. A view kraal lifted goes back down: the lines that
        // scroll are not counted on the alternate screen.
        if mode == PrivateMode::Named(NamedPrivateMode::SwapScreenAndSetRestoreCursor)
            && !self.term.mode().contains(TermMode::ALT_SCREEN)
        {
            self.archive.drop_lens(self.term);
            if let Some((_, true)) = self.lifted.take() {
                self.term.grid_mut().scroll_display(Scroll::Bottom);
            }
        }
        self.term.set_private_mode(mode);
    }

    fn delete_chars(&mut self, count: usize) {
        mended(self.term, |t| t.delete_chars(count));
    }

    fn insert_blank(&mut self, count: usize) {
        mended(self.term, |t| t.insert_blank(count));
    }

    fn erase_chars(&mut self, count: usize) {
        mended(self.term, |t| t.erase_chars(count));
    }

    fn clear_line(&mut self, mode: LineClearMode) {
        mended(self.term, |t| t.clear_line(mode));
    }

    #[inline]
    fn input(&mut self, c: char) {
        if zerowidth_full(self.term, c) {
            return;
        }
        // A character pushes a row only when it wraps: after the last column was written, or a
        // wide one at the last column.
        if self.term.grid().cursor.input_needs_wrap || c >= WIDE_FROM {
            self.room();
        }
        if self.term.mode().contains(TermMode::INSERT) {
            // The characters after it move right, as with `insert_blank`.
            mended(self.term, |t| t.input(c));
        } else {
            self.term.input(c);
        }
    }

    pushing! {
        linefeed();
        newline();
        scroll_up(rows: usize);
        delete_lines(rows: usize);
    }

    forward! {
        put_tab(count: u16);
        set_scrolling_region(top: usize, bottom: Option<usize>);
        scroll_down(rows: usize);
        insert_blank_lines(rows: usize);
        reverse_index();
        unset_private_mode(mode: PrivateMode);
        decaln();
        set_title(title: Option<String>);
        set_cursor_style(style: Option<CursorStyle>);
        set_cursor_shape(shape: CursorShape);
        goto(line: i32, col: usize);
        goto_line(line: i32);
        goto_col(col: usize);
        move_up(rows: usize);
        move_down(rows: usize);
        identify_terminal(intermediate: Option<char>);
        device_status(arg: usize);
        move_forward(cols: usize);
        move_backward(cols: usize);
        move_down_and_cr(rows: usize);
        move_up_and_cr(rows: usize);
        backspace();
        carriage_return();
        bell();
        substitute();
        set_horizontal_tabstop();
        move_backward_tabs(count: u16);
        move_forward_tabs(count: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_tabs(mode: TabulationClearMode);
        set_tabs(interval: u16);
        terminal_attribute(attr: Attr);
        set_mode(mode: Mode);
        unset_mode(mode: Mode);
        report_mode(mode: Mode);
        report_private_mode(mode: PrivateMode);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(index: CharsetIndex);
        configure_charset(index: CharsetIndex, charset: StandardCharset);
        set_color(index: usize, color: Rgb);
        dynamic_color_sequence(prefix: String, index: usize, terminator: &str);
        reset_color(index: usize);
        clipboard_store(clipboard: u8, data: &[u8]);
        clipboard_load(clipboard: u8, terminator: &str);
        push_title();
        pop_title();
        text_area_size_pixels();
        text_area_size_chars();
        set_hyperlink(link: Option<Hyperlink>);
        set_mouse_cursor_icon(icon: CursorIcon);
        report_keyboard_mode();
        push_keyboard_mode(mode: KeyboardModes);
        pop_keyboard_modes(count: u16);
        set_keyboard_mode(mode: KeyboardModes, behavior: KeyboardModesApplyBehavior);
        set_modify_other_keys(mode: ModifyOtherKeys);
        report_modify_other_keys();
        set_scp(char_path: ScpCharPath, update_mode: ScpUpdateMode);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use alacritty_terminal::term::Config as TermConfig;
    use alacritty_terminal::vi_mode::ViMotion;
    use alacritty_terminal::vte::ansi::Processor;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::super::pty::{Listener, Size};
    use super::super::screen::{lines_text, render_term};
    use super::super::{Pane, PaneEnv, PaneSpec};
    use super::*;

    const COLS: u16 = 30;
    const ROWS: u16 = 10;
    const SCROLLBACK: usize = 3000;

    /// A pane whose program prints nothing (the test feeds it), and an emulator that keeps its
    /// whole history, as a pane did before the archive, fed the same.
    struct Both {
        pane: Pane,
        reference: Term<Listener>,
        parser: Processor,
    }

    impl Both {
        fn new() -> Both {
            Both::sized(SCROLLBACK, ROWS)
        }

        /// A pane `rows` high that keeps `scrollback` rows of history.
        fn sized(scrollback: usize, rows: u16) -> Both {
            let (tx, _rx) = crate::server::event_channel(64);
            let env = PaneEnv {
                socket: "/nonexistent".into(),
                session: "t".into(),
                shell: "sh".into(),
                scrollback,
                graphics: Arc::new(crate::graphics::Shared::new(true, 64 << 20)),
            };
            let spec = PaneSpec { command: vec!["sleep".into(), "30".into()], ..Default::default() };
            let pane = Pane::spawn(1, None, spec, COLS, rows, &env, &tx).unwrap();
            let config = TermConfig { scrolling_history: scrollback, kitty_keyboard: true, ..TermConfig::default() };
            let reference = Term::new(config, &Size { cols: COLS, rows }, Listener::default());
            Both { pane, reference, parser: Processor::new() }
        }

        /// Feed both; then the pane's output stops (a flood goes to the archive).
        fn feed(&mut self, bytes: &[u8]) {
            self.pane.feed(bytes);
            self.pane.rest();
            self.parser.advance(&mut super::super::fuzz::Plain(&mut self.reference), bytes);
        }

        fn term(&self) -> &TermPane {
            self.pane.term().unwrap()
        }

        /// Rows of history the pane holds, both parts, at its width now.
        fn history(&mut self) -> usize {
            let t = self.pane.term_mut().unwrap();
            let width = t.term.columns();
            let archived: usize = t.archive.lines().oldest_first().map(|l| l.rows_at(width)).sum();
            assert_eq!(archived, t.archive.lines().rows());
            archived + t.own_history()
        }

        /// Rows the lens leaves out between its own and the emulator's: a line of the lens is
        /// that many rows further up in the history than on the grid.
        fn gap(&self) -> usize {
            let t = self.term();
            let Some(lens) = &t.archive.lens else { return 0 };
            let width = t.term.columns();
            t.archive.lines().from(lens.end).take((lens.last - lens.end) as usize).map(|l| l.rows_at(width)).sum()
        }

        /// The line of the history `line` of the grid shows, as the emulator alone has it.
        fn in_history(&self, line: Line) -> Line {
            let t = self.term();
            if t.archive.lens.is_some() && line.0 < -(t.own_history() as i32) {
                return line - self.gap();
            }
            line
        }

        /// How far up the view is in the history.
        fn view_offset(&self) -> usize {
            let top = -(self.pane.display_offset() as i32);
            -self.in_history(Line(top)).0 as usize
        }

        fn resize(&mut self, cols: u16, rows: u16) {
            self.pane.resize(cols, rows);
            self.reference.resize(Size { cols, rows });
        }

        /// The reference keeps the rows output pushes past the scrollback, as the pane does
        /// until the next move, rather than let go of them at once: views far up compare.
        fn room_past_the_scrollback(&mut self) {
            let limit = self.term().archive.limit;
            self.reference.grid_mut().update_history(limit + 2 * EMULATOR_ROWS);
        }
    }

    impl Drop for Both {
        fn drop(&mut self) {
            self.pane.kill();
        }
    }

    /// Lines of every kind: colours, wide characters, lines that wrap, links, coloured ends.
    fn sample(lines: usize) -> String {
        sample_of(lines, true)
    }

    /// The same, `fills` or not: the emulator rewraps a line whose end is coloured into more
    /// rows, where the archive colours the end of its new last row.
    fn sample_of(lines: usize, fills: bool) -> String {
        let mut s = String::new();
        for i in 0..lines {
            match i % 6 {
                0 => s.push_str(&format!("\x1b[3{}mcoloured {i}\x1b[0m plain\r\n", i % 8)),
                1 => s.push_str(&format!("日本語の行 {i} wide\r\n")),
                2 => s.push_str(&format!("{} long {i}\r\n", "x".repeat(70))),
                3 => s.push_str(&format!("\x1b]8;;https://example.org/{i}\x1b\\link {i}\x1b]8;;\x1b\\ after\r\n")),
                4 if fills => s.push_str(&format!("\x1b[44mbar {i}\x1b[K\x1b[0m\r\n")),
                _ => s.push_str(&format!("line {i}\r\n")),
            }
        }
        s
    }

    fn view(term: &Term<Listener>) -> Buffer {
        let area = Rect::new(0, 0, term.columns() as u16, term.screen_lines() as u16);
        let mut buf = Buffer::empty(area);
        let mut links = crate::ansi::Links::new();
        render_term(term, area, &mut buf, Some(&mut links));
        buf
    }

    /// The reference's last `rows` rows of history and its screen, as text.
    fn reference_text(r: &Term<Listener>, rows: usize) -> String {
        let bottom = super::super::screen::last_nonblank_line(r).unwrap_or(0) + 1;
        lines_text(r, -(rows as i32), bottom)
    }

    #[test]
    fn the_emulator_keeps_a_short_history_and_the_rest_reads_as_before() {
        let mut b = Both::new();
        b.feed(sample(4000).as_bytes());
        let t = b.term();
        assert!(t.term.grid().history_size() <= t.archive.high, "{}", t.term.grid().history_size());
        assert!(!t.archive.lines().is_empty());
        let kept = b.history();
        let high = b.term().archive.high;
        assert!((SCROLLBACK - high..=SCROLLBACK).contains(&kept), "{kept}");
        // `kraal read --lines`: the same text, through the archive.
        let all = b.pane.text(Some(kept + ROWS as usize));
        assert_eq!(all, reference_text(&b.reference, kept));
        let bottom = super::super::screen::last_nonblank_line(&b.reference).unwrap_or(0) + 1;
        assert_eq!(b.pane.text(Some(5)), lines_text(&b.reference, bottom - 5, bottom));
        // Lines kept in a few bytes each, against a row of 24-byte cells each.
        let t = b.term();
        let bytes = t.archive.lines().bytes();
        assert!(bytes < t.archive.lines().len() * 60, "{bytes} bytes for {} lines", t.archive.lines().len());
    }

    #[test]
    fn a_cell_keeps_a_bounded_number_of_combining_characters() {
        let mut b = Both::new();
        let combining: String = std::iter::repeat_n('\u{301}', 5000).collect();
        // A narrow character, then a wide one whose second half the cursor passed.
        b.feed(format!("e{combining}日{combining}x").as_bytes());
        let row = &b.term().term.grid()[Line(0)];
        for column in [0, 1] {
            assert_eq!(row[Column(column)].zerowidth().map(<[char]>::len), Some(16), "column {column}");
        }
        assert_eq!(row[Column(3)].c, 'x');
    }

    #[test]
    fn output_that_stops_leaves_the_whole_scrollback() {
        // Short and long scrollbacks, short and tall panes.
        for (scrollback, rows) in [(100, 24), (40, 24), (20, 24), (400, 50), (SCROLLBACK, ROWS)] {
            let mut b = Both::sized(scrollback, rows);
            let lines: String = (0..scrollback + 3 * usize::from(rows)).map(|i| format!("line {i}\r\n")).collect();
            b.feed(lines.as_bytes());
            assert_eq!(b.history(), scrollback, "{scrollback} rows of scrollback, {rows} high");
            let all = b.pane.text(Some(scrollback + usize::from(rows)));
            assert_eq!(all, b.reference_text_all(), "{scrollback} rows of scrollback, {rows} high");
        }
    }

    #[test]
    fn a_flood_that_stops_leaves_the_whole_scrollback() {
        let lines = |n: std::ops::Range<usize>| n.map(|i| format!("line {i}\r\n")).collect::<String>();
        let mut b = Both::new();
        b.feed(lines(0..500).as_bytes());
        // A flood (more rows within a second than the scrollback holds): once the emulator may
        // fill up, the archive's rows come back to it rather than go.
        b.pane.term_mut().unwrap().archive.flood = true;
        let flood = lines(500..3450);
        b.pane.feed(flood.as_bytes());
        b.parser.advance(&mut b.reference, flood.as_bytes());
        assert!(b.pane.rest());
        assert_eq!(b.history(), SCROLLBACK);
        assert_eq!(b.pane.text(Some(SCROLLBACK + ROWS as usize)), b.reference_text_all());
    }

    #[test]
    fn a_region_scrolling_under_a_view_scrolled_back_keeps_it_within_the_history() {
        let mut b = Both::new();
        b.feed(sample(100).as_bytes());
        b.pane.scroll(1);
        // The region below the top line scrolls: no row goes into the history.
        b.feed(format!("\x1b[2;{ROWS}r\x1b[{ROWS};1H{}", "\n".repeat(100)).as_bytes());
        assert!(b.pane.refresh_screen(COLS, ROWS).is_some());
        let grid = b.term().term.grid();
        assert!(grid.display_offset() <= grid.history_size(), "{} > {}", grid.display_offset(), grid.history_size());
    }

    /// What reads the screen (each tick for agents) or places an image, while the archive is
    /// busy (another thread reads it, the rows of a long output are stored), goes on at once.
    #[test]
    fn the_screen_and_images_are_read_without_the_archive() {
        let mut b = Both::new();
        b.feed(sample(1000).as_bytes());
        let store = b.term().archive.store.clone();
        let (held_tx, held_rx) = crossbeam_channel::bounded(0);
        let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(0);
        let reader = std::thread::spawn(move || {
            let lines = store.lines();
            held_tx.send(()).unwrap();
            let _ = done_rx.recv_timeout(Duration::from_secs(3));
            drop(lines);
        });
        held_rx.recv().unwrap();
        let start = Instant::now();
        assert!(b.pane.text(None).contains("line 995"));
        let screen = start.elapsed();
        b.pane.feed(b"\x1b_Ga=q,i=1;AAAA\x1b\\");
        let image = start.elapsed() - screen;
        let _ = done_tx.send(());
        reader.join().unwrap();
        assert!(screen < Duration::from_secs(1), "the screen waited {screen:?}");
        assert!(image < Duration::from_secs(1), "the image waited {image:?}");
    }

    #[test]
    fn a_full_screen_program_shown_while_the_view_is_far_back_leaves_the_history_as_it_was() {
        let mut b = Both::new();
        b.feed(sample(4000).as_bytes());
        let kept = b.history();
        let before = b.pane.text(Some(kept + ROWS as usize));
        b.pane.scroll(1500);
        assert!(b.term().archive.lens.is_some());
        b.feed(b"\x1b[?1049h\x1b[Hfull screen");
        // Its screen alone: the history is the other screen's.
        assert_eq!(b.pane.text(Some(50)), "full screen\n");
        b.resize(COLS + 3, ROWS + 2);
        b.resize(COLS, ROWS);
        b.feed(b"\x1b[?1049l");
        assert_eq!(b.history(), kept);
        assert_eq!(b.pane.text(Some(kept + ROWS as usize)), before);
    }

    #[test]
    fn a_flood_stays_in_the_emulator_until_it_stops() {
        let mut b = Both::new();
        b.feed(sample(1000).as_bytes());
        // More than the scrollback, at once: kept as the emulator alone keeps it.
        let flood = sample_of(6000, false);
        b.pane.feed(flood.as_bytes());
        b.parser.advance(&mut b.reference, flood.as_bytes());
        let t = b.term();
        assert!(t.archive.flood);
        assert_eq!(t.term.grid().history_size(), SCROLLBACK);
        assert!(t.archive.lines().is_empty(), "older than all the emulator holds");
        assert_eq!(b.pane.text(Some(SCROLLBACK + 20)), b.reference_text_all());
        // Typing meanwhile moves nothing.
        b.pane.write_input(Vec::new());
        assert_eq!(b.term().term.grid().history_size(), SCROLLBACK);
        // Once it stops, the history goes to the archive and reads the same.
        assert!(b.pane.rest());
        assert!(!b.pane.rest());
        let t = b.term();
        assert!(!t.archive.flood && t.term.grid().history_size() <= t.archive.high);
        let kept = b.history();
        assert_eq!(b.pane.text(Some(kept + ROWS as usize)), reference_text(&b.reference, kept));
        // Steady output, that the archive keeps up with, is no flood.
        for _ in 0..20 {
            b.pane.feed(sample(100).as_bytes());
            b.term().archive.lines();
        }
        assert!(!b.term().archive.flood);
    }

    #[test]
    fn scrolling_back_shows_what_the_emulator_showed() {
        let mut b = Both::new();
        b.room_past_the_scrollback();
        b.feed(sample(4000).as_bytes());
        let kept = b.history();
        // Line by line near the bottom, then page by page to the top, and back down.
        let mut offset = 0;
        for step in [1, 1, 3, 10, 200, 333, 1000, 5000] {
            b.pane.scroll(step);
            offset = (offset + step as usize).min(kept);
            b.reference.scroll_display(Scroll::Bottom);
            b.reference.scroll_display(Scroll::Delta(offset as i32));
            assert_eq!(b.view_offset(), offset);
            assert_eq!(view(&b.term().term), view(&b.reference), "{offset} rows up");
        }
        // Back at the bottom, the rows that came back go again (as many as the scrollback holds
        // with the emulator's rows: a batch of the oldest may go).
        b.pane.scroll_to_bottom();
        assert!(b.term().term.grid().history_size() <= b.term().archive.high);
        let kept = b.history();
        assert!(kept >= SCROLLBACK - b.term().archive.high);
        b.pane.scroll_to_top();
        b.reference.scroll_display(Scroll::Bottom);
        b.reference.scroll_display(Scroll::Delta(kept as i32));
        assert_eq!(view(&b.term().term), view(&b.reference));
        for _ in 0..5 {
            b.pane.scroll_page(false);
            b.reference.scroll_display(Scroll::PageDown);
            assert_eq!(view(&b.term().term), view(&b.reference));
        }
        // Output while the view is up keeps it on its text, and the history within its bounds.
        b.feed(sample(50).as_bytes());
        assert_eq!(view(&b.term().term), view(&b.reference));
        b.pane.scroll_to_bottom();
        b.feed(b"x\r\n");
        assert!(b.term().term.grid().history_size() <= b.term().archive.high);
    }

    #[test]
    fn search_goes_through_the_archive_and_round() {
        let mut b = Both::new();
        b.feed(b"needle first\r\n");
        b.feed(sample(900).as_bytes());
        b.feed(b"needle second\r\n");
        b.feed(sample(900).as_bytes());
        b.feed(b"Needle third\r\n");
        b.feed(sample(100).as_bytes());
        assert!(b.pane.search("needle"));
        assert_eq!(b.pane.selection_text().as_deref(), Some("Needle"), "smart case: lower case matches both");
        let line = |b: &Both| {
            let t = b.term();
            let sel = t.term.selection.as_ref().unwrap().to_range(&t.term).unwrap();
            super::super::screen::row_text(&t.term, sel.start.line)
        };
        assert_eq!(line(&b), "Needle third");
        assert!(b.pane.search_next(true));
        assert_eq!(line(&b), "needle second");
        assert!(b.pane.search_next(true));
        assert_eq!(line(&b), "needle first");
        // Round to the newest.
        assert!(b.pane.search_next(true));
        assert_eq!(line(&b), "Needle third");
        // And the other way, round to the oldest.
        assert!(b.pane.search_next(false));
        assert_eq!(line(&b), "needle first");
        assert!(b.pane.search_next(false));
        assert_eq!(line(&b), "needle second");
        assert!(b.pane.search("Needle"));
        assert!(b.pane.search_next(true));
        assert_eq!(line(&b), "Needle third", "capitals match exactly");
        assert!(!b.pane.search("haystack"));
    }

    #[test]
    fn copy_mode_selects_across_the_archive() {
        let mut b = Both::new();
        b.feed(b"the very first line\r\n");
        b.feed(sample(3500).as_bytes());
        b.pane.copy_mode(true);
        b.pane.vi_edge(true);
        let t = b.term();
        let first = super::super::screen::row_text(&t.term, t.term.vi_mode_cursor.point.line);
        // The first line went out of the scrollback; the oldest kept is the reference's.
        let kept = b.history();
        let top = super::super::screen::row_text(&b.reference, Line(-(kept as i32)));
        assert_eq!(first, top);
        // Laid out alone: a few screens, not the whole history.
        assert!(b.term().archive.lens.is_some());
        assert!(b.term().term.grid().history_size() < 1000);
        // From there, down past what the lens and the emulator held before, selected line by line.
        b.pane.vi_select(alacritty_terminal::selection::SelectionType::Lines);
        for _ in 0..2000 {
            b.pane.vi_motion(ViMotion::Down);
        }
        let selected = b.pane.selection_text().unwrap();
        // The same selection in the emulator that kept everything reads the same: the grid's top
        // is the oldest line there too.
        let t = b.term();
        let history = t.term.grid().history_size() as i32;
        let selection = t.term.selection.clone().unwrap();
        let range = selection.to_range(&t.term).unwrap();
        assert_eq!(range.start.line, t.term.topmost_line());
        assert!(range.end.line >= t.term.topmost_line() + 2000);
        let all = Line(-history.max(kept as i32))..Line(ROWS as i32);
        b.reference.selection = selection.rotate(&t.term, &all, kept as i32 - history);
        let expected = b.reference.selection_to_string().unwrap();
        let differ = selected.lines().zip(expected.lines()).position(|(a, b)| a != b);
        assert_eq!(differ, None, "{:?}", differ.map(|i| (selected.lines().nth(i), expected.lines().nth(i))));
        assert_eq!(selected, expected);
        assert!(selected.lines().count() > 1500);
        // Up again, a line at a time, beyond the top of the emulator's part.
        b.pane.vi_select(alacritty_terminal::selection::SelectionType::Lines);
        b.pane.copy_mode(false);
        b.pane.copy_mode(true);
        for _ in 0..1500 {
            b.pane.vi_motion(ViMotion::Up);
        }
        let t = b.term();
        let at = t.term.vi_mode_cursor.point.line;
        let read = super::super::screen::row_text;
        assert_eq!(read(&t.term, at), read(&b.reference, b.in_history(at)));
        b.pane.copy_mode(false);
        assert!(b.term().term.grid().history_size() <= b.term().archive.high, "given back when done");
    }

    /// A whole command as zsh with kraal's integration prints it.
    fn command(line: &str, output: &str, status: i32) -> String {
        format!("\x1b]133;A\x07$ \x1b]133;B\x07{line}\r\n\x1b]133;C\x07{output}\x1b]133;D;{status}\x07")
    }

    #[test]
    fn commands_and_their_output_are_found_in_the_archive() {
        let mut b = Both::new();
        b.feed(command("first", "one\r\n", 0).as_bytes());
        let long = sample(2000);
        b.feed(command("long", &long, 0).as_bytes());
        b.feed(command("last", "z\r\n", 0).as_bytes());
        b.feed(b"\x1b]133;A\x07$ ");
        b.pane.copy_mode(true);
        let cursor = |b: &Both| {
            let t = b.term();
            super::super::screen::row_text(&t.term, t.term.vi_mode_cursor.point.line)
        };
        // From the top of the history, laid out alone: the marks are found on the whole of it.
        b.pane.vi_edge(true);
        assert!(b.term().archive.lens.is_some());
        assert_eq!(cursor(&b), "$ first");
        assert!(b.pane.jump_to_prompt(false));
        assert_eq!(cursor(&b), "$ long");
        assert!(b.term().archive.lens.is_none());
        b.pane.copy_mode(false);
        b.pane.copy_mode(true);
        assert!(b.pane.jump_to_prompt(true));
        assert_eq!(cursor(&b), "$ last");
        assert!(b.pane.jump_to_prompt(true));
        assert_eq!(cursor(&b), "$ long");
        assert!(b.pane.jump_to_prompt(true));
        assert_eq!(cursor(&b), "$ first");
        assert!(!b.pane.jump_to_prompt(true));
        assert!(b.pane.jump_to_prompt(false));
        assert_eq!(cursor(&b), "$ long");
        // The long output, whole, though most of it left the emulator: what the same selection
        // reads from the emulator that kept everything.
        assert!(b.pane.select_command(false));
        let selected = b.pane.selection_text().unwrap();
        b.reference.selection = b.term().term.selection.clone();
        assert_eq!(Some(selected.clone()), b.reference.selection_to_string());
        assert_eq!(selected.lines().count(), 2000);
        assert_eq!(selected.lines().next(), Some("coloured 0 plain"));
        b.pane.copy_mode(false);
        b.feed(command("again", &sample(1500), 0).as_bytes());
        // Read while the view is far back: it stays there.
        b.pane.copy_mode(true);
        b.pane.vi_edge(true);
        assert!(b.term().archive.lens.is_some());
        let top = |b: &Both| {
            let t = b.term();
            super::super::screen::row_text(&t.term, Line(-(t.term.grid().display_offset() as i32)))
        };
        let shown = top(&b);
        let (text, output) = b.pane.last_command_output().unwrap();
        assert_eq!(top(&b), shown);
        assert_eq!(text.as_deref(), Some("again"));
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 1500, "wrapped rows read as their line");
        assert_eq!(lines[0], "coloured 0 plain");
        assert_eq!(lines[1499], "line 1499");
    }

    #[test]
    fn the_top_of_a_long_history_is_laid_out_alone() {
        let mut b = Both::new();
        b.room_past_the_scrollback();
        b.feed(sample(4000).as_bytes());
        let kept = b.history();
        let read = b.pane.text(Some(kept + ROWS as usize));
        let saved = b.pane.scrollback_ansi().unwrap();
        b.pane.copy_mode(true);
        b.pane.vi_edge(true);
        assert!(b.term().archive.lens.is_some());
        assert!(b.term().term.grid().history_size() < 1000);
        b.reference.scroll_display(Scroll::Delta(kept as i32));
        assert_eq!(view(&b.term().term), view(&b.reference));
        // What reads the history reads it whole meanwhile.
        assert_eq!(b.pane.text(Some(kept + ROWS as usize)), read);
        assert_eq!(b.pane.scrollback_ansi().unwrap(), saved);
        assert_eq!(b.history(), kept);
        // Output keeps the view on its text, and the rows it pushed past the scrollback until
        // the next move.
        b.feed(b"one more\r\nand another\r\n");
        assert_eq!(view(&b.term().term), view(&b.reference));
        assert_eq!(b.history(), kept + 2);
        for _ in 0..3 {
            b.pane.scroll_page(false);
            b.reference.scroll_display(Scroll::PageDown);
        }
        assert_eq!(view(&b.term().term), view(&b.reference));
        // A new height: the same line at the top of the view.
        let top = |b: &Both| {
            let t = b.term();
            super::super::screen::row_text(&t.term, Line(-(t.term.grid().display_offset() as i32)))
        };
        let shown = top(&b);
        b.resize(COLS, ROWS + 4);
        assert!(b.term().archive.lens.is_some());
        assert_eq!(top(&b), shown);
        // Up from there, to the oldest line kept, as the emulator alone shows it.
        for _ in 0..100 {
            b.pane.scroll(3);
        }
        let t = b.term();
        assert_eq!(t.term.grid().display_offset(), t.term.grid().history_size());
        let kept = b.history();
        b.reference.scroll_display(Scroll::Bottom);
        b.reference.scroll_display(Scroll::Delta(kept as i32));
        assert_eq!(view(&b.term().term), view(&b.reference));
        // A search that finds nothing leaves the view far back; one that does takes it there.
        let before = view(&b.term().term);
        assert!(!b.pane.search("nowhere to be found"));
        assert_eq!(view(&b.term().term), before);
        assert!(b.pane.search("line 3"));
        let t = b.term();
        let at = t.term.vi_mode_cursor.point.line;
        assert!(super::super::screen::row_text(&t.term, at).starts_with("line 3"));
        // Leaving copy mode, the lens goes.
        b.pane.copy_mode(false);
        let t = b.term();
        assert!(t.archive.lens.is_none());
        assert!(t.term.grid().history_size() <= t.archive.high);
        assert!((kept - 20..=SCROLLBACK).contains(&b.history()));
        // Down from the top in copy mode, a line at a time: the lens moves along.
        b.pane.copy_mode(true);
        b.pane.vi_edge(true);
        for _ in 0..1000 {
            b.pane.vi_motion(ViMotion::Down);
        }
        let kept = b.history();
        let t = b.term();
        let lens = t.archive.lens.as_ref().unwrap();
        assert!(lens.rows < 1000, "{} rows", lens.rows);
        let width = t.term.columns();
        let first = t.archive.lines().span().start;
        let above: usize =
            t.archive.lines().from(first).take((lens.start - first) as usize).map(|l| l.rows_at(width)).sum();
        let line = t.term.vi_mode_cursor.point.line;
        let row = line.0 + t.term.grid().history_size() as i32;
        let reference = Line(row + above as i32 - kept as i32);
        let read = |term: &Term<Listener>, l: Line| super::super::screen::row_text(term, l);
        assert_eq!(read(&t.term, line), read(&b.reference, reference));
        // The view the same rows above the cursor.
        let offset = t.term.grid().display_offset() as i32 + line.0 - reference.0;
        let ours = view(&t.term);
        b.reference.scroll_display(Scroll::Bottom);
        b.reference.scroll_display(Scroll::Delta(offset));
        assert_eq!(ours, view(&b.reference));
    }

    #[test]
    fn a_selection_far_back_stays_when_the_view_comes_down() {
        let mut b = Both::new();
        b.feed(sample(4000).as_bytes());
        b.pane.scroll(1500);
        assert!(b.term().archive.lens.is_some());
        b.pane.selection_start(0, 2, alacritty_terminal::selection::SelectionType::Simple);
        b.pane.selection_update(20, 5);
        let selected = b.pane.selection_text().unwrap();
        b.pane.scroll_to_bottom();
        assert_eq!(b.pane.selection_text(), Some(selected));
        // Once it goes, so do the rows it kept.
        b.pane.clear_selection();
        b.pane.scroll_to_bottom();
        let t = b.term();
        assert!(t.archive.lens.is_none() && t.term.grid().history_size() <= t.archive.high);
    }

    #[test]
    fn saved_text_holds_the_archive_and_comes_back() {
        let mut b = Both::new();
        b.feed(sample(4000).as_bytes());
        let saved = b.pane.scrollback_ansi().unwrap();
        let kept = b.history();
        let reference = super::super::scrollback::encode(&b.reference, 0, &history::History::default(), usize::MAX);
        // The same lines as the emulator alone would save, the oldest it kept first.
        let reference_lines: Vec<&[u8]> = reference.split(|c| *c == b'\n').collect();
        let saved_lines: Vec<&[u8]> = saved.split(|c| *c == b'\n').collect();
        assert!(saved_lines.len() >= kept / 2, "{} lines", saved_lines.len());
        assert_eq!(saved_lines[..], reference_lines[reference_lines.len() - saved_lines.len()..]);
        // A restart gives it back, through the archive.
        let mut fresh = Both::new();
        assert!(fresh.pane.restore_scrollback(&saved, None, SCROLLBACK));
        let text = fresh.pane.text(Some(SCROLLBACK + 20));
        let before = b.pane.text(Some(SCROLLBACK + 20));
        let restored: Vec<&str> = text.lines().collect();
        assert_eq!(restored[restored.len() - 1], "kraal: restored");
        let old: Vec<&str> = before.lines().collect();
        let tail = &restored[..restored.len() - 1];
        assert_eq!(tail[tail.len() - 100..], old[old.len() - 100..]);
        assert!(!fresh.term().archive.lines().is_empty());
    }

    #[test]
    fn clearing_the_history_clears_the_archive() {
        let mut b = Both::new();
        b.feed(sample(2000).as_bytes());
        assert!(!b.term().archive.lines().is_empty());
        b.feed(b"\x1b[H\x1b[2J\x1b[3J");
        assert!(b.term().archive.lines().is_empty());
        assert_eq!(b.pane.text(Some(100)), b.reference_text_all());
        b.feed(sample(2000).as_bytes());
        b.feed(b"\x1bc");
        assert!(b.term().archive.lines().is_empty());
        // Not from a full-screen program: its screen has no history.
        b.feed(sample(2000).as_bytes());
        b.feed(b"\x1b[?1049h\x1b[3J\x1b[?1049l");
        assert!(!b.term().archive.lines().is_empty());
    }

    impl Both {
        fn reference_text_all(&self) -> String {
            let bottom = super::super::screen::last_nonblank_line(&self.reference).unwrap_or(0) + 1;
            lines_text(&self.reference, -(self.reference.grid().history_size() as i32), bottom)
        }
    }

    #[test]
    fn a_new_width_rewraps_the_archive_when_it_is_read() {
        let mut b = Both::new();
        b.feed(sample_of(4000, false).as_bytes());
        for (cols, rows) in [(17, 10), (55, 14), (30, 6), (30, 10)] {
            b.resize(cols, rows);
            let t = b.term();
            assert!(t.term.grid().history_size() <= t.archive.high);
            let kept = b.history();
            let ours = b.pane.text(Some(kept + rows as usize));
            let reference = b.reference_text_all();
            // The emulator alone dropped its oldest rows as narrow rows took more: the newest agree.
            let ours: Vec<&str> = ours.lines().collect();
            let reference: Vec<&str> = reference.lines().collect();
            // The emulator alone may have kept the end of a line whose start it dropped; the
            // archive drops whole lines.
            let n = ours.len().min(reference.len()) - 1;
            assert!(n > 1000);
            assert_eq!(ours[ours.len() - n..], reference[reference.len() - n..], "{cols}x{rows}");
        }
        // Scrolled to the top, the rows come back at the width now.
        b.pane.scroll_to_top();
        let t = b.term();
        let top = t.term.topmost_line();
        let first = super::super::screen::row_text(&t.term, top);
        assert!(!first.is_empty());
    }

    #[test]
    fn the_emulator_is_driven_as_the_parser_drives_it_alone() {
        // Every kind of sequence, so that none is lost on the way.
        let corpus: &[u8] = b"\x1b]0;title\x07\x1b]2;other\x1b\\plain \x1b[1;3;4;9;31;42mstyled\x1b[0m\r\n\
            \x1b[5;10Hat\x1b[2;3fthere\x1b[A\x1b[B\x1b[C\x1b[D\x1b[2E\x1b[F\x1b[5G\x1b[3d\
            tab\tstop\x1bH\x1b[2g\x1b[3g\x1b[I\x1b[Z\x08\r\
            \x1b[2@\x1b[3P\x1b[4X\x1b[K\x1b[1K\x1b[2K\x1b[J\x1b[1J\
            \x1b[2;8r\x1b[3L\x1b[2M\x1b[2S\x1b[3T\x1bD\x1bE\x1bM\x1b[r\
            \x1b7\x1b[10;10H\x1b8\x1b[s\x1b[u\x1b#8\
            \x1b[4hins\x1b[4l\x1b[20h\n\x1b[20l\x1b[?6h\x1b[?6l\x1b[?7l\x1b[?7h\x1b[?25l\x1b[?25h\
            \x1b[?1h\x1b=\x1b[?1l\x1b>\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1004h\
            \x1b(0qqq\x1b(B\x1b)0\x0eqq\x0f\
            \x1b]4;1;rgb:12/34/56\x07\x1b]104;1\x07\x1b]8;id=x;https://a.b\x1b\\ln\x1b]8;;\x1b\\\
            \x1b]52;c;aGk=\x07\x1b[22;0t\x1b[23;0t\x1b[6n\x1b[c\x1b[>c\x1b[5n\x1b[18t\x1b[14t\
            \x1b[3 q\x1b[?1049hALT\x1b[?1049l\x1b[>1u\x1b[?u\x1b[=3;1u\x1b[<u\x1b[>4;2m\x1b[?4m\
            \x1b[?2026h\x1b[?2026l\x1b[?1$p\x1b[4$p\x1b]10;?\x07\x1b]11;?\x07\x1b]12;#ff0000\x07\
            end\x1b[8;30H\xe6\x97\xa5\xe6\x9c\xac\x1b[1J\x1bc";
        let events = Rc::new(RefCell::new(Vec::new()));
        let size = Size { cols: COLS, rows: ROWS };
        let config = TermConfig { scrolling_history: 100, kitty_keyboard: true, ..TermConfig::default() };
        let mut plain = Term::new(config.clone(), &size, Listener(events.clone()));
        let fed_events = Rc::new(RefCell::new(Vec::new()));
        let mut fed = Term::new(config, &size, Listener(fed_events.clone()));
        let mut archive = Archive::new(100, ROWS as usize);
        let mut seen = 0;
        // Byte by byte through the archive, in one piece alone, and compared after each piece.
        let (mut p1, mut p2): (Processor, Processor) = (Processor::new(), Processor::new());
        for piece in corpus.split_inclusive(|b| *b == b'\x07' || *b == b'\\' || *b == b'm') {
            p1.advance(&mut plain, piece);
            for byte in piece {
                let lifted = &mut None;
                p2.advance(&mut Feed { term: &mut fed, archive: &mut archive, seen: &mut seen, lifted }, &[*byte]);
            }
            for l in -(plain.grid().history_size() as i32)..ROWS as i32 {
                assert_eq!(
                    plain.grid()[Line(l)],
                    fed.grid()[Line(l)],
                    "line {l} after {:?}",
                    String::from_utf8_lossy(piece)
                );
            }
            assert_eq!(plain.mode(), fed.mode());
            assert_eq!(plain.grid().cursor.point, fed.grid().cursor.point);
            assert_eq!(format!("{:?}", plain.colors()[1]), format!("{:?}", fed.colors()[1]));
        }
        let describe = |e: &Rc<RefCell<Vec<alacritty_terminal::event::Event>>>| {
            e.borrow().iter().map(|e| format!("{e:?}")).collect::<Vec<_>>()
        };
        assert_eq!(describe(&events), describe(&fed_events));
        assert!(!events.borrow().is_empty());
    }

    /// Output of every kind, at random: text in colours, wide and combining characters, tabs,
    /// erasing, inserting and deleting characters and lines, cursor moves, insert mode, scroll
    /// regions, the alternate screen. The history reads as the emulator's alone, cell by cell:
    /// a row is never cut short on its way to the archive.
    #[test]
    fn rows_written_any_way_keep_all_their_cells() {
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        for round in 0..6 {
            let mut b = Both::new();
            let mut out = String::new();
            for _ in 0..12_000 {
                let n = next(100);
                match n {
                    0..40 => {
                        let len = next(40) as usize;
                        out.push_str(&format!("\x1b[3{}m{}", next(8), "abcdefghijk ".repeat(4).get(..len).unwrap()));
                    }
                    40..55 => out.push_str("\r\n"),
                    55..58 => out.push_str("\x1b[44m"),
                    58..62 => out.push_str("\x1b[0m"),
                    62..64 => out.push_str(["\x1b[K", "\x1b[1K", "\x1b[2K"][next(3) as usize]),
                    64..66 => out.push_str(&format!("\x1b[{}X", next(50))),
                    66..67 => out.push_str(&format!("\x1b[{}@", next(5))),
                    67..68 => out.push_str(&format!("\x1b[{}P", next(5))),
                    68..70 => out.push('\t'),
                    70..72 => out.push_str("日本\u{301}x"),
                    72..74 => out.push_str(&format!("\x1b[{};{}H", next(12) + 1, next(35) + 1)),
                    74..76 => out.push_str(["\x1b[A", "\x1b[B", "\x1b[5C", "\x1b[3D", "\r"][next(5) as usize]),
                    76..77 => out.push_str(["\x1b[4h", "\x1b[4l"][next(2) as usize]),
                    77..78 => out.push_str(["\x1b[2;8r", "\x1b[1;5r", "\x1b[r"][next(3) as usize]),
                    78..79 => {
                        out.push_str(["\x1b[2S", "\x1b[T", "\x1b[2L", "\x1b[M", "\x1bM", "\x1bD"][next(6) as usize])
                    }
                    79..80 => out.push_str(["\x1b[J", "\x1b[1J", "\x1b[2J"][next(3) as usize]),
                    80..81 if next(10) == 0 => out.push_str(["\x1b[?1049h", "\x1b[?1049l"][next(2) as usize]),
                    81..82 if next(50) == 0 => out.push_str("\x1b#8"),
                    _ => out.push_str(&format!("line {n} \x1b[1mbold\x1b[0m\r\n")),
                }
            }
            out.push_str("\x1b[?1049l\x1b[r\x1b[4l\x1b[0m");
            for chunk in out.as_bytes().chunks(997) {
                b.feed(chunk);
            }
            let kept = b.history();
            assert!(!b.term().archive.lines().is_empty(), "round {round}");
            eprintln!(
                "kept {kept} emulator {} lines {} reference {}",
                b.term().term.grid().history_size(),
                b.term().archive.lines().len(),
                b.reference.grid().history_size()
            );
            // Every row, from the top down a page at a time, then through views scrolled up from
            // the bottom.
            b.pane.scroll_to_top();
            let mut offset = kept;
            loop {
                b.reference.scroll_display(Scroll::Bottom);
                b.reference.scroll_display(Scroll::Delta(offset as i32));
                assert_eq!(view(&b.term().term), view(&b.reference), "round {round}, {offset} rows up, going down");
                if offset == 0 {
                    break;
                }
                let step = (ROWS as usize - 1).min(offset);
                b.pane.scroll(-(step as i32));
                offset -= step;
            }
            assert!(b.term().archive.lens.is_none());
            for offset in (0..=kept).rev().step_by(ROWS as usize - 1) {
                b.pane.scroll_to_bottom();
                b.pane.scroll(offset as i32);
                b.reference.scroll_display(Scroll::Bottom);
                b.reference.scroll_display(Scroll::Delta(offset as i32));
                assert_eq!(view(&b.term().term), view(&b.reference), "round {round}, {offset} rows up");
            }
        }
    }

    /// What resizing a pane with a full archive, and browsing a long history, cost: `cargo test
    /// --release -- --ignored --nocapture browse_cost`.
    #[test]
    #[ignore]
    fn browse_cost() {
        let mut b = Both::new();
        b.feed(sample(4000).as_bytes());
        let start = std::time::Instant::now();
        for i in 0..40 {
            let (cols, rows) = if i % 2 == 0 { (40, 11) } else { (42, 12) };
            b.pane.resize(cols, rows);
        }
        eprintln!("resize: {:?} each", start.elapsed() / 40);
        // A wide pane, a long history, pages up first (as the compare harness does).
        let mut b = Both::new();
        b.resize(169, 47);
        let log: String = (0..100_000)
            .map(|i| {
                format!("\x1b[2m00:00:{:02}.{:03}\x1b[0m \x1b[32mINFO\x1b[0m p00 req={i:06} ok\r\n", i % 60, i % 1000)
            })
            .collect();
        let t = b.pane.term_mut().unwrap();
        t.archive.limit = 100_000;
        t.term.grid_mut().update_history(100_000);
        t.parse(log.as_bytes());
        t.rest();
        // Once the archive has them all, and gave the memory of the rows back.
        drop(t.archive.lines());
        std::thread::sleep(std::time::Duration::from_millis(200));
        b.pane.copy_mode(true);
        let start = std::time::Instant::now();
        for _ in 0..60 {
            b.pane.scroll_page(true);
        }
        eprintln!("page up: {:?} each", start.elapsed() / 60);
        let start = std::time::Instant::now();
        b.pane.vi_edge(true);
        eprintln!("top of 100k lines: {:?}", start.elapsed());
    }

    /// What the archive costs on bulk output: `cargo test --release -- --ignored --nocapture
    /// archive_cost`.
    #[test]
    #[ignore]
    fn archive_cost() {
        let size = Size { cols: 169, rows: 47 };
        let config = TermConfig { scrolling_history: 10_000, ..TermConfig::default() };
        let seq: Vec<u8> = (1..=3_000_000).flat_map(|i| format!("{i}\r\n").into_bytes()).collect();
        let log: Vec<u8> = (0..300_000)
            .flat_map(|i| {
                format!("\x1b[2m00:00:{:02}.{:03}\x1b[0m \x1b[32mINFO\x1b[0m p00 req={i:06} ok\r\n", i % 60, i % 1000)
                    .into_bytes()
            })
            .collect();
        // As the compare harness writes its coloured file: a colour every word, long lines.
        let words =
            ["error", "warning:", "Compiling", "src/server/render.rs:412:9", "fn", "0x7ffd3a2c", "Ok(())", "αβγ"];
        let mut colours = Vec::new();
        let mut n: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            n ^= n << 13;
            n ^= n >> 7;
            n ^= n << 17;
            n
        };
        while colours.len() < 50_000_000 {
            let len = 100 + (next() % 500) as usize;
            let mut visible = 0;
            while visible < len {
                match next() % 6 {
                    0 => colours.extend_from_slice(format!("\x1b[38;5;{}m", next() % 256).as_bytes()),
                    1 => colours.extend_from_slice(format!("\x1b[38;2;{};{};{}m", next() % 256, 7, 9).as_bytes()),
                    2 => colours.extend_from_slice(format!("\x1b[1;3{}m", next() % 8).as_bytes()),
                    3 => colours.extend_from_slice(b"\x1b[0m"),
                    4 => colours.extend_from_slice(format!("\x1b[48;5;{}m", 232 + next() % 24).as_bytes()),
                    _ => {}
                }
                let w = words[(next() % words.len() as u64) as usize];
                colours.extend_from_slice(w.as_bytes());
                colours.push(b' ');
                visible += w.chars().count() + 1;
            }
            colours.extend_from_slice(b"\x1b[0m\r\n");
        }
        for (name, input) in [("seq 3M", &seq), ("log 300k", &log), ("colours 50 MB", &colours)] {
            // Plain, flooded, and output too slow to be a flood (all of it moves to the archive).
            for (how, archived, slow) in [("plain", false, false), ("flood", true, false), ("steady", true, true)] {
                let mut term = Term::new(config.clone(), &size, Listener::default());
                let mut archive = Archive::new(10_000, 47);
                let mut seen = 0;
                let mut parser: Processor = Processor::new();
                let start = std::time::Instant::now();
                for chunk in input.chunks(64 << 10) {
                    if slow {
                        archive.window = (std::time::Instant::now(), 0);
                    }
                    if archived {
                        let lifted = &mut None;
                        parser.advance(
                            &mut Feed { term: &mut term, archive: &mut archive, seen: &mut seen, lifted },
                            chunk,
                        );
                    } else {
                        parser.advance(&mut term, chunk);
                    }
                }
                let took = start.elapsed();
                eprintln!(
                    "{name}: {how} {took:?}, emulator history {}, archive {} lines",
                    term.grid().history_size(),
                    archive.lines().len()
                );
            }
        }
    }
}
