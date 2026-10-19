//! Randomized tests of a pane fed any output (see `crate::fuzz`): nothing panics or hangs, the
//! history stays within the scrollback, the screen drawn from the cache is the screen, and the
//! emulator shows what alacritty alone shows when kraal passes the output on.

use std::sync::Arc;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vi_mode::ViMotion;
use alacritty_terminal::vte::ansi::Processor;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::pty::{Listener, Size};
use super::screen::{render_term, row_text};
use super::*;
use crate::fuzz::{self, Rng};

struct Fixture {
    pane: Pane,
    env: PaneEnv,
    tx: crate::server::EventSender,
    _rx: crate::server::EventReceiver,
}

/// A pane whose program is never started: what it is fed is all it prints.
fn fixture(cols: u16, rows: u16, scrollback: usize, graphics: Arc<crate::graphics::Shared>) -> Fixture {
    let (tx, rx) = crate::server::event_channel(4096);
    let env = PaneEnv { socket: "/nonexistent".into(), session: "t".into(), shell: "sh".into(), scrollback, graphics };
    let spec = PaneSpec { command: vec!["true".into()], ..Default::default() };
    let batch = super::spawn::batch();
    let pane = Pane::spawn(1, None, spec, cols, rows, &env, &tx).unwrap();
    drop(batch);
    Fixture { pane, env, tx, _rx: rx }
}

fn graphics(rng: &mut Rng) -> Arc<crate::graphics::Shared> {
    let shared = Arc::new(crate::graphics::Shared::new(!rng.one_in(8), rng.pick(&[1 << 20, 64 << 20]).to_owned()));
    shared.set_available(!rng.one_in(4));
    shared.set_cell(rng.range(1, 30) as u16, rng.range(1, 40) as u16);
    shared
}

const MOTIONS: [ViMotion; 21] = [
    ViMotion::Up,
    ViMotion::Down,
    ViMotion::Left,
    ViMotion::Right,
    ViMotion::First,
    ViMotion::Last,
    ViMotion::FirstOccupied,
    ViMotion::High,
    ViMotion::Middle,
    ViMotion::Low,
    ViMotion::SemanticLeft,
    ViMotion::SemanticRight,
    ViMotion::SemanticLeftEnd,
    ViMotion::SemanticRightEnd,
    ViMotion::WordLeft,
    ViMotion::WordRight,
    ViMotion::WordLeftEnd,
    ViMotion::WordRightEnd,
    ViMotion::Bracket,
    ViMotion::ParagraphUp,
    ViMotion::ParagraphDown,
];

const SELECTIONS: [SelectionType; 4] =
    [SelectionType::Simple, SelectionType::Block, SelectionType::Semantic, SelectionType::Lines];

/// A size panes take, small ones often.
fn size(rng: &mut Rng) -> (u16, u16) {
    match rng.below(4) {
        0 => (rng.range(0, 4) as u16, rng.range(0, 3) as u16),
        1 => (rng.range(2, 300) as u16, rng.range(1, 120) as u16),
        _ => (rng.range(5, 90) as u16, rng.range(2, 30) as u16),
    }
}

/// What a client does with the pane, between reads of its output.
fn act(rng: &mut Rng, f: &mut Fixture, recent: &[u8]) {
    let p = &mut f.pane;
    let action = rng.below(30);
    let t = p.term().unwrap();
    fuzz::trace(|| {
        let g = t.term.grid();
        format!(
            "act {action} on {}x{} history {} offset {} vi {:?} lens {}",
            p.cols,
            p.rows,
            g.history_size(),
            g.display_offset(),
            t.term.vi_mode_cursor.point,
            t.archive.lens_rows()
        )
    });
    match action {
        0 => {
            let (cols, rows) = size(rng);
            p.resize(cols, rows);
        }
        1 => {
            p.rest();
        }
        2 => p.scroll(rng.range(0, 60) as i32 - 30),
        // Farther than kraal ever scrolls at once (lines, pages, halves of the screen).
        3 => p.scroll((rng.number() % 200_000) as i32 - 100_000),
        4 => p.scroll_page(rng.one_in(2)),
        5 => {
            if rng.one_in(2) {
                p.scroll_to_top()
            } else {
                p.scroll_to_bottom()
            }
        }
        6 => p.copy_mode(!p.in_copy_mode()),
        7..10 => {
            p.copy_mode(true);
            for _ in 0..rng.range(1, 20) {
                let motion = *rng.pick(&MOTIONS);
                fuzz::trace(|| format!("  {motion:?} from {:?}", p.term().unwrap().term.vi_mode_cursor.point));
                p.vi_motion(motion);
                check_grid(p);
            }
        }
        10 => p.vi_edge(rng.one_in(2)),
        11 => p.vi_scroll(rng.range(0, 80) as i32 - 40),
        12 => p.vi_select(*rng.pick(&SELECTIONS)),
        13 => {
            // A query from what was printed, or anything.
            let query = if !recent.is_empty() && rng.one_in(2) {
                let at = rng.below(recent.len());
                let n = rng.range(1, 12).min(recent.len() - at);
                String::from_utf8_lossy(&recent[at..at + n]).into_owned()
            } else {
                String::from_utf8_lossy(&rng.text(10)).into_owned()
            };
            crate::fuzz::trace(|| format!("search {query:?}"));
            p.search(&query);
            for _ in 0..rng.below(6) {
                p.search_next(rng.one_in(2));
            }
        }
        14 => p.end_search(),
        15 => {
            p.copy_mode(true);
            p.select_command(rng.one_in(2));
            let _ = p.selection_text();
        }
        16 => {
            p.copy_mode(true);
            p.jump_to_prompt(rng.one_in(2));
        }
        17 => {
            let _ = p.last_command_output();
            let _ = p.last_failure();
            let _ = p.last_command();
        }
        18 => {
            let _ = p.text(Some(rng.size(5000)));
            let _ = p.text(None);
            let _ = p.live_text();
        }
        19 => {
            let _ = p.screen_rows();
            let (x, y) = (rng.below(400) as u16, rng.below(200) as u16);
            let _ = p.url_at(x, y);
            let _ = p.cursor(Rect::new(0, 0, p.cols, p.rows));
        }
        20 => {
            let (x, y) = (rng.below(400) as u16, rng.below(200) as u16);
            let kind = *rng.pick(&SELECTIONS);
            let (x2, y2) = (rng.below(400) as u16, rng.below(200) as u16);
            fuzz::trace(|| format!("  select {kind:?} from {x},{y} to {x2},{y2}"));
            p.selection_start(x, y, kind);
            p.selection_update(x2, y2);
            let _ = p.selection_text();
        }
        21 => p.clear_selection(),
        22 => {
            let _ = p.poll_sync_timeout();
        }
        23 => {
            let saved = p.scrollback_ansi();
            if let Some(saved) = saved {
                let mut fresh = fixture(p.cols, p.rows, rng.range(0, 3000), f.env.graphics.clone());
                fresh.pane.restore_scrollback(&saved, rng.one_in(2).then_some(1_700_000_000), rng.size(5000));
                let _ = fresh.pane.text(Some(100));
            }
        }
        24 => {
            let _ = p.respawn(&f.env, &f.tx);
        }
        25 => {
            let graphics = &f.env.graphics;
            graphics.set_available(!graphics.available());
        }
        26 => {
            // The images waiting to be prepared, as the server's threads do.
            while let Some(work) = p.take_image_work() {
                p.image_prepared(work());
            }
        }
        _ => {
            // A frame: what a client is sent, drawn twice.
            draw_and_compare(p);
        }
    }
}

/// The screen drawn from the cache brought up to date is the screen drawn whole.
fn draw_and_compare(p: &mut Pane) {
    let (cols, rows) = (p.cols, p.rows);
    let Some(cache) = p.refresh_screen(cols, rows) else { return };
    let area = Rect::new(1, 2, cols, rows);
    let screen = Rect::new(0, 0, cols + 2, rows + 3);
    let (mut cached, mut full) = (Buffer::empty(screen), Buffer::empty(screen));
    let (mut cached_links, mut full_links) = (crate::ansi::Links::new(), crate::ansi::Links::new());
    cache.draw(area, &mut cached, |_| Some((0, cols)), &mut cached_links);
    p.render(area, &mut full, &mut full_links);
    assert_eq!(cached, full, "the cached screen differs from the screen");
    assert_eq!(cached_links, full_links);
    no_controls(&full);
    p.show_images(area, &mut full, true);
    let _ = p.images();
}

/// The emulator's state is one it can read: the view, the copy-mode cursor and the selection
/// on its grid, the lens in the primary screen's history.
fn check_grid(p: &Pane) {
    let Some(t) = p.term() else { return };
    let term = &t.term;
    let grid = term.grid();
    let (top, bottom, last) = (term.topmost_line(), term.bottommost_line(), term.last_column());
    assert!(
        grid.display_offset() <= grid.history_size(),
        "view {} rows up, of {}",
        grid.display_offset(),
        grid.history_size()
    );
    if term.mode().contains(TermMode::ALT_SCREEN) {
        assert_eq!(t.archive.lens_rows(), 0, "a lens on the alternate screen");
    } else {
        assert!(
            t.archive.lens_rows() <= grid.history_size(),
            "a lens of {} rows in {}",
            t.archive.lens_rows(),
            grid.history_size()
        );
    }
    if term.mode().contains(TermMode::VI) {
        let point = term.vi_mode_cursor.point;
        assert!(
            point.line >= top && point.line <= bottom && point.column <= last,
            "copy-mode cursor {point:?} off {top:?}..={bottom:?}"
        );
    }
    if let Some(range) = term.selection.as_ref().and_then(|s| s.to_range(term)) {
        let on = |p: alacritty_terminal::index::Point| p.line >= top && p.line <= bottom && p.column <= last;
        assert!(on(range.start) && on(range.end), "selection {range:?} off {top:?}..={bottom:?}");
    }
}

/// What the pane holds stays within its limits.
fn check_bounds(p: &mut Pane, scrollback: usize) {
    let Some(t) = p.term_mut() else { return };
    let own = t.own_history();
    // The archive holds whole lines, trimmed to the scrollback at the width they were laid out
    // at (at a new width, when they are next brought back): at least a row each. The emulator
    // keeps a couple of screens and a batch of its own beyond what the archive trims to, and the
    // lens is a copy.
    let lines = t.archive.lines().len();
    let screens = t.term.screen_lines();
    assert!(
        lines + own <= scrollback + 2 * screens.max(32) + 1300,
        "a history of {lines} archived lines and {own} rows, for a scrollback of {scrollback}"
    );
    // A line is kept as its text and the changes of attributes along it: a few bytes a cell at
    // most, for rows of the widest pane.
    let bytes = t.archive.lines().bytes();
    assert!(bytes <= (scrollback + 1300) * 300 * 64, "{bytes} bytes for {lines} archived lines");
}

#[test]
fn fuzz_pane_output() {
    fuzz::run("pane_output", 12, |rng| {
        let (cols, rows) = size(rng);
        let scrollback = *rng.pick(&[0, 10, 100, 1000, 3000]);
        let mut f = fixture(cols, rows, scrollback, graphics(rng));
        let mut recent = Vec::new();
        for _ in 0..rng.range(1, 40) {
            if rng.below(3) > 0 {
                recent = fuzz::terminal_output(rng, 6000, true);
                fuzz::trace(|| format!("feed {:?}", String::from_utf8_lossy(&recent)));
                for chunk in rng.chunks(&recent) {
                    let signals = f.pane.feed(chunk);
                    for s in signals {
                        if let PaneSignal::Notification { title, body } = s {
                            assert!(title.chars().count() <= 300 && body.chars().count() <= 300);
                        }
                    }
                    fuzz::trace(|| {
                        let t = f.pane.term().unwrap();
                        let g = t.term.grid();
                        format!(
                            "  read offset {} history {} alt {} vi {}: {:?}",
                            g.display_offset(),
                            g.history_size(),
                            t.term.mode().contains(TermMode::ALT_SCREEN),
                            t.term.mode().contains(TermMode::VI),
                            chunk.escape_ascii().to_string().chars().take(200).collect::<String>()
                        )
                    });
                    check_grid(&f.pane);
                }
            } else {
                act(rng, &mut f, &recent);
            }
            check_grid(&f.pane);
            check_bounds(&mut f.pane, scrollback);
        }
        draw_and_compare(&mut f.pane);
    });
}

/// An emulator that keeps its whole history, fed as a pane's would be without kraal.
struct Reference {
    term: Term<Listener>,
    parser: Processor,
    /// The start of a character the output so far ended with.
    cut: Vec<u8>,
}

impl Reference {
    fn new(cols: u16, rows: u16, scrollback: usize) -> Reference {
        let config = TermConfig { scrolling_history: scrollback, kitty_keyboard: true, ..TermConfig::default() };
        let term = Term::new(config, &Size { cols, rows }, Listener::default());
        Reference { term, parser: Processor::new(), cut: Vec::new() }
    }

    /// Feed `bytes`, the rest of a character cut by the end of the last alone, as kraal does
    /// (vte 0.15 drops the character after it otherwise).
    fn feed(&mut self, bytes: &[u8]) {
        let need = self.cut.first().map_or(0, |b| super::archive::utf8_len(*b).saturating_sub(self.cut.len()));
        let alone =
            if need > 0 && bytes.len() > need && bytes[..need].iter().all(|b| b & 0xc0 == 0x80) { need } else { 0 };
        self.cut.extend_from_slice(bytes);
        let cut = super::archive::incomplete_char(&self.cut);
        self.cut.drain(..self.cut.len() - cut);
        self.parser.advance(&mut Plain(&mut self.term), &bytes[..alone]);
        self.parser.advance(&mut Plain(&mut self.term), &bytes[alone..]);
    }
}

/// The text of every row of the history and the screen, oldest first.
fn all_rows<T>(term: &Term<T>) -> Vec<String> {
    let top = -(term.grid().history_size() as i32);
    (top..term.screen_lines() as i32).map(|l| row_text(term, alacritty_terminal::index::Line(l))).collect()
}

fn view<T: alacritty_terminal::event::EventListener>(term: &Term<T>) -> Buffer {
    let area = Rect::new(0, 0, term.columns() as u16, term.screen_lines() as u16);
    let mut buf = Buffer::empty(area);
    render_term(term, area, &mut buf, None);
    no_controls(&buf);
    buf
}

/// What is drawn holds no control character: sent to the client's terminal, it would act there.
fn no_controls(buf: &Buffer) {
    if let Some(cell) = buf.content().iter().find(|c| c.symbol().chars().any(char::is_control)) {
        panic!("a control character drawn: {:?}", cell.symbol());
    }
}

/// The pane's screen is the emulator's, unless either holds a synchronized update back. Once
/// resized, the text only: the archive colours the end of a line's new last row where the
/// emulator rewraps the coloured cells.
fn same_screen(t: &TermPane, r: &Reference, resized: bool, what: &str) {
    if t.parser.sync_timeout().sync_timeout().is_some() || r.parser.sync_timeout().sync_timeout().is_some() {
        return;
    }
    let (mut ours, mut theirs) = (view(&t.term), view(&r.term));
    // kraal blanks the second half of a wide character whose first half was deleted (`CSI P`):
    // alacritty keeps it, half of nothing.
    let area = theirs.area;
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let orphan = theirs[(x, y)].symbol().is_empty()
                && (x == area.left() || unicode_width::UnicodeWidthStr::width(theirs[(x - 1, y)].symbol()) < 2);
            if orphan {
                theirs[(x, y)].reset();
                ours[(x, y)].reset();
            }
        }
    }
    if resized {
        let text = |b: &Buffer| b.content.iter().map(|c| c.symbol().to_string()).collect::<Vec<_>>();
        assert_eq!(text(&ours), text(&theirs), "{what}:\n{ours:?}\n{theirs:?}");
    } else {
        assert_eq!(ours, theirs, "{what}");
    }
}

/// Output kraal passes on to the emulator as it is: no images, no kraal sequences that make it
/// answer, fewer keyboard modes than the guard lets through.
/// `ascii`: no byte past ASCII (no wide or zero-width characters), for a pane that changes
/// size: the archive lays out whole lines where the emulator rewraps its rows, and they cut a
/// line around a wide character in different places.
fn passed_on(rng: &mut Rng, max: usize, ascii: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let n = rng.size(max);
    while out.len() < n {
        let mut piece = Vec::new();
        fuzz::terminal_piece(rng, &mut piece, false);
        let pushes = piece.windows(2).filter(|w| *w == b"[>").count();
        if pushes > 50 {
            continue;
        }
        if ascii {
            // And lines shorter than the thousand rows past which the archive keeps a line in
            // pieces (no repeats of a character, `CSI b`).
            if piece.starts_with(b"\x1b[") && piece.ends_with(b"b") {
                continue;
            }
            piece.retain(u8::is_ascii);
            if rng.one_in(3) {
                piece.extend_from_slice(b"\r\n");
            }
        }
        out.extend(piece);
    }
    out
}

#[test]
fn fuzz_pane_shows_what_the_emulator_shows() {
    fuzz::run("pane_shows_what_the_emulator_shows", 8, |rng| {
        let (cols, rows) = (rng.range(2, 100) as u16, rng.range(1, 40) as u16);
        let scrollback = *rng.pick(&[0, 50, 1000, 3000]);
        let disabled = Arc::new(crate::graphics::Shared::new(false, 0));
        let mut f = fixture(cols, rows, scrollback, disabled);
        let mut r = Reference::new(cols, rows, scrollback);
        let mut resized = false;
        let resizes = rng.one_in(3);
        for step in 0..rng.range(1, 30) {
            if resizes && rng.one_in(4) {
                // A larger screen (a pane zoomed, a window grown): what each holds is laid out
                // at its width, the screen filled from the history. The history is the same
                // while it is far from full: kraal keeps at least all but what the emulator may
                // hold of its own (a couple of screens and a batch), and drops whole lines where
                // the emulator alone drops rows. (A narrower screen rewraps rows the emulator
                // alone cut at the old width. Behind the alternate screen, the primary one is
                // resized without what the archive holds. Once resized, the emulator alone keeps
                // the rows it rewrapped, kraal lines it laid out whole: they may wrap apart at
                // the next size.)
                if resized
                    || r.term.grid().history_size() + 1300 > scrollback
                    || r.term.mode().contains(TermMode::ALT_SCREEN)
                {
                    break;
                }
                let (cols, rows) = (r.term.columns(), r.term.screen_lines());
                let (cols, rows) = (rng.range(cols, cols + 60) as u16, rng.range(rows, rows + 30) as u16);
                fuzz::trace(|| {
                    let t = f.pane.term_mut().unwrap();
                    let archived = t.archive.lines().len();
                    format!(
                        "resize {cols}x{rows}: reference history {}, own {}, archived lines {archived}, scrollback {scrollback}",
                        r.term.grid().history_size(),
                        t.own_history(),
                    )
                });
                // kraal ends a synchronized update at a command mark: what it held lands before
                // the new size in both.
                if let Some(t) = f.pane.term_mut() {
                    t.marked(|t| t.stop_sync());
                }
                r.parser.stop_sync(&mut Plain(&mut r.term));
                f.pane.resize(cols, rows);
                r.term.resize(Size { cols, rows });
                resized = true;
                let t = f.pane.term().unwrap();
                fuzz::trace(|| {
                    format!(
                        "resized: reference history {} cursor {:?}, own {} cursor {:?}",
                        r.term.grid().history_size(),
                        r.term.grid().cursor.point,
                        t.own_history(),
                        t.term.grid().cursor.point
                    )
                });
                same_screen(t, &r, true, &format!("the screen at step {step}, resized"));
            }
            // The same reads for both: vte 0.15 reads a C1 control cut across two reads as a
            // character (which a repeat, `CSI b`, repeats).
            let output = passed_on(rng, 5000, resizes);
            for chunk in rng.chunks(&output) {
                fuzz::trace(|| format!("feed {:?}", chunk.escape_ascii().to_string()));
                if std::env::var_os("KRAAL_FUZZ_EACH_BYTE").is_some() {
                    // To find where they part: a byte at a time, compared after each.
                    for (i, byte) in chunk.iter().enumerate() {
                        f.pane.feed(&[*byte]);
                        r.feed(&[*byte]);
                        let what = format!("after {:?} at step {step}", chunk[..=i].escape_ascii().to_string());
                        same_screen(f.pane.term().unwrap(), &r, resized, &what);
                    }
                    continue;
                }
                f.pane.feed(chunk);
                r.feed(chunk);
            }
            if rng.one_in(3) {
                f.pane.rest();
            }
            if rng.one_in(5) {
                // Any pending synchronized update lands in both.
                if let Some(t) = f.pane.term_mut() {
                    t.marked(|t| t.stop_sync());
                }
                let r = &mut r;
                r.parser.stop_sync(&mut Plain(&mut r.term));
            }
            fuzz::trace(|| {
                let t = f.pane.term_mut().unwrap();
                let width = t.term.columns();
                let archived: usize = t.archive.lines().oldest_first().map(|l| l.rows_at(width)).sum();
                format!(
                    "step {step}: reference history {}, own {}, archived {archived}",
                    r.term.grid().history_size(),
                    t.own_history()
                )
            });
            let t = f.pane.term().unwrap();
            if t.parser.sync_timeout().sync_timeout().is_some() || r.parser.sync_timeout().sync_timeout().is_some() {
                continue;
            }
            same_screen(t, &r, resized, &format!("the screen at step {step}"));
            // Cell by cell, with what the screen does not show (where lines wrap), until a resize:
            // the emulator alone rewraps rows it rewrapped before, the archive whole lines.
            for l in 0..if resized { 0 } else { t.term.screen_lines() as i32 } {
                let line = alacritty_terminal::index::Line(l);
                let (ours, theirs) = (&t.term.grid()[line], &r.term.grid()[line]);
                // A link without an id is given one by its emulator: only its target counts.
                let cell = |c: &alacritty_terminal::term::cell::Cell| {
                    let link = c.hyperlink().map(|h| h.uri().to_string());
                    (c.c, c.fg, c.bg, c.flags, c.zerowidth().map(<[char]>::to_vec), c.underline_color(), link)
                };
                use alacritty_terminal::term::cell::Flags;
                let orphan = |c: alacritty_terminal::index::Column| {
                    theirs[c].flags.contains(Flags::WIDE_CHAR_SPACER)
                        && (c.0 == 0 || !theirs[c - 1].flags.contains(Flags::WIDE_CHAR))
                };
                let differs = (0..t.term.columns())
                    .map(alacritty_terminal::index::Column)
                    .find(|c| !orphan(*c) && cell(&ours[*c]) != cell(&theirs[*c]));
                if let Some(c) = differs {
                    panic!("row {l} at step {step}, column {}: {:?} {:?}", c.0, ours[c], theirs[c]);
                }
            }
            let (ours, theirs) = (&t.term.grid().cursor, &r.term.grid().cursor);
            assert_eq!(ours.input_needs_wrap, theirs.input_needs_wrap, "the wrap pending at step {step}");
            assert_eq!(t.term.mode(), r.term.mode(), "the modes at step {step}");
            assert_eq!(t.term.grid().cursor.point, r.term.grid().cursor.point, "the cursor at step {step}");
            if resized {
                // The archive lays out its lines at the new width as the emulator rewraps its
                // rows, but for the rows a line cut at the old width leaves.
                continue;
            }
            // The history, the newest rows first: kraal drops whole lines where the emulator
            // drops rows.
            let mut ours = t.archived_rows(usize::MAX);
            ours.extend(all_rows(&t.term));
            let mut reference = all_rows(&r.term);
            for rows in [&mut ours, &mut reference] {
                while rows.last().is_some_and(String::is_empty) {
                    rows.pop();
                }
            }
            let n = ours.len().min(reference.len()).saturating_sub(1);
            if let Some(i) = (1..=n).find(|i| ours[ours.len() - i] != reference[reference.len() - i]) {
                let near = |rows: &[String]| rows[rows.len() - (i + 2).min(rows.len())..rows.len() - i + 1].to_vec();
                panic!(
                    "the history at step {step}, {i} rows up of {} and {}:\n{:?}\n{:?}",
                    ours.len(),
                    reference.len(),
                    near(&ours),
                    near(&reference)
                );
            }
        }
    });
}

/// The keyboard mode stacks as alacritty keeps them: one per screen, the alternate screen's in
/// front while it is up, both emptied by a reset.
#[derive(Default)]
struct Stacks {
    depth: [usize; 2],
    alt: bool,
    deepest: usize,
}

impl alacritty_terminal::vte::ansi::Handler for Stacks {
    fn push_keyboard_mode(&mut self, _: alacritty_terminal::vte::ansi::KeyboardModes) {
        let d = &mut self.depth[usize::from(self.alt)];
        *d += 1;
        self.deepest = self.deepest.max(*d);
    }

    fn pop_keyboard_modes(&mut self, n: u16) {
        let d = &mut self.depth[usize::from(self.alt)];
        *d = d.saturating_sub(usize::from(n));
    }

    fn set_private_mode(&mut self, mode: alacritty_terminal::vte::ansi::PrivateMode) {
        use alacritty_terminal::vte::ansi::{NamedPrivateMode, PrivateMode};
        if mode == PrivateMode::Named(NamedPrivateMode::SwapScreenAndSetRestoreCursor) {
            self.alt = true;
        }
    }

    fn unset_private_mode(&mut self, mode: alacritty_terminal::vte::ansi::PrivateMode) {
        use alacritty_terminal::vte::ansi::{NamedPrivateMode, PrivateMode};
        if mode == PrivateMode::Named(NamedPrivateMode::SwapScreenAndSetRestoreCursor) {
            self.alt = false;
        }
    }

    fn reset_state(&mut self) {
        self.depth = [0, 0];
        self.alt = false;
    }
}

/// What the guard lets through never takes a stack deeper than its limit, as alacritty's own
/// parser reads it (synchronized updates included), however the output wraps the pushes.
#[test]
fn fuzz_pane_keyboard_stacks_stay_bounded() {
    fuzz::run("pane_keyboard_stacks_stay_bounded", 12, |rng| {
        let mut guard = super::guard::KeyboardStackGuard::default();
        let mut stacks = Stacks::default();
        let mut parser: Processor = Processor::new();
        let push: &[&[u8]] = &[b"\x1b[>1u", b"\x1b[>u", b"\x1b[>31u", b"\x1b[>1;2u", b"\x1b[?1049h", b"\x1b[?1049l"];
        for _ in 0..rng.range(1, 6) {
            let mut output = Vec::new();
            for _ in 0..rng.range(100, 3000) {
                match rng.below(8) {
                    0..5 => output.extend_from_slice(rng.one_of(push)),
                    5 => output.extend_from_slice(rng.one_of(&[
                        &b"\x1b[<u"[..],
                        b"\x1b[<3u",
                        b"\x1bc",
                        b"\x1b[?2026h",
                        b"\x1b[?2026l",
                    ])),
                    _ => fuzz::terminal_piece(rng, &mut output, false),
                }
            }
            for chunk in rng.chunks(&output) {
                parser.advance(&mut stacks, &guard.filter(chunk));
            }
            if rng.one_in(2) {
                parser.stop_sync(&mut stacks);
            }
            assert!(stacks.deepest <= 1024, "a stack {} deep", stacks.deepest);
        }
    });
}

/// The OSC sequences kraal reads itself, from any bytes: no panic, what they say cleaned and
/// checked.
#[test]
fn fuzz_pane_osc() {
    fuzz::run("pane_osc", 200, |rng| {
        let mut scanner = super::osc::OscScanner::default();
        let output = fuzz::terminal_output(rng, 20_000, false);
        for chunk in rng.chunks(&output) {
            for (end, osc) in scanner.scan(chunk) {
                assert!(end <= chunk.len());
                match osc {
                    super::osc::Osc::Notify { title, body } => {
                        assert!(title.chars().count() <= 300 && body.chars().count() <= 300);
                        assert!(!title.chars().chain(body.chars()).any(char::is_control));
                    }
                    super::osc::Osc::Attached(Some((server, session))) => {
                        assert!(crate::servers::valid_name(&server) && crate::paths::valid_session_name(&session));
                    }
                    _ => {}
                }
            }
        }
        let mut body = rng
            .pick(&["7;file://h/a%20b", "133;D;1", "777;notify;t;b", "7701;context;k8s=a", "9;x"])
            .as_bytes()
            .to_vec();
        rng.mutate(&mut body, &[b";", b"%", b"%2", b"=", b"7701", b"\xff"]);
        let _ = super::osc::parse_osc(&body);
        let _ = super::osc::percent_decode(&String::from_utf8_lossy(&body));
    });
}

/// The emulator alone, but for the edits that may split a wide character, which kraal mends
/// (see `archive::mended`).
pub(super) struct Plain<'a>(pub(super) &'a mut Term<Listener>);

mod plain {
    use alacritty_terminal::vte::ansi::{
        Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
        KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb, ScpCharPath, ScpUpdateMode,
        StandardCharset, TabulationClearMode, cursor_icon::CursorIcon,
    };

    use alacritty_terminal::term::TermMode;

    use super::Plain;
    use crate::pane::archive::mended;

    impl Handler for Plain<'_> {
        fn delete_chars(&mut self, count: usize) {
            mended(self.0, |t| Handler::delete_chars(t, count))
        }

        fn linefeed(&mut self) {
            Handler::linefeed(&mut *self.0)
        }
        fn newline(&mut self) {
            Handler::newline(&mut *self.0)
        }
        fn scroll_up(&mut self, rows: usize) {
            Handler::scroll_up(&mut *self.0, rows)
        }
        fn delete_lines(&mut self, rows: usize) {
            Handler::delete_lines(&mut *self.0, rows)
        }
        fn put_tab(&mut self, count: u16) {
            Handler::put_tab(&mut *self.0, count)
        }
        fn clear_line(&mut self, mode: LineClearMode) {
            mended(self.0, |t| Handler::clear_line(t, mode))
        }
        fn erase_chars(&mut self, count: usize) {
            mended(self.0, |t| Handler::erase_chars(t, count))
        }
        fn insert_blank(&mut self, count: usize) {
            mended(self.0, |t| Handler::insert_blank(t, count))
        }
        fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
            Handler::set_scrolling_region(&mut *self.0, top, bottom)
        }
        fn scroll_down(&mut self, rows: usize) {
            Handler::scroll_down(&mut *self.0, rows)
        }
        fn insert_blank_lines(&mut self, rows: usize) {
            Handler::insert_blank_lines(&mut *self.0, rows)
        }
        fn reverse_index(&mut self) {
            Handler::reverse_index(&mut *self.0)
        }
        fn unset_private_mode(&mut self, mode: PrivateMode) {
            Handler::unset_private_mode(&mut *self.0, mode)
        }
        fn decaln(&mut self) {
            Handler::decaln(&mut *self.0)
        }
        fn set_title(&mut self, title: Option<String>) {
            Handler::set_title(&mut *self.0, title)
        }
        fn set_cursor_style(&mut self, style: Option<CursorStyle>) {
            Handler::set_cursor_style(&mut *self.0, style)
        }
        fn set_cursor_shape(&mut self, shape: CursorShape) {
            Handler::set_cursor_shape(&mut *self.0, shape)
        }
        fn goto(&mut self, line: i32, col: usize) {
            Handler::goto(&mut *self.0, line, col)
        }
        fn goto_line(&mut self, line: i32) {
            Handler::goto_line(&mut *self.0, line)
        }
        fn goto_col(&mut self, col: usize) {
            Handler::goto_col(&mut *self.0, col)
        }
        fn move_up(&mut self, rows: usize) {
            Handler::move_up(&mut *self.0, rows)
        }
        fn move_down(&mut self, rows: usize) {
            Handler::move_down(&mut *self.0, rows)
        }
        fn identify_terminal(&mut self, intermediate: Option<char>) {
            Handler::identify_terminal(&mut *self.0, intermediate)
        }
        fn device_status(&mut self, arg: usize) {
            Handler::device_status(&mut *self.0, arg)
        }
        fn move_forward(&mut self, cols: usize) {
            Handler::move_forward(&mut *self.0, cols)
        }
        fn move_backward(&mut self, cols: usize) {
            Handler::move_backward(&mut *self.0, cols)
        }
        fn move_down_and_cr(&mut self, rows: usize) {
            Handler::move_down_and_cr(&mut *self.0, rows)
        }
        fn move_up_and_cr(&mut self, rows: usize) {
            Handler::move_up_and_cr(&mut *self.0, rows)
        }
        fn backspace(&mut self) {
            Handler::backspace(&mut *self.0)
        }
        fn carriage_return(&mut self) {
            Handler::carriage_return(&mut *self.0)
        }
        fn bell(&mut self) {
            Handler::bell(&mut *self.0)
        }
        fn substitute(&mut self) {
            Handler::substitute(&mut *self.0)
        }
        fn set_horizontal_tabstop(&mut self) {
            Handler::set_horizontal_tabstop(&mut *self.0)
        }
        fn move_backward_tabs(&mut self, count: u16) {
            Handler::move_backward_tabs(&mut *self.0, count)
        }
        fn move_forward_tabs(&mut self, count: u16) {
            Handler::move_forward_tabs(&mut *self.0, count)
        }
        fn save_cursor_position(&mut self) {
            Handler::save_cursor_position(&mut *self.0)
        }
        fn restore_cursor_position(&mut self) {
            Handler::restore_cursor_position(&mut *self.0)
        }
        fn clear_tabs(&mut self, mode: TabulationClearMode) {
            Handler::clear_tabs(&mut *self.0, mode)
        }
        fn set_tabs(&mut self, interval: u16) {
            Handler::set_tabs(&mut *self.0, interval)
        }
        fn terminal_attribute(&mut self, attr: Attr) {
            Handler::terminal_attribute(&mut *self.0, attr)
        }
        fn set_mode(&mut self, mode: Mode) {
            Handler::set_mode(&mut *self.0, mode)
        }
        fn unset_mode(&mut self, mode: Mode) {
            Handler::unset_mode(&mut *self.0, mode)
        }
        fn report_mode(&mut self, mode: Mode) {
            Handler::report_mode(&mut *self.0, mode)
        }
        fn report_private_mode(&mut self, mode: PrivateMode) {
            Handler::report_private_mode(&mut *self.0, mode)
        }
        fn set_keypad_application_mode(&mut self) {
            Handler::set_keypad_application_mode(&mut *self.0)
        }
        fn unset_keypad_application_mode(&mut self) {
            Handler::unset_keypad_application_mode(&mut *self.0)
        }
        fn set_active_charset(&mut self, index: CharsetIndex) {
            Handler::set_active_charset(&mut *self.0, index)
        }
        fn configure_charset(&mut self, index: CharsetIndex, charset: StandardCharset) {
            Handler::configure_charset(&mut *self.0, index, charset)
        }
        fn set_color(&mut self, index: usize, color: Rgb) {
            Handler::set_color(&mut *self.0, index, color)
        }
        fn dynamic_color_sequence(&mut self, prefix: String, index: usize, terminator: &str) {
            Handler::dynamic_color_sequence(&mut *self.0, prefix, index, terminator)
        }
        fn reset_color(&mut self, index: usize) {
            Handler::reset_color(&mut *self.0, index)
        }
        fn clipboard_store(&mut self, clipboard: u8, data: &[u8]) {
            Handler::clipboard_store(&mut *self.0, clipboard, data)
        }
        fn clipboard_load(&mut self, clipboard: u8, terminator: &str) {
            Handler::clipboard_load(&mut *self.0, clipboard, terminator)
        }
        fn push_title(&mut self) {
            Handler::push_title(&mut *self.0)
        }
        fn pop_title(&mut self) {
            Handler::pop_title(&mut *self.0)
        }
        fn text_area_size_pixels(&mut self) {
            Handler::text_area_size_pixels(&mut *self.0)
        }
        fn text_area_size_chars(&mut self) {
            Handler::text_area_size_chars(&mut *self.0)
        }
        fn set_hyperlink(&mut self, link: Option<Hyperlink>) {
            Handler::set_hyperlink(&mut *self.0, link)
        }
        fn set_mouse_cursor_icon(&mut self, icon: CursorIcon) {
            Handler::set_mouse_cursor_icon(&mut *self.0, icon)
        }
        fn report_keyboard_mode(&mut self) {
            Handler::report_keyboard_mode(&mut *self.0)
        }
        fn push_keyboard_mode(&mut self, mode: KeyboardModes) {
            Handler::push_keyboard_mode(&mut *self.0, mode)
        }
        fn pop_keyboard_modes(&mut self, count: u16) {
            Handler::pop_keyboard_modes(&mut *self.0, count)
        }
        fn set_keyboard_mode(&mut self, mode: KeyboardModes, behavior: KeyboardModesApplyBehavior) {
            Handler::set_keyboard_mode(&mut *self.0, mode, behavior)
        }
        fn set_modify_other_keys(&mut self, mode: ModifyOtherKeys) {
            Handler::set_modify_other_keys(&mut *self.0, mode)
        }
        fn report_modify_other_keys(&mut self) {
            Handler::report_modify_other_keys(&mut *self.0)
        }
        fn set_scp(&mut self, char_path: ScpCharPath, update_mode: ScpUpdateMode) {
            Handler::set_scp(&mut *self.0, char_path, update_mode)
        }
        fn clear_screen(&mut self, mode: ClearMode) {
            mended(self.0, |t| Handler::clear_screen(t, mode))
        }
        fn reset_state(&mut self) {
            Handler::reset_state(&mut *self.0)
        }
        fn input(&mut self, c: char) {
            if crate::pane::archive::zerowidth_full(self.0, c) {
                return;
            }
            if self.0.mode().contains(TermMode::INSERT) {
                mended(self.0, |t| Handler::input(t, c))
            } else {
                Handler::input(&mut *self.0, c)
            }
        }
        fn set_private_mode(&mut self, mode: PrivateMode) {
            Handler::set_private_mode(&mut *self.0, mode)
        }
    }
}

/// What a pane saves of its text comes back as it was: saved, given to a new pane, saved again,
/// it is the same, but for the line saying it was restored. A damaged file is read without a
/// panic.
#[test]
fn fuzz_pane_scrollback_files() {
    fuzz::run("pane_scrollback_files", 20, |rng| {
        let (cols, rows) = (rng.range(2, 120) as u16, rng.range(1, 40) as u16);
        let disabled = || Arc::new(crate::graphics::Shared::new(false, 0));
        let mut f = fixture(cols, rows, 3000, disabled());
        for _ in 0..rng.range(1, 6) {
            let output = passed_on(rng, 4000, false);
            for chunk in rng.chunks(&output) {
                f.pane.feed(chunk);
            }
        }
        // Back on the primary screen, the update and the reads all in.
        f.pane.feed(b"\x1b[?1049l\x1b[?2026l");
        f.pane.rest();
        let Some(saved) = f.pane.scrollback_ansi() else { return };
        let mut again = fixture(cols, rows, 3000, disabled());
        again.pane.restore_scrollback(&saved, None, 3000 + usize::from(rows));
        let resaved = again.pane.scrollback_ansi().unwrap();
        // The new pane's own lines: the restored ones, then the line that says so.
        let text = String::from_utf8_lossy(&resaved);
        let kept = text.rsplit_once("\r\n").map_or("", |(k, _)| k);
        let want = String::from_utf8_lossy(&saved);
        // The text of each line: blanks the cursor had moved past end the last line saved, not
        // the line given back, and a coloured end may come back as coloured blanks.
        let plain = |l: &str| {
            let mut out = String::new();
            let mut chars = l.chars();
            while let Some(c) = chars.next() {
                if c == '\u{1b}' {
                    // `CSI … m`, `CSI K`, and links (`OSC 8 … ESC \`).
                    match chars.next() {
                        Some('[') => while chars.next().is_some_and(|c| !c.is_ascii_alphabetic()) {},
                        Some(']') => while chars.next().is_some_and(|c| c != '\u{1b}') {},
                        _ => {}
                    }
                } else {
                    out.push(c);
                }
            }
            out.trim_end().to_string()
        };
        let lines = |t: &str| {
            let mut lines: Vec<String> = t.split("\r\n").map(plain).collect();
            while lines.last().is_some_and(String::is_empty) {
                lines.pop();
            }
            lines
        };
        let (kept, want) = (lines(kept), lines(&want));
        let n = kept.len().min(want.len());
        if let Some(i) = (1..=n).find(|i| kept[kept.len() - i] != want[want.len() - i]) {
            panic!(
                "line {i} from the end of {} saved, {} saved again:\n{:?}\n{:?}",
                want.len(),
                kept.len(),
                want[want.len() - i],
                kept[kept.len() - i]
            );
        }
        // A damaged file.
        let mut damaged = saved;
        rng.mutate(&mut damaged, &[b"\x1b[", b"\x1b]8;;", b"\r\n", b"m", b"\x1b\\", b"\x07", b"\x1bP"]);
        let mut other = fixture(cols, rows, rng.range(0, 3000), disabled());
        other.pane.restore_scrollback(&damaged, Some(rng.number() as i64), rng.size(5000));
        let _ = other.pane.scrollback_ansi();
    });
}
