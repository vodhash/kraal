//! Composes what one client sees: the sidebar, the top bar, panes in their frames, overlays.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};
use unicode_width::UnicodeWidthStr;

use super::bottom_bar::BottomBar;
use super::geometry::Sidebar;
use super::overlays;
use super::ui::Mode;
use super::{Client, ClientId, Server};
use crate::agent::AgentState;
use crate::ansi::{Cursor, Kept};
use crate::keymap::Action;
use crate::layout::PaneId;
use crate::workspace::catalog::Entry as WorkspaceEntry;

fn th() -> crate::theme::Theme {
    crate::theme::current()
}

#[derive(Debug, Clone, PartialEq)]
pub enum BarItem {
    Workspace,
    Tab(usize),
    NewTab,
    Toast(PaneId, super::attention::ToastAction),
    /// A label a click does nothing on.
    Text,
}

/// One row of the sidebar.
#[derive(Clone, PartialEq)]
enum SideRow {
    /// The line that opens a section, with its name.
    Section(&'static str),
    /// A row of an entry: what clicking it does, and whether it is the current one.
    Entry {
        item: Option<SideItem>,
        line: Line<'static>,
        selected: bool,
    },
    Blank,
}

fn side_entry(
    rows: &mut Vec<SideRow>,
    item: SideItem,
    dot: Span<'static>,
    name: Span<'static>,
    detail: impl Into<Line<'static>>,
    selected: bool,
) {
    rows.push(SideRow::Entry { item: Some(item.clone()), line: Line::from(vec![dot, name]), selected });
    let mut detail = detail.into();
    detail.spans.insert(0, dim_span("   "));
    rows.push(SideRow::Entry { item: Some(item), line: detail, selected });
}

pub(super) struct AccountRows {
    width: u16,
    room: usize,
    threshold: u8,
    theme: crate::theme::Theme,
    rows: Vec<SideRow>,
}

struct StripEntry {
    item: SideItem,
    state: Option<AgentState>,
    icon: AgentState,
    selected: bool,
}

fn agent_urgency(state: &AgentState) -> u8 {
    match state {
        AgentState::Blocked => 4,
        AgentState::Done => 3,
        AgentState::Working => 2,
        AgentState::Idle => 1,
        AgentState::Unknown => 0,
    }
}

fn strip_label(text: &str, item: SideItem, theme: crate::theme::Theme) -> SideRow {
    SideRow::Entry {
        item: Some(item),
        line: Line::styled(text.to_owned(), Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        selected: false,
    }
}

fn strip_entries(entries: Vec<StripEntry>, room: usize, list: SideItem, theme: crate::theme::Theme) -> Vec<SideRow> {
    if room == 0 {
        return Vec::new();
    }
    let overflow = entries.len() > room || entries.len() > 99;
    let has_current = entries.iter().any(|e| e.selected);
    let mut entries: Vec<_> = entries.into_iter().enumerate().take(99).collect();
    if overflow {
        // Keep the current workspace and urgent agents, but retain their original list numbers.
        entries.sort_by_key(|(i, e)| (std::cmp::Reverse((e.selected, e.state.as_ref().map_or(0, agent_urgency))), *i));
        entries.truncate(room - 1);
        entries.sort_by_key(|(i, _)| *i);
    }
    let current_in_list = has_current && !entries.iter().any(|(_, e)| e.selected);
    let mut rows: Vec<_> = entries
        .into_iter()
        .map(|(i, e)| {
            let number = Style::default().fg(if e.selected { theme.text } else { theme.dim });
            let number = if e.selected { number.add_modifier(Modifier::BOLD) } else { number };
            SideRow::Entry {
                item: Some(e.item),
                line: Line::from(vec![
                    Span::styled(format!("{:>2}", i + 1), number),
                    Span::styled(
                        theme.icon(e.icon),
                        Style::default().fg(e.state.map_or(theme.dim, |s| theme.state_color(s))),
                    ),
                ]),
                selected: e.selected,
            }
        })
        .collect();
    if overflow {
        let mut row = strip_label(if theme.ascii { "..." } else { " ⋮ " }, list, theme);
        if let SideRow::Entry { selected, .. } = &mut row {
            *selected = current_in_list;
        }
        rows.push(row);
    }
    rows
}

#[derive(Debug, Clone, PartialEq)]
pub enum SideItem {
    Workspaces,
    Panes,
    ToggleSidebar,
    AddWorkspace,
    Workspace(u32),
    ReviewAccount(String),
    Accounts,
    Account(String),
    AddAccount,
    /// A definition not running, by its file.
    Definition(PathBuf),
    Pane(PaneId),
    /// A tunnel, by local port.
    Tunnel(u16),
    /// A local port a pane's program listens on.
    Port(u16),
    /// An agent of a server's kraal session: the server, and its pane's id there.
    Remote {
        server: String,
        pane: u32,
    },
}

/// What the chrome shows of a pane: its name, why it is production, what it works on.
#[derive(PartialEq)]
pub(super) struct Facts {
    name: String,
    production: Option<String>,
    details: Vec<Span<'static>>,
    frame_details: Vec<Span<'static>>,
}

/// The facts of each pane, with what they were worked out from (the chrome's generation and
/// the pane's output count, see `Server::pane_facts`), and a number that changes with them.
#[derive(Default)]
pub(super) struct FactsCache {
    panes: RefCell<HashMap<PaneId, FactsEntry>>,
    versions: Cell<u64>,
}

struct FactsEntry {
    from: FactsFrom,
    /// The frame they were last found to hold in.
    checked: u64,
    version: u64,
    facts: Rc<Facts>,
}

/// What a pane's facts are worked out from, besides time while a context is read: the chrome's
/// generation (anything but output), and what of the pane's output can change them without
/// saying so (the OSC sequences kraal reads, the scroll, the directory its shell went to).
#[derive(PartialEq)]
struct FactsFrom {
    chrome: u64,
    oscs: u64,
    offset: usize,
    cwd: u64,
}

impl FactsCache {
    /// Forget the panes that are gone.
    pub(super) fn keep(&self, alive: impl Fn(PaneId) -> bool) {
        self.panes.borrow_mut().retain(|id, _| alive(*id));
    }

    /// The panes whose facts were worked out for frame `frame`.
    fn checked_for(&self, frame: u64) -> Vec<PaneId> {
        self.panes.borrow().iter().filter(|(_, e)| e.checked == frame).map(|(id, _)| *id).collect()
    }

    /// The version of the facts last worked out for `pane`.
    fn version(&self, pane: PaneId) -> Option<u64> {
        self.panes.borrow().get(&pane).map(|e| e.version)
    }
}

/// What a client's last frame was made of: the panes drawn from their screen rows with nothing
/// over them, where, and the versions of the rows drawn; what its chrome was drawn from, when
/// nothing went over it.
#[derive(Default)]
pub struct FrameMemo {
    panes: Vec<(PaneId, Rect, Vec<u64>)>,
    chrome: Option<Chrome>,
    /// The look of each pane's frame, with what it was made from, in the order drawn.
    looks: Looks,
    /// What the frame showed around its panes' rows, when the next one can be composed from what
    /// one pane printed alone (see `Server::compose_output`).
    rest: Option<Rest>,
}

/// What a frame showed besides the rows of its panes, each of them alone on its cells and drawn
/// from its screen rows, without images, while keys go to programs. The next frame shows the same
/// around the rows of a pane that printed, while what this holds does: the chrome's generation
/// (everything but output), that pane's facts, the directories read for the panes, and the
/// accounts' usage windows.
struct Rest {
    chrome: u64,
    /// The facts of the panes of the client's workspace, by version, as its top bar has them.
    facts: Vec<(PaneId, u64)>,
    /// Until when no directory a frame read for a pane is read again unless the pane prints.
    cwd_until: Option<Instant>,
    /// When the usage its sidebar shows changes by itself (see [`Inputs`]).
    usage_until: Option<i64>,
}

/// What a frame is made of for the encoder: the cursor, the links, and the cells the last frame
/// already had.
type Composed = (Option<Cursor>, crate::ansi::Links, Vec<Kept>);

/// What a part of the chrome (a bar, the sidebar) is made from, besides the client: the chrome's
/// generation, and the facts of the panes it shows, by version. The same make the same.
#[derive(PartialEq, Default)]
struct Inputs {
    chrome: u64,
    panes: Vec<(PaneId, u64)>,
    /// When the accounts' rows change with nothing else happening: a window resets or a
    /// last-checked age changes (Unix seconds). The sidebar alone shows usage: none for the bars.
    usage_until: Option<i64>,
}

/// The looks of the frames of a frame's panes, with what each was made from.
type Looks = Vec<(PaneId, LookInputs, Rc<FrameLook>)>;

/// The look of each pane's frame, in order; none for a pane without one.
type Frames = Vec<Option<Rc<FrameLook>>>;

/// What a frame's look is made from (see `Server::frame_look`).
#[derive(PartialEq, Clone, Copy)]
struct LookInputs {
    chrome: u64,
    version: u64,
    tile: Rect,
    focused: bool,
    sync: bool,
    floating: bool,
    marked: bool,
}

/// What a frame's chrome is drawn from: the same draws the same cells.
struct Chrome {
    theme: crate::theme::Theme,
    /// The sidebar's place, and each pane's tile, inner place and content, and whether only its
    /// screen rows are drawn there.
    side: Rect,
    places: Vec<(PaneId, Rect, Rect, Rect, bool)>,
    sidebar: Vec<SideRow>,
    sidebar_from: Inputs,
    bar: TopBar,
    bar_from: Inputs,
    bottom: BottomBar,
    bottom_from: Inputs,
    frames: Frames,
    /// The rows of the panes folded in stacks, and their frames' looks.
    folds: Vec<(PaneId, Rect)>,
    fold_looks: Frames,
}

impl Chrome {
    /// Laid out as `other`: the same things go to the same places, in the same colours.
    fn same_places(&self, other: &Chrome) -> bool {
        self.theme == other.theme && self.side == other.side && self.places == other.places && self.folds == other.folds
    }
}

/// What a pane's frame shows: its place, its colour, its title and what it works on.
#[derive(PartialEq)]
struct FrameLook {
    tile: Rect,
    color: Color,
    title: Vec<Span<'static>>,
    details: Vec<Span<'static>>,
}

/// The panes are drawn apart, on their own cells: none floats, no tile (or row of a pane folded
/// in a stack, `folds`) overlaps another, the sidebar (`side`) or the top bar (their screen ends
/// above the bottom bar).
fn apart(placed: &[Placed], folds: &[(PaneId, Rect)], side: Rect) -> bool {
    let places: Vec<Rect> = placed.iter().map(|p| p.tile).chain(folds.iter().map(|(_, r)| *r)).collect();
    !placed.iter().any(|p| p.floating)
        && places
            .iter()
            .enumerate()
            .all(|(i, r)| r.y > 0 && !r.intersects(side) && places[i + 1..].iter().all(|q| !q.intersects(*r)))
}

/// The rows of pane contents the frame already has.
fn kept_rows(placed: &[Placed]) -> Vec<Kept> {
    let mut kept: Vec<Kept> = placed
        .iter()
        .filter_map(|p| Some((p, p.rows.as_ref()?)))
        .flat_map(|(p, rows)| {
            let c = p.content;
            // No row to draw: all of them are kept.
            let all = (rows.is_empty()).then(|| vec![None; usize::from(c.height)]);
            let rows = all.unwrap_or_else(|| rows.clone());
            rows.into_iter().enumerate().flat_map(move |(r, span)| {
                let y = c.y + r as u16;
                // A row is kept but for the columns drawn again.
                let (from, to) = span.map_or((c.width, c.width), |(a, b)| (a.min(c.width), b.min(c.width)));
                [Kept { y, x0: c.x, x1: c.x + from }, Kept { y, x0: c.x + to, x1: c.right() }]
                    .into_iter()
                    .filter(|k| k.x0 < k.x1)
            })
        })
        .collect();
    kept.sort_unstable_by_key(|k| (k.y, k.x0));
    kept
}

/// For each of a pane's screen `rows`, `width` columns wide, the columns to draw again (from,
/// to) over a frame that has them at the versions `old`: none for a row the frame has, all of
/// them for a row with images or one it never had.
fn rows_to_draw(rows: &crate::pane::ScreenCache, old: &[u64], width: u16) -> Vec<Option<(u16, u16)>> {
    let whole = Some((0, width));
    let row = |(r, v): (usize, &u64)| match old.get(r) {
        _ if rows.has_images(r) => whole,
        Some(was) if was == v => None,
        // The row changed in a few columns since the frame has it: those.
        Some(was) => rows.changed_since(r, *was).or(whole),
        None => whole,
    };
    rows.versions().iter().enumerate().map(row).collect()
}

/// The cells of a screen to draw again, row by row.
struct Changed {
    area: Rect,
    rows: Vec<Vec<(u16, u16)>>,
}

impl Changed {
    fn new(area: Rect) -> Changed {
        Changed { area, rows: vec![Vec::new(); usize::from(area.height)] }
    }

    fn row(&mut self, y: u16, x0: u16, x1: u16) {
        let (x0, x1) = (x0.max(self.area.x), x1.min(self.area.right()));
        if let Some(row) = y.checked_sub(self.area.y).and_then(|r| self.rows.get_mut(usize::from(r)))
            && x0 < x1
        {
            row.push((x0, x1));
        }
    }

    fn rect(&mut self, r: Rect) {
        for y in r.top()..r.bottom() {
            self.row(y, r.left(), r.right());
        }
    }

    /// The edges of `r`.
    fn border(&mut self, r: Rect) {
        if r.is_empty() {
            return;
        }
        self.row(r.top(), r.left(), r.right());
        self.row(r.bottom() - 1, r.left(), r.right());
        for y in r.top() + 1..r.bottom().saturating_sub(1) {
            self.row(y, r.left(), r.left() + 1);
            self.row(y, r.right() - 1, r.right());
        }
    }

    /// Every other cell, row by row.
    fn kept(mut self) -> Vec<Kept> {
        let mut kept = Vec::new();
        for (i, row) in self.rows.iter_mut().enumerate() {
            let y = self.area.y + i as u16;
            row.sort_unstable();
            let mut x = self.area.left();
            for &(x0, x1) in row.iter() {
                if x0 > x {
                    kept.push(Kept { y, x0: x, x1: x0 });
                }
                x = x.max(x1);
            }
            if x < self.area.right() {
                kept.push(Kept { y, x0: x, x1: self.area.right() });
            }
        }
        kept
    }
}

/// A pane as a frame shows it.
struct Placed {
    pane: PaneId,
    tile: Rect,
    inner: Rect,
    floating: bool,
    /// Where its content goes.
    content: Rect,
    /// Drawn from its screen rows: for each row, the columns to draw again (from, to), none
    /// when the frame already has it.
    rows: Option<Vec<Option<(u16, u16)>>>,
    /// The versions of its rows, when nothing else is drawn over them.
    versions: Option<Vec<u64>>,
}

impl Server {
    /// Whether what the chrome shows of pane `id` is no longer what was last worked out of it,
    /// for the frame being drawn.
    pub(super) fn facts_changed(&self, id: PaneId) -> bool {
        let before = self.facts.version(id);
        self.pane_facts(id).map(|(version, _)| version) != before
    }

    /// What the chrome shows of pane `id`, with its version. Worked out again only when
    /// something it comes from may have changed (see `FactsFrom`), and while a context is read
    /// (one that does not answer soon becomes unknown).
    fn pane_facts(&self, id: PaneId) -> Option<(u64, Rc<Facts>)> {
        let pane = self.panes.get(&id)?;
        let frame = crate::pane::frame();
        let timed = self.contexts_reading();
        // Once in a frame: it shows one moment.
        if let Some(e) = self.facts.panes.borrow().get(&id)
            && frame != 0
            && e.checked == frame
        {
            return Some((e.version, e.facts.clone()));
        }
        let from = FactsFrom {
            chrome: self.chrome_gen,
            oscs: pane.oscs,
            offset: pane.display_offset(),
            cwd: pane.cwd_version(),
        };
        if !timed
            && let Some(e) = self.facts.panes.borrow_mut().get_mut(&id)
            && e.from == from
        {
            e.checked = frame;
            return Some((e.version, e.facts.clone()));
        }
        let facts = Facts {
            name: pane.display_name(),
            production: self.pane_production_shown(id),
            details: self.pane_details(id, false),
            frame_details: self.pane_details(id, true),
        };
        let mut panes = self.facts.panes.borrow_mut();
        if let Some(e) = panes.get_mut(&id)
            && *e.facts == facts
        {
            e.from = from;
            e.checked = frame;
            return Some((e.version, e.facts.clone()));
        }
        let version = self.facts.versions.get() + 1;
        self.facts.versions.set(version);
        let facts = Rc::new(facts);
        panes.insert(id, FactsEntry { from, checked: frame, version, facts: facts.clone() });
        Some((version, facts))
    }

    /// Compose what client `id` sees at `now` (Unix seconds: the accounts' usage windows count
    /// or not by it) into `buf`, which holds its last frame: the rows of panes that did not
    /// change since are left as they are (and returned, so that the encoder does not compare
    /// them), everything else is drawn again.
    pub fn compose(&mut self, id: ClientId, now: i64, buf: &mut Buffer) -> Composed {
        let Some(c) = self.clients.get_mut(&id) else { return (None, Default::default(), Vec::new()) };
        let full = Rect::new(0, 0, c.cols.max(1), c.rows.max(1));
        let mut memo = std::mem::take(&mut c.memo);
        if buf.area != full {
            *buf = Buffer::empty(full);
            memo = FrameMemo::default();
        }
        if c.rows < 3 || c.cols < 10 {
            buf.reset();
            buf.set_string(0, 0, "kraal: terminal too small", Style::default().fg(Color::Yellow));
            return (None, Default::default(), Vec::new());
        }
        // Pickers, dialogs and menus go over the panes.
        let covered = !matches!(c.mode, Mode::Normal | Mode::Resize | Mode::Scroll | Mode::Search { .. });
        let c = &self.clients[&id];
        let rows = c.rows;
        let cols = c.cols;
        // The panes, the sidebar and what goes over them end above the bottom bar.
        let screen = self.screen_above_bar(cols, rows);
        let bottom_row = Rect::new(0, screen.bottom(), cols, self.bottom_rows());
        let sw = self.sidebar_cols(c);
        let side = Rect::new(0, 0, sw, if sw > 0 { screen.height } else { 0 });

        // A client shorter than the one the layout is for sees no folded row below its screen.
        let folds: Vec<(PaneId, Rect)> = self
            .ws_index(c.workspace)
            .and_then(|wi| self.workspaces[wi].tab())
            .map_or_else(Vec::new, |tab| self.stack_rows(tab, self.client_area(id)))
            .into_iter()
            .filter(|(_, row)| row.y < screen.bottom())
            .collect();
        let placed = self.place_panes(id, (screen, side), &folds, (!covered).then_some(&mut memo));
        let c = &self.clients[&id];
        let mut looks = Vec::with_capacity(memo.looks.len());
        let (frames, fold_looks) = self.frame_looks(id, &placed, &folds, (&memo.looks, &mut looks));
        memo.looks = looks;
        // The bars and the sidebar made from the same inputs as the last frame's are the same:
        // they are taken from it rather than made again.
        let mut last = memo.chrome.take();
        let bar_from = self.bar_inputs(c);
        let (bar, bar_same) = match last.as_mut().filter(|l| l.side == side && l.bar_from == bar_from) {
            Some(l) => (std::mem::take(&mut l.bar), true),
            None => (self.top_bar(c), false),
        };
        let sidebar_from = if sw > 0 { self.sidebar_inputs(side.height, now) } else { Inputs::default() };
        let (sidebar, sidebar_same) =
            match last.as_mut().filter(|l| sw > 0 && l.side == side && l.sidebar_from == sidebar_from) {
                Some(l) => (std::mem::take(&mut l.sidebar), true),
                None if sw > 0 => (self.sidebar_rows(c, side.height, now), false),
                None => (Vec::new(), false),
            };
        let bottom_from = if self.bottom_bar { self.bottom_inputs(c) } else { Inputs::default() };
        let (bottom, bottom_same) = match last.as_mut().filter(|l| self.bottom_bar && l.bottom_from == bottom_from) {
            Some(l) => (std::mem::take(&mut l.bottom), true),
            None if self.bottom_bar => (self.bottom_bar(c), false),
            None => (BottomBar::default(), false),
        };
        let chrome = Chrome {
            theme: th(),
            side,
            places: placed.iter().map(|p| (p.pane, p.tile, p.inner, p.content, p.versions.is_some())).collect(),
            sidebar,
            sidebar_from,
            bar,
            bar_from,
            bottom,
            bottom_from,
            frames,
            folds,
            fold_looks,
        };
        // The chrome is drawn again where it differs from the last frame's, when nothing went
        // over it then or does now, and it is laid out the same: then the cells between the
        // panes are blank in both.
        let cached = !covered && apart(&placed, &chrome.folds, side);
        let last = last.filter(|last| cached && last.same_places(&chrome));
        let differs = |now: &Option<Rc<FrameLook>>, then: &Option<Rc<FrameLook>>| match (now, then) {
            (Some(a), Some(b)) => !Rc::ptr_eq(a, b) && a != b,
            (a, b) => a.is_some() != b.is_some(),
        };
        let (kept, sidebar_changed, bar_changed, bottom_changed, frames_changed, folds_changed) = match &last {
            Some(last) => {
                let mut changed = Changed::new(full);
                let sidebar_changed = !sidebar_same && last.sidebar != chrome.sidebar;
                if sidebar_changed {
                    changed.rect(side);
                }
                let bar_changed = !bar_same && last.bar != chrome.bar;
                if bar_changed {
                    changed.rect(Rect::new(side.right(), 0, cols.saturating_sub(side.right()), 1));
                }
                let bottom_changed = !bottom_same && last.bottom != chrome.bottom;
                if bottom_changed {
                    changed.rect(bottom_row);
                }
                let frames: Vec<bool> =
                    chrome.frames.iter().zip(&last.frames).map(|(now, then)| differs(now, then)).collect();
                for (p, _) in placed.iter().zip(&frames).filter(|(_, f)| **f) {
                    changed.border(p.tile);
                }
                let folds: Vec<bool> =
                    chrome.fold_looks.iter().zip(&last.fold_looks).map(|(now, then)| differs(now, then)).collect();
                for ((_, row), _) in chrome.folds.iter().zip(&folds).filter(|(_, f)| **f) {
                    changed.row(row.y, row.x, row.right());
                }
                for p in &placed {
                    match (&p.rows, p.versions.is_some()) {
                        (Some(rows), true) => {
                            for (r, span) in rows.iter().enumerate() {
                                if let Some((from, to)) = span {
                                    let x = p.content.x;
                                    changed.row(p.content.y + r as u16, x + from, (x + to).min(p.content.right()));
                                }
                            }
                        }
                        _ => changed.rect(p.inner),
                    }
                }
                (changed.kept(), sidebar_changed, bar_changed, bottom_changed, frames, folds)
            }
            None => (kept_rows(&placed), true, true, true, vec![true; placed.len()], vec![true; chrome.folds.len()]),
        };
        clear_except(buf, &kept);

        if sw > 0 && sidebar_changed {
            if self.sidebar_presentation(c) == Sidebar::Strip {
                Self::draw_strip(&chrome.sidebar, side, buf, th());
            } else {
                Self::draw_sidebar(&chrome.sidebar, side, buf);
            }
        }
        if bar_changed {
            Self::draw_top_bar(&chrome.bar, cols, buf);
        }
        if bottom_changed {
            chrome.bottom.draw(bottom_row.y, buf);
        }
        if let Some((button, label)) = self.empty_workspace_button(id) {
            let wi = self.ws_index(self.clients[&id].workspace).unwrap();
            let message = if self.workspaces[wi].kept.is_some() { self.workspaces[wi].status() } else { "No panes." };
            if button.y >= self.main_area(&self.clients[&id]).y + 2 {
                buf.set_stringn(
                    button.x,
                    button.y - 2,
                    message,
                    usize::from(button.width),
                    Style::default().fg(th().dim),
                );
            }
            buf.set_stringn(button.x, button.y, label, usize::from(button.width), Style::default().fg(th().accent));
            if self.workspaces[wi].kept.is_some() && button.y + 2 < screen.bottom() {
                buf.set_stringn(
                    button.x,
                    button.y + 2,
                    "Enter to continue",
                    usize::from(button.width),
                    Style::default().fg(th().dim),
                );
            }
        }
        let mut links = crate::ansi::Links::new();
        let mut cursor = self.draw_panes(
            id,
            buf,
            &mut links,
            &placed,
            (&chrome.frames, &frames_changed),
            (&chrome.folds, &chrome.fold_looks, &folds_changed),
        );
        // With every pane drawn from its rows alone, without images, and keys going to programs,
        // the next frame is this one and what a pane printed (see `Server::compose_output`).
        let rows_alone = |p: &Placed| {
            let screen = self.panes.get(&p.pane).and_then(|pane| pane.screen_cache());
            p.versions.is_some() && screen.is_some_and(|rows| !rows.any_images())
        };
        let plain = cached && matches!(self.clients[&id].mode, Mode::Normal) && placed.iter().all(rows_alone);
        memo.rest = plain.then(|| {
            // The directories this frame's facts come from: the next whole frame reads again
            // those read too long ago.
            let read = self.facts.checked_for(crate::pane::frame());
            let held = read.iter().filter_map(|p| self.panes.get(p)?.cwd_held_until());
            Rest {
                chrome: self.chrome_gen,
                facts: chrome.bar_from.panes.clone(),
                cwd_until: held.min(),
                usage_until: chrome.sidebar_from.usage_until,
            }
        });
        if !covered {
            memo.panes = placed.into_iter().filter_map(|p| Some((p.pane, p.content, p.versions?))).collect();
        } else {
            memo.panes.clear();
        }
        memo.chrome = cached.then_some(chrome);
        if let Some(c) = self.clients.get_mut(&id) {
            c.memo = memo;
        }

        let c = &self.clients[&id];
        let overlay_cursor = match &c.mode {
            Mode::Picker(p) => Some(overlays::draw_picker(p, screen, buf)),
            Mode::Prompt(p) => Some(overlays::draw_prompt(p, screen, buf)),
            Mode::ServerForm(p) => Some(super::server_ui::draw_form(p, screen, buf)),
            Mode::TunnelForm(p) => Some(super::tunnel_ui::draw_form(p, screen, buf)),
            Mode::WorkspaceForm(p) => Some(super::workspace_ui::draw_form(p, screen, buf)),
            Mode::AccountForm(form) => Some(super::account_ui::draw_form(form, screen, buf)),
            Mode::PaneForm(form) => Some(super::pane_ui::draw_form(form, screen, buf)),
            Mode::TunnelDetails(details) => {
                self.draw_tunnel_details(details, screen, buf);
                Some(None)
            }
            Mode::AccountDetails(details) => {
                super::account_ui::draw_details(details, screen, buf);
                None
            }
            Mode::Help { scroll } => {
                let keys = [("↑↓", "scroll"), ("esc", "close")];
                overlays::draw_viewer("Help", &self.help_lines(), *scroll, &keys, screen, buf);
                None
            }
            Mode::Messages { scroll } => {
                let keys = [("↑↓", "scroll"), ("esc", "close")];
                overlays::draw_viewer("Messages", &self.message_lines(), *scroll, &keys, screen, buf);
                None
            }
            Mode::Confirm(q) => {
                overlays::draw_confirm(q, screen, buf);
                None
            }
            Mode::Hints(h) => {
                // The labels go over the screen they were made for; if the pane moved, they would
                // point at something else, so there are none (a key then ends hints).
                let place = self.pane_inner_rect(id, h.pane);
                match &h.snapshot {
                    Some(snap) if place == Some(snap.area) => {
                        buf.merge(snap);
                        self.draw_hints(id, h.pane, &h.hints, &h.typed, buf);
                    }
                    Some(_) => {}
                    None => self.draw_hints(id, h.pane, &h.hints, &h.typed, buf),
                }
                None
            }
            Mode::Leader { typed, miss } => {
                let specs: Vec<crate::input::KeySpec> =
                    typed.iter().map(|k| crate::input::KeySpec::new(k.code, k.mods)).collect();
                let mut title = self.keymap.prefix_label();
                if !specs.is_empty() {
                    let keys: Vec<String> = specs.iter().map(|k| crate::keymap::pretty(k, th().ascii)).collect();
                    let named = self.keymap.group_title(&specs).unwrap_or("…");
                    title = format!("{title} {} · {named}", keys.join(" "));
                }
                let entries = self.keymap.menu(typed);
                let root = typed.is_empty();
                let items = overlays::menu_items(&entries, root);
                let footer = overlays::menu_footer(&entries, root);
                let layout = overlays::menu_layout(&items, screen);
                let back = !root;
                overlays::draw_menu(&items, &layout, (&title, &footer), back, miss.as_deref(), buf);
                // Links under the menu would open from it.
                links.retain(|&(x, y), _| !layout.area.contains(ratatui::layout::Position::new(x, y)));
                None
            }
            _ => None,
        };
        let c = &self.clients[&id];
        match &c.mode {
            Mode::Picker(_)
            | Mode::Prompt(_)
            | Mode::ServerForm(_)
            | Mode::TunnelForm(_)
            | Mode::WorkspaceForm(_)
            | Mode::AccountForm(_)
            | Mode::PaneForm(_) => {
                cursor = overlay_cursor.flatten();
            }
            Mode::Leader { .. }
            | Mode::Help { .. }
            | Mode::Messages { .. }
            | Mode::Confirm(_)
            | Mode::Hints(_)
            | Mode::TunnelDetails(_)
            | Mode::AccountDetails(_) => cursor = None,
            Mode::Search { .. } => {
                // After the query, in the top bar.
                cursor = self.top_bar(c).cursor.map(|x| Cursor {
                    x: x.min(c.cols.saturating_sub(1)),
                    y: 0,
                    shape: crate::ansi::CursorShape::Bar,
                    blinking: true,
                });
            }
            _ => {}
        }
        // Overlays (pickers, prompts…) hide the links under them.
        if !matches!(
            self.clients[&id].mode,
            Mode::Normal | Mode::Leader { .. } | Mode::Resize | Mode::Scroll | Mode::Search { .. }
        ) {
            links.clear();
        }
        (cursor, links, kept)
    }

    /// Compose what client `id` sees at `now` into `buf`, its last frame, from what pane
    /// `printed` alone printed since: its rows that changed, and the cursor. Only when the last
    /// frame says the rest still shows the same (see [`Rest`]); None when the frame has to be
    /// composed whole. Nothing is then worked out for the other panes, the bars or the sidebar:
    /// the echo of a key costs the same whatever else is on the screen.
    pub(super) fn compose_output(
        &mut self,
        id: ClientId,
        printed: PaneId,
        now: i64,
        buf: &mut Buffer,
    ) -> Option<Composed> {
        let c = self.clients.get(&id)?;
        let rest = c.memo.rest.as_ref()?;
        let full = Rect::new(0, 0, c.cols.max(1), c.rows.max(1));
        let holds = rest.chrome == self.chrome_gen
            && matches!(c.mode, Mode::Normal)
            && buf.area == full
            && !c.encoder.redraws_all(full)
            // A frame reads again the directories read too long ago, and the contexts being
            // read: both are for a whole frame to do.
            && rest.cwd_until.is_none_or(|until| Instant::now() < until)
            && !self.contexts_reading()
            // A usage window that reset or a last-checked age that changed alters the sidebar.
            && rest.usage_until.is_none_or(|until| now < until);
        if !holds {
            return None;
        }
        // The pane's facts are worked out as for any frame (its directory is read again): with
        // other facts, the chrome changes.
        let facts = rest.facts.iter().find(|f| f.0 == printed)?.1;
        if self.pane_facts(printed)?.0 != facts {
            return None;
        }
        let focus = self.ws_index(c.workspace).and_then(|wi| self.workspaces[wi].tab().and_then(|t| t.focus));
        let mut links = c.encoder.links().clone();
        let mut changed = Changed::new(full);
        // A pane that is not on the screen (another tab, folded in a stack) changes nothing there.
        if let Some(at) = c.memo.panes.iter().position(|p| p.0 == printed) {
            let content = c.memo.panes[at].1;
            let rows = self.panes.get_mut(&printed)?.refresh_screen(content.width, content.height)?;
            let had = &mut self.clients.get_mut(&id)?.memo.panes[at].2;
            if rows.any_images() || had.len() != rows.versions().len() {
                return None;
            }
            let draw = rows_to_draw(rows, had, content.width);
            had.copy_from_slice(rows.versions());
            for (r, (from, to)) in draw.iter().enumerate().filter_map(|(r, columns)| Some((r, (*columns)?))) {
                changed.row(content.y + r as u16, content.x + from, (content.x + to).min(content.right()));
            }
            links.retain(|(x, y), _| !content.contains((*x, *y).into()));
            let kept = changed.kept();
            clear_except(buf, &kept);
            rows.draw(content, buf, |r| draw.get(r).copied().flatten(), &mut links);
            return Some((self.shown_cursor(id, focus), links, kept));
        }
        Some((self.shown_cursor(id, focus), links, changed.kept()))
    }

    /// The cursor of pane `focus` where client `id`'s last frame has its rows, as a frame of
    /// panes drawn from their rows alone shows it.
    fn shown_cursor(&self, id: ClientId, focus: Option<PaneId>) -> Option<Cursor> {
        let (pane, content, _) = self.clients.get(&id)?.memo.panes.iter().find(|p| Some(p.0) == focus)?;
        self.panes.get(pane)?.cursor(*content)
    }

    /// A frame composed from what one pane printed is the frame composed whole, at the same
    /// `now`: every such frame is checked in debug builds, those of the tests among them.
    #[cfg(debug_assertions)]
    pub(super) fn assert_whole(&mut self, id: ClientId, now: i64, buf: &Buffer, (cursor, links, _): &Composed) {
        let memo = self.clients.get_mut(&id).map(|c| std::mem::take(&mut c.memo));
        let mut whole = Buffer::empty(buf.area);
        let (whole_cursor, whole_links, _) = self.compose(id, now, &mut whole);
        if let (Some(c), Some(memo)) = (self.clients.get_mut(&id), memo) {
            c.memo = memo;
        }
        let same = whole == *buf && whole_cursor == *cursor && whole_links == *links;
        assert!(same, "client {id}: the frame composed from one pane's output is not the whole frame");
    }

    /// Where a pane's content is drawn for a client.
    pub(super) fn pane_inner_rect(&self, id: ClientId, pane: PaneId) -> Option<Rect> {
        let wi = self.ws_index(self.clients.get(&id)?.workspace)?;
        let (_, frame, _) = self
            .tab_rects(self.workspaces[wi].tab()?, self.client_area(id))
            .into_iter()
            .find(|(p, _, _)| *p == pane)?;
        Some(self.inner(frame))
    }

    /// Hint labels over the matches of `pane`: the label on the first cells, the rest of the
    /// match underlined. Labels that no longer fit what was typed disappear.
    fn draw_hints(&self, id: ClientId, pane: PaneId, hints: &[crate::hints::Hint], typed: &str, buf: &mut Buffer) {
        let c = &self.clients[&id];
        let Some(wi) = self.ws_index(c.workspace) else { return };
        let Some(tab) = self.workspaces[wi].tab() else { return };
        let Some((_, frame, _)) = self.tab_rects(tab, self.client_area(id)).into_iter().find(|(p, _, _)| *p == pane)
        else {
            return;
        };
        let (_, inner) = self.placed(frame, self.screen_above_bar(c.cols, c.rows));
        let typed = typed.to_ascii_lowercase();
        let label_style = Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD);
        for h in hints {
            let (x, y) = (inner.x + h.m.col, inner.y + h.m.row);
            if y >= inner.y + inner.height || x >= inner.x + inner.width {
                continue;
            }
            let live = h.label.starts_with(&typed);
            let end = (x + h.m.width).min(inner.x + inner.width);
            for cx in x..end {
                let cell = &mut buf[(cx, y)];
                cell.set_style(if live {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::UNDERLINED)
                } else {
                    Style::default().fg(th().dim)
                });
            }
            if live {
                for (i, ch) in h.label.chars().enumerate() {
                    let cx = x + i as u16;
                    if cx >= inner.x + inner.width {
                        break;
                    }
                    let style = if i < typed.len() { label_style.fg(Color::DarkGray) } else { label_style };
                    buf[(cx, y)].set_char(ch).set_style(style);
                }
            }
        }
    }

    // ---- top bar ----------------------------------------------------------------------

    /// The top bar, on the first row, right of the sidebar. Left: the workspace without a full
    /// sidebar to name it, a broadcast in progress (never hidden), then the tabs, or the keys of
    /// the mode in use instead. Right: a message, what the focused pane works on when it has no
    /// frame, agents across workspaces, the menu key unless the bottom bar shows it, and the
    /// session unless it is the default one. When room runs out, the right side gives way first.
    pub fn top_bar(&self, c: &Client) -> TopBar {
        let mut bar = TopBar::default();
        let Some(wi) = self.ws_index(c.workspace) else {
            let left = self.panes_left(c);
            let used = if !matches!(self.sidebar_presentation(c), Sidebar::Full(_)) {
                let label = " Workspaces ";
                bar.items.push((
                    left,
                    label.len() as u16,
                    BarItem::Workspace,
                    vec![Span::styled(label, Style::default().fg(th().accent).add_modifier(Modifier::BOLD))],
                ));
                left + label.len() as u16
            } else {
                left
            };
            bar.right = self.bar_right(c, c.cols.saturating_sub(used + 1));
            return bar;
        };
        let ws = &self.workspaces[wi];
        let cols = c.cols;
        let mut x = self.panes_left(c);
        if !matches!(self.sidebar_presentation(c), Sidebar::Full(_)) {
            // A broadcast in progress always shows, if need be with a shorter workspace name.
            let broadcasting = ws.sync != super::SyncScope::Off && ws.tab().and_then(|t| t.focus).is_some();
            let name_room = if broadcasting { (cols as usize / 4).clamp(1, 20) } else { 20 };
            let name = format!(" {} ", crate::text::truncate(&ws.name, name_room));
            let w = name.width() as u16;
            let style = Style::default().fg(th().accent).add_modifier(Modifier::BOLD);
            bar.items.push((x, w, BarItem::Workspace, vec![Span::styled(name, style)]));
            x += w + 1;
        }
        let room = cols.saturating_sub(x).min(cols / 2 + 8);
        if let Some(spans) = self.broadcast_spans(c, room) {
            let w = spans_width(&spans).min(cols.saturating_sub(x));
            bar.items.push((x, w, BarItem::Text, spans));
            x += w + 1;
        }
        if let Some((spans, cursor)) = self.mode_spans(c, cols.saturating_sub(x)) {
            let w = spans_width(&spans).min(cols.saturating_sub(x));
            bar.cursor = cursor.map(|at| x.saturating_add(at));
            bar.items.push((x, w, BarItem::Text, spans));
            bar.right = self.bar_right(c, cols.saturating_sub(x + w + 1));
            return bar;
        }
        let tabs = self.tab_labels(ws);
        // The active tab keeps its room, with the counts of hidden ones around it.
        let active = tabs.get(ws.active).map_or(0, |t| t.0);
        bar.right = self.bar_right(c, cols.saturating_sub(x + active + 9));
        let limit = cols.saturating_sub(spans_width(&bar.right) + 1);
        place_tabs(&mut bar.items, x, limit, tabs, ws.active);
        bar
    }

    /// The label of each tab with its width: number and name (the active tab filled in), then the
    /// strongest agent state, zoom, broadcast, a bell. A tab with a production pane is red.
    fn tab_labels(&self, ws: &super::Workspace) -> Vec<(u16, Vec<Span<'static>>)> {
        let mut tabs = Vec::new();
        for (i, tab) in ws.tabs.iter().enumerate() {
            let active = i == ws.active;
            let prod = tab.panes().iter().any(|p| self.pane_facts(*p).is_some_and(|(_, f)| f.production.is_some()));
            let mut spans = Vec::new();
            let base = match (active, prod) {
                (true, true) => Style::default().fg(Color::Black).bg(th().production).add_modifier(Modifier::BOLD),
                (true, false) => Style::default().fg(Color::Black).bg(th().accent).add_modifier(Modifier::BOLD),
                (false, true) => Style::default().fg(th().production),
                (false, false) => Style::default().fg(Color::Gray),
            };
            let number = if active || prod { base } else { base.fg(th().dim) };
            spans.push(Span::styled(format!(" {} ", i + 1), number));
            spans.push(Span::styled(format!("{} ", crate::text::truncate(&self.tab_label(tab), 24)), base));
            let mark = |text: &str, color: Color| Span::styled(format!(" {text}"), Style::default().fg(color));
            // Strongest agent state of the tab.
            let state = tab
                .panes()
                .iter()
                .filter_map(|p| self.panes.get(p))
                .filter(|p| p.agent.kind.is_some())
                .map(|p| p.agent.state)
                .max_by_key(|s| match s {
                    AgentState::Blocked => 4,
                    AgentState::Done => 3,
                    AgentState::Working => 2,
                    _ => 0,
                });
            if let Some(s) = state.filter(|s| matches!(s, AgentState::Blocked | AgentState::Done | AgentState::Working))
            {
                spans.push(mark(th().icon(s), th().state_color(s)));
            }
            if tab.zoom.is_some() {
                spans.push(mark("Z", Color::LightBlue));
            }
            let synced = match ws.sync {
                super::SyncScope::Tab => active,
                super::SyncScope::Workspace => true,
                super::SyncScope::Marked => tab.panes().iter().any(|p| ws.marked.contains(p)),
                super::SyncScope::Off => false,
            };
            if synced {
                spans.push(mark("S", th().sync));
            }
            if tab.panes().iter().any(|p| self.panes.get(p).is_some_and(|p| p.bell)) && !active {
                spans.push(mark("!", Color::Red));
            }
            tabs.push((spans_width(&spans), spans));
        }
        tabs
    }

    /// A broadcast in progress, within `room` columns: how many panes get the keys, how many of
    /// them are production panes, and how to stop. With less room the shortcut and the words go
    /// first; the count and PROD stay.
    fn broadcast_spans(&self, c: &Client, room: u16) -> Option<Vec<Span<'static>>> {
        let ws = self.ws_index(c.workspace).map(|i| &self.workspaces[i])?;
        if ws.sync == super::SyncScope::Off {
            return None;
        }
        let targets = ws.sync_targets(ws.tab()?.focus?);
        let n = targets.len();
        let prod = targets.iter().filter(|p| self.pane_facts(**p).is_some_and(|(_, f)| f.production.is_some())).count();
        let prod_text = if prod > 0 { format!(", {prod} PROD") } else { String::new() };
        let full = format!("BROADCAST → {n} pane{}{prod_text}", if n == 1 { "" } else { "s" });
        // The toggle stops any broadcast, and its key is usually the shorter.
        let stop = self.keymap.hint(Action::BroadcastToggle).or_else(|| self.keymap.hint(Action::BroadcastOff));
        let stop_w = stop.as_ref().map_or(0, |k| k.width() + " stop".len());
        let room = room as usize;
        let tiny = format!("B{n}{}", if prod > 0 { " PROD" } else { "" });
        let what =
            [full, format!("BROADCAST {n}{prod_text}")].into_iter().find(|w| w.width() + 2 <= room).unwrap_or(tiny);
        let color = if prod > 0 { th().production } else { th().sync };
        let mut out = vec![badge_span(&what, color)];
        if let Some(stop) = stop.filter(|_| what.width() + 3 + stop_w <= room) {
            out.push(Span::raw(" "));
            out.extend(key_list(&[(&stop, "stop")]));
        }
        Some(out)
    }

    /// The keys of the mode in use, shown in place of the tabs within `room` columns; for a
    /// search, also the column of its cursor. The leader menu, pickers and dialogs say what they
    /// are themselves.
    fn mode_spans(&self, c: &Client, room: u16) -> Option<(Vec<Span<'static>>, Option<u16>)> {
        let t = th();
        let label = c.mode.label();
        let mut out = Vec::new();
        let mut cursor = None;
        match &c.mode {
            Mode::Resize => {
                out.push(badge_span(label, t.accent));
                out.push(Span::raw(" "));
                out.extend(key_list(&[("←↓↑→", "resize"), ("⏎", "done"), ("esc", "done")]));
            }
            Mode::Scroll => {
                out.push(badge_span(label, t.working));
                out.push(Span::raw(" "));
                let prompts = self.client_focus(c.id).and_then(|p| self.panes.get(&p)).is_some_and(|p| p.has_prompts());
                let mut list = vec![("arrows", "move"), ("v", "select"), ("y", "copy"), ("/", "search")];
                if prompts {
                    list.extend([("p P", "prompts"), ("o", "output")]);
                }
                list.push(("esc", "exit"));
                out.extend(key_list(&list));
            }
            Mode::Search { query, found } => {
                out.push(badge_span(label, t.working));
                out.push(Span::raw(" "));
                out.push(Span::styled("/", Style::default().fg(t.working).add_modifier(Modifier::BOLD)));
                // A long query shows its end, where the typing is.
                let shown = (room as usize / 2).saturating_sub(spans_width(&out) as usize).max(8);
                out.push(Span::raw(crate::text::truncate_start(query, shown)));
                cursor = Some(spans_width(&out));
                out.push(Span::raw("  "));
                if !query.is_empty() && !found {
                    out.push(Span::styled("no match", Style::default().fg(t.production)));
                } else {
                    out.extend(key_list(&[("↑↓", "older/newer"), ("⏎", "keep"), ("esc", "cancel")]));
                }
            }
            Mode::Hints(crate::server::ui::HintsMode { hints, act, .. }) => {
                out.push(badge_span(if *act { "HINTS: ACT" } else { "HINTS: COPY" }, t.blocked));
                out.push(Span::raw(" "));
                if *act {
                    let mut acts: Vec<&str> = Vec::new();
                    for h in hints {
                        let a = h.m.kind.action();
                        if !acts.contains(&a) {
                            acts.push(a);
                        }
                    }
                    out.push(dim_span(&format!("type a label to {} · ", acts.join(", "))));
                    out.extend(key_list(&[("tab", "copy instead"), ("esc", "cancel")]));
                } else {
                    out.push(dim_span("type a label to copy it · "));
                    out.extend(key_list(&[("tab", "act on it instead"), ("esc", "cancel")]));
                }
            }
            _ => return None,
        }
        Some((out, cursor))
    }

    /// The right side of the top bar, within `room` columns, most important first: a message,
    /// the focused pane's details, agents, the menu key, the session. A message too long is cut;
    /// the other pieces show whole or not at all.
    fn bar_right(&self, c: &Client, room: u16) -> Vec<Span<'static>> {
        let t = th();
        let normal = matches!(c.mode, Mode::Normal);
        let waiting = self.attention_queue().len();
        let mut notice = Vec::new();
        if let Some((msg, _, kind)) = &c.toast {
            let style = Style::default().fg(kind.colour(t));
            if let Some((_, action)) = &c.toast_action {
                let label = action.label();
                let prefix = msg.strip_suffix(&format!(" · {label}")).unwrap_or(msg);
                let available = room.saturating_sub(1);
                if available >= label.width() as u16 + 4 {
                    let prefix = crate::text::truncate(prefix, usize::from(available) - label.width() - 3);
                    notice.push(Span::styled(format!("{prefix} · "), style));
                    notice.push(Span::styled(label, style.add_modifier(Modifier::UNDERLINED)));
                }
            } else {
                notice.push(Span::styled(msg.clone(), style));
            }
        } else if normal && c.welcome && waiting == 0 {
            // Until the menu is opened once.
            notice.push(Span::styled("Welcome to kraal", Style::default().fg(t.accent).add_modifier(Modifier::BOLD)));
            let find = self.keymap.hint(Action::CommandPalette);
            let help = self.keymap.hint(Action::Help);
            let list: Vec<(&str, &str)> = [(find.as_deref(), "find any command"), (help.as_deref(), "help")]
                .into_iter()
                .filter_map(|(k, what)| Some((k?, what)))
                .collect();
            if !list.is_empty() {
                notice.push(Span::raw("  "));
                notice.extend(key_list(&list));
            }
        } else if normal && waiting > 0 {
            // Long commands that ended out of sight wait too: then they are panes, not agents.
            let agents = self.panes.values().filter(|p| p.agent.state.needs_attention()).count()
                + self.remote_agents().filter(|(_, a)| a.state.needs_attention()).count()
                == waiting;
            notice.push(Span::styled(
                format!(
                    "{} {waiting} {}{} need{} you",
                    t.icon(AgentState::Blocked),
                    if agents { "agent" } else { "pane" },
                    if waiting == 1 { "" } else { "s" },
                    if waiting == 1 { "s" } else { "" }
                ),
                Style::default().fg(t.blocked),
            ));
            if let Some(k) = self.keymap.hint(Action::NextAttention) {
                notice.push(Span::raw(" "));
                notice.extend(key_list(&[(&k, "go")]));
            }
        }
        let menu =
            if normal && !self.bottom_bar { key_list(&[(&self.keymap.prefix_label(), "menu")]) } else { Vec::new() };
        let session = if self.session == "main" {
            Vec::new()
        } else {
            vec![Span::styled(self.session.clone(), Style::default().fg(t.dim))]
        };
        let mut pieces = [notice, self.focused_details(c), self.agent_counts(), menu, session];
        // Two columns between pieces, one free at the right edge.
        let room = room.saturating_sub(1);
        let mut used = 0u32;
        let mut keep = [false; 5];
        for (i, p) in pieces.iter_mut().enumerate() {
            if p.is_empty() {
                continue;
            }
            let gap = if used > 0 { 2 } else { 0 };
            if i == 0 && spans_width(p) > room && room >= 12 {
                *p = fit_spans(std::mem::take(p), room);
            }
            let w = u32::from(spans_width(p));
            if used + gap + w <= u32::from(room) {
                keep[i] = true;
                used += gap + w;
            }
        }
        let mut out = Vec::new();
        for (p, kept) in pieces.into_iter().zip(keep) {
            if kept {
                if !out.is_empty() {
                    out.push(Span::raw("  "));
                }
                out.extend(p);
            }
        }
        if !out.is_empty() {
            out.push(Span::raw(" "));
        }
        out
    }

    /// What the focused pane works on, when it has no frame to show it (pane frames off).
    fn focused_details(&self, c: &Client) -> Vec<Span<'static>> {
        let Some(wi) = self.ws_index(c.workspace) else { return Vec::new() };
        let Some(tab) = self.workspaces[wi].tab() else { return Vec::new() };
        let Some(f) = tab.focus else { return Vec::new() };
        let Some((_, frame, floating)) =
            self.tab_rects(tab, self.client_area(c.id)).into_iter().find(|(p, ..)| *p == f)
        else {
            return Vec::new();
        };
        if floating || self.inner(frame).y > frame.y {
            return Vec::new();
        }
        let Some((_, facts)) = self.pane_facts(f) else { return Vec::new() };
        let mut out = Vec::new();
        if facts.production.is_some() {
            out.push(badge_span("PROD", th().production));
        }
        out.extend(facts.details.iter().cloned());
        out
    }

    /// Agents across all workspaces and the servers watched: how many are blocked, done and
    /// working.
    fn agent_counts(&self) -> Vec<Span<'static>> {
        let (mut blocked, mut working, mut done) = (0, 0, 0);
        let remote = self.remote_agents().map(|(_, a)| a.state);
        for state in self.panes.values().map(|p| p.agent.state).chain(remote) {
            match state {
                AgentState::Blocked => blocked += 1,
                AgentState::Working => working += 1,
                AgentState::Done => done += 1,
                _ => {}
            }
        }
        let mut out: Vec<Span<'static>> = Vec::new();
        for (n, s) in [(blocked, AgentState::Blocked), (done, AgentState::Done), (working, AgentState::Working)] {
            if n > 0 {
                if !out.is_empty() {
                    out.push(Span::raw(" "));
                }
                out.push(Span::styled(
                    format!("{}{n}", th().icon(s)),
                    Style::default().fg(th().state_color(s)).add_modifier(Modifier::BOLD),
                ));
            }
        }
        out
    }

    pub fn tab_bar_hit(&self, id: ClientId, col: u16) -> Option<BarItem> {
        let c = self.clients.get(&id)?;
        let bar = self.top_bar(c);
        if let Some((pane, action)) = c
            .toast_action
            .as_ref()
            .filter(|_| c.toast.as_ref().is_some_and(|(_, until, _)| *until > std::time::Instant::now()))
        {
            let mut x = c.cols.saturating_sub(spans_width(&bar.right) + 1);
            for span in &bar.right {
                let width = span.width() as u16;
                if span.content == action.label()
                    && span.style.add_modifier.contains(Modifier::UNDERLINED)
                    && (x..x + width).contains(&col)
                {
                    return Some(BarItem::Toast(*pane, action.clone()));
                }
                x += width;
            }
        }
        bar.items.into_iter().find(|(x, w, ..)| col >= *x && col < x + w).map(|(_, _, item, _)| item)
    }

    fn draw_top_bar(bar: &TopBar, cols: u16, buf: &mut Buffer) {
        for (x, w, _, spans) in &bar.items {
            buf.set_line(*x, 0, &Line::from(spans.clone()), *w);
        }
        let rw = spans_width(&bar.right);
        if rw > 0 && rw < cols {
            buf.set_line(cols - rw, 0, &Line::from(bar.right.clone()), rw);
        }
    }

    // ---- sidebar ----------------------------------------------------------------------

    fn strip_rows(&self, c: &Client, height: u16, theme: crate::theme::Theme) -> Vec<SideRow> {
        let mut workspaces = Vec::new();
        let mut agents = Vec::new();
        for entry in self.cached_workspace_catalog() {
            match entry {
                WorkspaceEntry::Kept(w) => {
                    let ws = w.workspace;
                    let states: Vec<_> = ws
                        .panes()
                        .into_iter()
                        .filter_map(|p| {
                            self.panes.get(&p).filter(|p| p.agent.kind.is_some()).map(|pane| (p, pane.agent.state))
                        })
                        .collect();
                    workspaces.push(StripEntry {
                        item: SideItem::Workspace(ws.id),
                        state: states.iter().map(|(_, s)| *s).max_by_key(agent_urgency),
                        icon: if ws.closed() { AgentState::Idle } else { AgentState::Working },
                        selected: ws.id == c.workspace,
                    });
                    agents.extend(states.into_iter().map(|(p, state)| StripEntry {
                        item: SideItem::Pane(p),
                        state: Some(state),
                        icon: state,
                        selected: false,
                    }));
                }
                WorkspaceEntry::Source(source) => {
                    workspaces.push(StripEntry {
                        item: SideItem::Definition(source.path.to_path_buf()),
                        state: None,
                        icon: AgentState::Idle,
                        selected: false,
                    });
                }
            }
        }
        workspaces.sort_by_key(|entry| match &entry.item {
            SideItem::Workspace(ws) => self.workspace_position(&super::ui::PickAction::SwitchWorkspace(*ws)),
            SideItem::Definition(path) => {
                crate::persist::WorkspaceKey::position(&self.workspace_order, None, Some(path))
            }
            _ => usize::MAX,
        });
        agents.extend(self.remote_agents().map(|(server, a)| StripEntry {
            item: SideItem::Remote { server: server.to_owned(), pane: a.pane },
            state: Some(a.state),
            icon: a.state,
            selected: false,
        }));

        let room = height.saturating_sub(3) as usize;
        let ws_need = workspaces.len().min(100);
        let ag_need = agents.len().min(100);
        let ws_room = ws_need.min(room.saturating_sub(ag_need.min(room / 2)));
        let ag_room = ag_need.min(room - ws_room);
        let mut rows = vec![strip_label("WS", SideItem::Workspaces, theme)];
        rows.extend(strip_entries(workspaces, ws_room, SideItem::Workspaces, theme));
        rows.push(strip_label("AG", SideItem::Panes, theme));
        rows.extend(strip_entries(agents, ag_room, SideItem::Panes, theme));
        rows.resize(height.saturating_sub(1) as usize, SideRow::Blank);
        if height > 0 {
            rows.push(strip_label(&format!(" {} ", theme.pointer()), SideItem::ToggleSidebar, theme));
        }
        rows
    }

    fn draw_strip(rows: &[SideRow], area: Rect, buf: &mut Buffer, theme: crate::theme::Theme) {
        for (y, row) in (area.y..area.bottom()).zip(rows) {
            for x in area.x..area.right() {
                buf[(x, y)].reset();
            }
            buf[(area.right() - 1, y)].set_symbol(if theme.ascii { "|" } else { "│" }).set_fg(theme.dim);
            if let SideRow::Entry { line, selected, .. } = row {
                if *selected {
                    for x in area.x..area.right() - 1 {
                        buf[(x, y)].set_bg(theme.selection);
                    }
                }
                buf.set_line(area.x, y, line, 3);
            }
        }
    }

    /// The sidebar's rows from the top of the screen, `height` rows in all. Each section opens on
    /// a line that carries its name, the first one on the top border; its entries take two rows,
    /// a name after a state dot and a detail below. Accounts sit at the bottom when there is room,
    /// with their usage at `now`.
    fn sidebar_rows(&self, c: &Client, height: u16, now: i64) -> Vec<SideRow> {
        #[cfg(test)]
        SIDEBAR_BUILDS.with(|n| n.set(n.get() + 1));
        if self.sidebar_presentation(c) == Sidebar::Strip {
            return self.strip_rows(c, height, th());
        }
        let t = th();
        let mut rows = vec![SideRow::Section("WORKSPACES")];
        let dot = |icon: &str, color: Color| Span::styled(format!(" {icon} "), Style::default().fg(color));
        let strong = |text: String| Span::styled(text, Style::default().fg(t.text).add_modifier(Modifier::BOLD));
        let faint = |text: String, color: Color| Span::styled(text, Style::default().fg(color));
        let plural = |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
        let sep = if t.ascii { " - " } else { " · " };

        // Saved workspaces keep their place when opened or closed.
        let mut agents = Vec::new();
        for entry in self.cached_workspace_catalog() {
            match entry {
                WorkspaceEntry::Kept(w) => {
                    let ws = w.workspace;
                    let panes = ws.panes();
                    let states: Vec<(PaneId, AgentState)> = panes
                        .iter()
                        .filter_map(|p| {
                            self.panes
                                .get(p)
                                .filter(|pane| pane.agent.kind.is_some())
                                .map(|pane| (*p, pane.agent.state))
                        })
                        .collect();
                    let color =
                        states.iter().map(|(_, s)| *s).max_by_key(agent_urgency).map_or(t.dim, |s| t.state_color(s));
                    let icon = t.icon(if ws.closed() { AgentState::Idle } else { AgentState::Working });
                    let (tabs, panes) = ws.counts();
                    let detail = match ws.source_status {
                        Some(status) => status.to_string(),
                        None => format!("{}{sep}{}{sep}{}", ws.status(), plural(tabs, "tab"), plural(panes, "pane")),
                    };
                    side_entry(
                        &mut rows,
                        SideItem::Workspace(ws.id),
                        dot(icon, color),
                        strong(ws.name.clone()),
                        faint(detail, t.dim),
                        ws.id == c.workspace,
                    );
                    agents.extend(states.into_iter().map(|(p, s)| (p, s, ws)));
                }
                WorkspaceEntry::Source(source) => {
                    let name = strong(source.name().unwrap().to_owned());
                    let detail = faint("Closed".into(), t.dim);
                    side_entry(
                        &mut rows,
                        SideItem::Definition(source.path.to_path_buf()),
                        dot(t.icon(AgentState::Idle), t.dim),
                        name,
                        detail,
                        false,
                    );
                }
            }
        }

        rows[1..].sort_by_key(|row| match row {
            SideRow::Entry { item: Some(SideItem::Workspace(ws)), .. } => {
                self.workspace_position(&super::ui::PickAction::SwitchWorkspace(*ws))
            }
            SideRow::Entry { item: Some(SideItem::Definition(path)), .. } => {
                crate::persist::WorkspaceKey::position(&self.workspace_order, None, Some(path))
            }
            _ => usize::MAX,
        });

        if let Some(super::ui::Drag::Workspace { drag, .. }) = &c.drag
            && drag.moved
        {
            for row in &mut rows {
                if let SideRow::Entry { item: Some(item), line, .. } = row {
                    let key = match item {
                        SideItem::Workspace(ws) => self.workspace_key(&super::ui::PickAction::SwitchWorkspace(*ws)),
                        SideItem::Definition(path) => Some(crate::persist::WorkspaceKey::Source(path.clone())),
                        _ => None,
                    };
                    if key.is_some() && key == drag.target {
                        line.style = line.style.add_modifier(Modifier::REVERSED);
                    }
                }
            }
        }

        // Reserve the add row even when the workspace list fills the sidebar.
        let visible = height.saturating_sub(3) as usize / 2;
        rows.truncate(1 + visible * 2);
        rows.push(SideRow::Entry {
            item: Some(SideItem::AddWorkspace),
            line: Line::from(strong(" + Add workspace".into())),
            selected: false,
        });

        // Agents, with their state and what runs them; the workspace when it is not this one.
        // Then those of the servers watched, after their server's name.
        let remote: Vec<(&str, &super::remote::RemoteAgent)> = self.remote_agents().collect();
        if !agents.is_empty() || !remote.is_empty() {
            rows.push(SideRow::Blank);
            rows.push(SideRow::Section("AGENTS"));
            for (p, s, ws) in agents {
                let pane = &self.panes[&p];
                let kind = match &pane.spec.account {
                    Some(a) => {
                        self.account_display_at(now).labels.get(&p).cloned().unwrap_or_else(|| a.replace(':', "·"))
                    }
                    None => pane.agent.kind.clone().unwrap_or_default(),
                };
                let mut detail = format!("{}{sep}{kind}", s.label());
                if let Some(pr) = self.pane_pr(p) {
                    detail = format!("{detail}{sep}{}", pr.label(t.ascii, false).0);
                }
                if ws.id != c.workspace {
                    detail = format!("{detail}{sep}{}", ws.name);
                }
                let color = t.state_color(s);
                side_entry(
                    &mut rows,
                    SideItem::Pane(p),
                    dot(t.icon(s), color),
                    strong(pane.display_name()),
                    faint(detail, color),
                    false,
                );
            }
            let path = if t.ascii { " > " } else { " › " };
            for (server, a) in remote {
                let color = t.state_color(a.state);
                let item = SideItem::Remote { server: server.to_string(), pane: a.pane };
                let name =
                    vec![dot(t.icon(a.state), color), faint(format!("{server}{path}"), t.dim), strong(a.name.clone())];
                let detail = format!("{}{sep}{}{sep}{}", a.state.label(), a.agent, a.workspace);
                rows.push(SideRow::Entry { item: Some(item.clone()), line: Line::from(name), selected: false });
                let detail = Line::from(vec![Span::raw("   "), faint(detail, color)]);
                rows.push(SideRow::Entry { item: Some(item), line: detail, selected: false });
            }
        }

        // Tunnels kraal keeps open, and dev servers seen in panes.
        if !self.tunnels.is_empty() {
            rows.push(SideRow::Blank);
            rows.push(SideRow::Section("TUNNELS"));
            for tunnel in &self.tunnels {
                let (icon, color) = match tunnel.state {
                    super::net::TunnelState::Up => (t.icon(AgentState::Working), t.done),
                    super::net::TunnelState::Connecting => (t.icon(AgentState::Idle), t.working),
                    super::net::TunnelState::Down => (t.icon(AgentState::Blocked), t.blocked),
                };
                let detail = format!("{} · localhost:{}", tunnel.state.label(), tunnel.spec.local);
                let name =
                    strong(format!("{} · {}:{}", tunnel.spec.server, tunnel.spec.remote_host, tunnel.spec.remote_port));
                side_entry(
                    &mut rows,
                    SideItem::Tunnel(tunnel.spec.local),
                    dot(icon, color),
                    name,
                    faint(detail, color),
                    false,
                );
            }
        }
        if !self.ports.is_empty() {
            rows.push(SideRow::Blank);
            rows.push(SideRow::Section("PORTS"));
            for (port, pane) in &self.ports {
                let mut name = self.panes.get(pane).map(|p| p.display_name()).unwrap_or_default();
                // Which worktree it serves, when that is not the pane's name already.
                if let Some(wt) = self.pane_worktree_name(*pane).filter(|w| !name.split_whitespace().any(|n| n == w)) {
                    name = format!("{name}{sep}{wt}");
                }
                let icon = t.icon(AgentState::Working);
                side_entry(
                    &mut rows,
                    SideItem::Port(*port),
                    dot(icon, t.accent),
                    strong(format!(":{port}")),
                    faint(name, t.dim),
                    false,
                );
            }
        }

        let room = (height as usize).saturating_sub(rows.len() + 1);
        let display = self.account_display_at(now);
        let width = self.sidebar_cols(c).saturating_sub(3);
        let mut cached = display.sidebar.borrow_mut();
        if cached.as_ref().is_none_or(|rows| {
            rows.width != width
                || rows.room != room
                || rows.threshold != self.accounts.limits.threshold
                || rows.theme != t
        }) {
            let mut items = display.items.clone();
            for item in &mut items {
                if !matches!(item.action, super::ui::PickAction::AccountDetails(_)) {
                    item.detail = super::account_ui::sidebar_summary(&item.detail, width.saturating_sub(2) as usize);
                }
                if let super::ui::PickAction::ReviewAccount(a) = &item.action
                    && crate::text::columns(&item.label) > width as usize
                {
                    item.label =
                        format!("? Account for {}", crate::paths::shorten_home(&crate::paths::expand_tilde(&a.home)));
                }
            }
            *cached = Some(AccountRows {
                width,
                room,
                threshold: self.accounts.limits.threshold,
                theme: t,
                rows: self.account_rows(items, room, now),
            });
        }
        let accounts = &cached.as_ref().unwrap().rows;
        if !accounts.is_empty() {
            let top = (height as usize).saturating_sub(accounts.len() + 1);
            rows.resize_with(top, || SideRow::Blank);
            rows.extend(accounts.iter().cloned().map(|mut row| {
                if let SideRow::Entry { item: Some(SideItem::Account(id)), selected, .. } = &mut row {
                    *selected = c.sidebar_hover.as_ref() == Some(&SideItem::Account(id.clone()))
                        || matches!(&c.mode, Mode::AccountDetails(details) if details.account == *id)
                        || matches!(&c.mode, Mode::Picker(p) if p.current().is_some_and(|i|
                            matches!(&i.action, super::ui::PickAction::AccountDetails(a) if a == id)));
                }
                row
            }));
        }
        rows
    }

    pub fn sidebar_hit(&self, id: ClientId, row: u16) -> Option<SideItem> {
        let c = self.clients.get(&id)?;
        let height = c.rows.saturating_sub(self.bottom_rows());
        if self.sidebar_cols(c) == 0 || row >= height {
            return None;
        }
        // The full sidebar ends with a border; the strip ends with its folding action.
        if self.sidebar_presentation(c) != Sidebar::Strip && row.saturating_add(1) >= height {
            return None;
        }
        match self.sidebar_rows(c, height, crate::accounts::now()).into_iter().nth(row as usize)? {
            SideRow::Entry { item, .. } => item,
            SideRow::Section("ACCOUNTS") => Some(SideItem::Accounts),
            SideRow::Section("WORKSPACES") => Some(SideItem::Workspaces),
            _ => None,
        }
    }

    /// The sidebar: one box over the whole height, its sections split by lines across it.
    fn draw_sidebar(rows: &[SideRow], area: Rect, buf: &mut Buffer) {
        #[cfg(test)]
        SIDEBAR_DRAWS.with(|n| n.set(n.get() + 1));
        if area.width < 4 || area.height < 3 {
            return;
        }
        let set = th().border(false);
        let line = Style::default().fg(th().dim);
        Block::default().borders(Borders::ALL).border_set(set).border_style(line).render(area, buf);
        let (left, right) = th().tees();
        let width = area.width - 2;
        for (i, row) in rows.iter().enumerate() {
            let y = area.y + i as u16;
            if y + 1 >= area.bottom() {
                break;
            }
            match row {
                SideRow::Section(name) => {
                    if i > 0 {
                        buf[(area.x, y)].set_symbol(left).set_style(line);
                        for x in area.x + 1..area.right() - 1 {
                            buf[(x, y)].set_symbol(set.horizontal_top).set_style(line);
                        }
                        buf[(area.right() - 1, y)].set_symbol(right).set_style(line);
                    }
                    let title = Line::from(vec![
                        Span::styled(set.horizontal_top, line),
                        Span::styled(
                            format!(" {name} "),
                            Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD),
                        ),
                    ]);
                    buf.set_line(area.x + 1, y, &title, width);
                }
                SideRow::Entry { item, line: text, selected } => {
                    if *selected {
                        for x in area.x + 1..area.right() - 1 {
                            buf[(x, y)].set_bg(th().selection);
                        }
                    }
                    let actions = (matches!(item, Some(SideItem::Workspace(_)))
                        || *selected && matches!(item, Some(SideItem::Account(_))))
                        && text.spans.first().is_some_and(|s| s.content != "   ");
                    let mut text = text.clone();
                    if matches!(item, Some(SideItem::Workspace(_) | SideItem::Account(_)))
                        && text.spans.first().is_some_and(|s| s.content == "   ")
                    {
                        let sep = if th().ascii { " - " } else { " · " };
                        let mut shown = String::new();
                        let detail: String = text.spans.iter().skip(1).map(|s| s.content.as_ref()).collect();
                        for part in detail.split(sep) {
                            let next = if shown.is_empty() { part.to_string() } else { format!("{shown}{sep}{part}") };
                            if crate::text::columns(&next) > width.saturating_sub(3) as usize {
                                break;
                            }
                            shown = next;
                        }
                        let mut remaining = 3 + shown.len();
                        for span in &mut text.spans {
                            let kept = remaining.min(span.content.len());
                            span.content = span.content[..kept].to_owned().into();
                            remaining -= kept;
                        }
                    }
                    buf.set_line(area.x + 1, y, &text, width.saturating_sub(if actions { 3 } else { 0 }));
                    if actions {
                        buf.set_string(
                            area.right() - 4,
                            y,
                            if th().ascii { "..." } else { " ⋮ " },
                            Style::default().fg(th().accent),
                        );
                    }
                }
                SideRow::Blank => {}
            }
        }
    }

    pub(super) fn empty_workspace_button(&self, id: ClientId) -> Option<(Rect, &'static str)> {
        let c = self.clients.get(&id)?;
        let ws = &self.workspaces[self.ws_index(c.workspace)?];
        if !ws.tabs.is_empty() {
            return None;
        }
        let area = self.main_area(c);
        let width = area.width.min(40);
        let label = if ws.close_error.is_some() {
            "Retry Close"
        } else if ws.closed() {
            "Open workspace"
        } else {
            "+ Add pane"
        };
        Some((Rect::new(area.x + (area.width - width) / 2, area.y + area.height / 2, width, 1), label))
    }

    // ---- panes ------------------------------------------------------------------------

    /// The panes client `id` sees, in drawing order, and for those drawn from their screen rows,
    /// which rows its last frame already has: with `memo` (nothing is to go over the panes), the
    /// rows of a pane at the same place with the same versions, when nothing else is drawn on
    /// its content (`side` is the sidebar's place) and they show no image.
    fn place_panes(
        &mut self,
        id: ClientId,
        (screen, side): (Rect, Rect),
        folds: &[(PaneId, Rect)],
        mut memo: Option<&mut FrameMemo>,
    ) -> Vec<Placed> {
        let Some(wi) = self.clients.get(&id).and_then(|c| self.ws_index(c.workspace)) else { return Vec::new() };
        let Some(tab) = self.workspaces[wi].tab() else { return Vec::new() };
        let rects = self.tab_rects(tab, self.client_area(id));
        let mut placed: Vec<Placed> = rects
            .into_iter()
            .map(|(pane, frame, floating)| {
                let (tile, inner) = self.placed(frame, screen);
                Placed { pane, tile, inner, floating, content: inner, rows: None, versions: None }
            })
            .filter(|p| !p.tile.is_empty())
            .collect();
        // The rows of panes folded in stacks (`folds`) are drawn too.
        let folded = folds.iter().map(|(p, r)| (*p, r.intersection(screen)));
        let tiles: Vec<(PaneId, Rect)> = placed.iter().map(|p| (p.pane, p.tile)).chain(folded).collect();
        for (i, p) in placed.iter_mut().enumerate() {
            let Some(pane) = self.panes.get_mut(&p.pane) else { continue };
            p.content = Rect::new(p.inner.x, p.inner.y, p.inner.width.min(pane.cols), p.inner.height.min(pane.rows));
            if p.inner.is_empty() {
                continue;
            }
            let content = p.content;
            let exited = pane.exit_code.is_some();
            let Some(rows) = pane.refresh_screen(content.width, content.height) else { continue };
            let alone = memo.is_some()
                && !p.floating
                && !exited
                && content.y > 0
                && !content.intersects(side)
                && tiles.iter().all(|(q, t)| *q == p.pane || !t.intersects(content));
            let versions = rows.versions();
            // Where the pane was in the last frame, most often.
            let mut same = memo.as_deref_mut().and_then(|m| {
                let at = |e: &(PaneId, Rect, Vec<u64>)| e.0 == p.pane && e.1 == content;
                let i = if m.panes.get(i).is_some_and(at) { i } else { m.panes.iter().position(at)? };
                Some(std::mem::take(&mut m.panes[i].2))
            });
            p.rows = Some(match &same {
                // Nothing new: no row to draw (an empty list).
                Some(old) if alone && old == versions && !rows.any_images() => Vec::new(),
                Some(old) if alone => rows_to_draw(rows, old, content.width),
                _ => vec![Some((0, content.width)); versions.len()],
            });
            p.versions = alone.then(|| match same.take() {
                Some(mut old) if old.len() == versions.len() => {
                    old.copy_from_slice(versions);
                    old
                }
                _ => versions.to_vec(),
            });
        }
        placed
    }

    /// Draw the rows of the panes folded in stacks (`folds`: where, their frames' looks, and
    /// whether to draw each), then the panes `placed`: their frames (`looks`) where `frames` says
    /// to, and the rows of their content the frame does not have yet.
    #[allow(clippy::type_complexity)]
    fn draw_panes(
        &mut self,
        id: ClientId,
        buf: &mut Buffer,
        links: &mut crate::ansi::Links,
        placed: &[Placed],
        (looks, frames): (&[Option<Rc<FrameLook>>], &[bool]),
        folds: (&[(PaneId, Rect)], &[Option<Rc<FrameLook>>], &[bool]),
    ) -> Option<Cursor> {
        let c = &self.clients[&id];
        let wi = self.ws_index(c.workspace)?;
        let images = self.can_show_images(id);
        let tab = self.workspaces[wi].tab()?;
        let focus = tab.focus;
        let show_floating = tab.show_floating;
        let floating_focus = focus.is_some_and(|f| tab.is_floating(f));
        // A pane folded in a stack shows the top line of its frame, and nothing else.
        for (((_, row), look), _) in folds.0.iter().zip(folds.1).zip(folds.2).filter(|(_, f)| **f) {
            let mut whole = Buffer::empty(Rect::new(row.x, row.y, row.width, 3));
            if let Some(look) = look {
                Self::draw_frame(look, &mut whole);
            }
            for x in row.x..row.right() {
                if buf.area.contains((x, row.y).into()) {
                    buf[(x, row.y)] = whole[(x, row.y)].clone();
                }
            }
        }
        let mut cursor = None;
        for (i, p) in placed.iter().enumerate() {
            let (pid, tile, inner, floating, content) = (p.pane, p.tile, p.inner, p.floating, p.content);
            if floating {
                Clear.render(tile, buf);
                // A floating pane hides the links of what is under it.
                links.retain(|(x, y), _| !tile.contains((*x, *y).into()));
            }
            let focused = focus == Some(pid) && (!floating_focus || show_floating);
            if let Some(look) = looks.get(i).and_then(Option::as_ref)
                && frames.get(i).copied().unwrap_or(true)
            {
                Self::draw_frame(look, buf);
            }
            // A smaller client may see only the frame's top.
            if inner.is_empty() {
                continue;
            }
            let Some(pane) = self.panes.get_mut(&pid) else { continue };
            match (&p.rows, pane.screen_cache()) {
                (Some(spans), Some(screen)) => {
                    screen.draw(content, buf, |r| spans.get(r).copied().flatten(), links);
                    // Only rows with image placeholders change with the images.
                    let rows: Vec<u16> = (0..content.height).filter(|r| screen.has_images(usize::from(*r))).collect();
                    for r in rows {
                        pane.show_images(Rect::new(content.x, content.y + r, content.width, 1), buf, images);
                    }
                }
                _ => {
                    if let crate::pane::Content::Files(f) = &mut pane.content {
                        f.set_viewer(id);
                    }
                    pane.render(content, buf, links);
                    pane.show_images(content, buf, images);
                }
            }
            if let Some(login) = &pane.spec.login
                && pane.exit_code.is_some()
            {
                let style = Style::default().fg(Color::Black).bg(Color::Yellow);
                for (row, message) in [login.state.message(), login.state.actions()].into_iter().enumerate() {
                    let y = inner.y + inner.height.saturating_sub(2) + row as u16;
                    if y < inner.bottom() {
                        buf.set_stringn(inner.x, y, format!(" {message} "), inner.width as usize, style);
                    }
                }
            } else if let Some(code) = pane.exit_code {
                let msg = match code {
                    _ if pane.suspended => " not started · enter run · x close ".to_string(),
                    Some(c) => format!(" exited ({c}) · enter restart · x close "),
                    None => " exited · enter restart · x close ".to_string(),
                };
                let y = inner.y + inner.height.saturating_sub(1);
                let color = if pane.suspended {
                    Color::Yellow
                } else if code == Some(0) {
                    Color::Gray
                } else {
                    Color::LightRed
                };
                let style = Style::default().fg(Color::Black).bg(color);
                buf.set_stringn(inner.x, y, msg, inner.width as usize, style);
            }
            // The copy-mode cursor shows where a selection will start.
            if focused && matches!(self.clients[&id].mode, Mode::Normal | Mode::Leader { .. } | Mode::Scroll) {
                cursor = self.panes[&pid].cursor(content);
            }
        }
        cursor
    }

    /// What a pane works on: git, contexts (production ones in red), the agent and its state,
    /// how far back it is scrolled.
    fn pane_details(&self, pid: PaneId, with_title: bool) -> Vec<Span<'static>> {
        let Some(pane) = self.panes.get(&pid) else { return Vec::new() };
        let mut out = Vec::new();
        if let Some(g) = self.pane_git(pid) {
            let color = if g.dirty || g.untracked { Color::Yellow } else { th().dim };
            out.push(Span::styled(format!(" {} ", g.label(th().ascii)), Style::default().fg(color)));
        }
        if let Some(pr) = self.pane_pr(pid) {
            let (label, tone) = pr.label(th().ascii, true);
            out.push(Span::styled(format!(" {label} "), Style::default().fg(Self::pr_color(tone))));
        }
        for (label, is_prod) in self.context_labels(pid) {
            let color = if is_prod { th().production } else { th().dim };
            out.push(Span::styled(format!(" {label} "), Style::default().fg(color)));
        }
        if let Some(kind) = &pane.agent.kind {
            let state = pane.agent.state;
            let style = Style::default().fg(th().state_color(state));
            let state = format!(" {} {} ", th().icon(state), state.label());
            // The account, or else the agent unless a displayed title already names it.
            let account = self.pane_account_label(pid);
            let linked = account.is_some();
            let named = with_title
                && pane.display_name().split_whitespace().next().is_some_and(|w| w.eq_ignore_ascii_case(kind));
            match account.or_else(|| (!named).then(|| kind.clone())) {
                Some(label) => {
                    let sep = if th().ascii { "-" } else { "·" };
                    out.push(Span::styled(format!("{state}{sep} "), style));
                    out.push(Span::styled(
                        label,
                        if linked { style.add_modifier(Modifier::UNDERLINED) } else { style },
                    ));
                    out.push(Span::raw(" "));
                }
                None => out.push(Span::styled(state, style)),
            }
        }
        // The last command the shell marked failed: its status, and how long it ran if that
        // was long enough to matter.
        if let Some((code, took)) = pane.last_failure() {
            let took = took.filter(|d| d.as_secs() >= 10).map(|d| format!(" {}", crate::pane::short_duration(d)));
            let mark = if th().ascii { "x" } else { "✗" };
            out.push(Span::styled(
                format!(" {mark} {code}{} ", took.unwrap_or_default()),
                Style::default().fg(Color::LightRed),
            ));
        }
        let offset = pane.display_offset();
        if offset > 0 {
            out.push(Span::styled(format!(" ↑{offset} "), Style::default().fg(Color::LightBlue)));
        }
        out
    }

    /// The frames of the panes `placed` that have one, and of the panes folded in stacks
    /// (`folds`: their rows, each the top of a frame), as client `id` sees them.
    fn frame_looks(
        &self,
        id: ClientId,
        placed: &[Placed],
        folds: &[(PaneId, Rect)],
        (last, now): (&Looks, &mut Looks),
    ) -> (Frames, Frames) {
        let Some(wi) = self.clients.get(&id).and_then(|c| self.ws_index(c.workspace)) else {
            return (placed.iter().map(|_| None).collect(), folds.iter().map(|_| None).collect());
        };
        let ws = &self.workspaces[wi];
        let Some(tab) = ws.tab() else { return (Vec::new(), Vec::new()) };
        let focus = tab.focus;
        let synced: Vec<PaneId> = match (ws.sync, focus) {
            (super::SyncScope::Off, _) | (_, None) => Vec::new(),
            (_, Some(f)) => ws.sync_targets(f),
        };
        let floating_focus = focus.is_some_and(|f| tab.is_floating(f));
        // A look made from the same as the last frame's is that one.
        let mut look = |pane: PaneId, tile: Rect, focused: bool, floating: bool| {
            let (version, _) = self.pane_facts(pane)?;
            let (sync, marked) = (synced.contains(&pane), ws.marked.contains(&pane));
            let from = LookInputs { chrome: self.chrome_gen, version, tile, focused, sync, floating, marked };
            // Where it was in the last frame, most often.
            let i = now.len();
            let then = last.get(i).filter(|l| l.0 == pane).or_else(|| last.iter().find(|l| l.0 == pane));
            let look = match then {
                Some((_, then, look)) if *then == from => look.clone(),
                _ => Rc::new(self.frame_look(pane, tile, focused, sync, floating, marked)?),
            };
            now.push((pane, from, look.clone()));
            Some(look)
        };
        let frames = placed
            .iter()
            .map(|p| {
                if p.inner == p.tile {
                    return None;
                }
                let focused = focus == Some(p.pane) && (!floating_focus || tab.show_floating);
                look(p.pane, p.tile, focused, p.floating)
            })
            .collect();
        let folded =
            folds.iter().map(|(pane, row)| look(*pane, Rect::new(row.x, row.y, row.width, 3), false, false)).collect();
        (frames, folded)
    }

    /// What client `c`'s top bar is made from: the chrome's generation and the facts of the
    /// panes of its workspace; in copy mode, also the output of the focused pane (its prompts
    /// show there).
    fn bar_inputs(&self, c: &Client) -> Inputs {
        let mut panes: Vec<(PaneId, u64)> = self
            .ws_index(c.workspace)
            .map(|wi| self.workspaces[wi].panes())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| Some((p, self.pane_facts(p)?.0)))
            .collect();
        if matches!(c.mode, Mode::Scroll)
            && let Some(p) = self.client_focus(c.id).and_then(|p| self.panes.get(&p))
        {
            panes.push((p.id, p.fed));
        }
        Inputs { chrome: self.chrome_gen, panes, usage_until: None }
    }

    /// What client `c`'s bottom bar is made from: the chrome's generation and the facts of its
    /// focused pane. (A client of another width starts its frames afresh.)
    fn bottom_inputs(&self, c: &Client) -> Inputs {
        let focus = self.client_focus(c.id).and_then(|p| Some((p, self.pane_facts(p)?.0)));
        Inputs { chrome: self.chrome_gen, panes: focus.into_iter().collect(), usage_until: None }
    }

    /// What a sidebar `height` rows high is made from at `now`: the chrome's generation, the
    /// facts of all panes, and the accounts' usage windows that count.
    fn sidebar_inputs(&self, height: u16, now: i64) -> Inputs {
        let mut panes: Vec<(PaneId, u64)> =
            self.panes.keys().filter_map(|p| Some((*p, self.pane_facts(*p)?.0))).collect();
        panes.sort_unstable();
        panes.push((0, u64::from(height)));
        Inputs { chrome: self.chrome_gen, panes, usage_until: self.account_summaries_until(now) }
    }

    /// A pane's frame: its name on the top border, what it works on at the right of it. The
    /// focused pane's frame is in the accent colour, a production one in red, a broadcast target
    /// in the broadcast colour.
    fn frame_look(
        &self,
        pid: PaneId,
        tile: Rect,
        focused: bool,
        sync: bool,
        floating: bool,
        marked: bool,
    ) -> Option<FrameLook> {
        let (_, facts) = self.pane_facts(pid)?;
        let prod = facts.production.is_some();
        let color = if prod {
            th().production
        } else if sync {
            th().sync
        } else if focused {
            th().accent
        } else {
            th().dim
        };
        let mut title = vec![Span::styled(
            format!(" {} ", facts.name),
            if focused {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            },
        )];
        if prod {
            title.push(badge_span("PROD", th().production));
            if sync {
                title.push(Span::styled(" sync ", Style::default().fg(th().sync).add_modifier(Modifier::BOLD)));
            }
        }
        if floating {
            title.insert(0, Span::styled(" ◇", Style::default().fg(color)));
        }
        if marked {
            title.insert(0, Span::styled(" ◆", Style::default().fg(th().sync).add_modifier(Modifier::BOLD)));
        }
        Some(FrameLook { tile, color, title, details: facts.frame_details.clone() })
    }

    pub(super) fn frame_account_hit(&self, pane: PaneId, tile: Rect, floating: bool, marked: bool, col: u16) -> bool {
        if self.inner(tile) == tile {
            return false;
        }
        let Some(look) = self.frame_look(pane, tile, false, false, floating, marked) else { return false };
        let width = spans_width(&look.details);
        if u32::from(spans_width(&look.title)) + u32::from(width) + 4 > u32::from(tile.width) {
            return false;
        }
        let mut x = tile.right() - 2 - width;
        for span in look.details {
            let width = span.width() as u16;
            if span.style.add_modifier.contains(Modifier::UNDERLINED) && (x..x + width).contains(&col) {
                return true;
            }
            x += width;
        }
        false
    }

    fn draw_frame(look: &FrameLook, buf: &mut Buffer) {
        let tile = look.tile;
        let tw = spans_width(&look.title);
        Block::default()
            .borders(Borders::ALL)
            .border_set(th().border(false))
            .border_style(Style::default().fg(look.color))
            .title(Line::from(look.title.clone()))
            .render(tile, buf);
        // The details end one line cell before the corner, and only show whole.
        let rw = spans_width(&look.details);
        if rw > 0 && u32::from(tw) + u32::from(rw) + 4 <= u32::from(tile.width) {
            buf.set_line(tile.right() - 2 - rw, tile.y, &Line::from(look.details.clone()), rw);
        }
    }

    // ---- help and messages -------------------------------------------------------------

    /// The reference, from the keymap in use: the keys after the prefix, each menu, the direct
    /// keys, then the keys of lists, dialogs and modes, and the mouse.
    pub(super) fn help_lines(&self) -> Vec<Line<'static>> {
        let km = &self.keymap;
        let p = km.prefix_label();
        let mut out = vec![
            Line::from(vec![
                Span::raw("Press "),
                Span::styled(p.clone(), Style::default().fg(th().accent).add_modifier(Modifier::BOLD)),
                Span::raw(", then a key: a menu shows what each key does. "),
            ]),
            Line::from(match km.hint(crate::keymap::Action::CommandPalette) {
                Some(k) => format!("Search every command with {k}. Keys preset: {}.", km.preset),
                None => format!("Keys preset: {}.", km.preset),
            }),
        ];
        if !km.menus.is_empty() {
            out.push(Line::from(format!("Lowercase: a theme's main action ({p} s). Capital: its menu ({p} S).")));
        }
        out.push(Line::raw(""));
        let sections = km.reference(th().ascii, false);
        let key_w = sections.iter().flat_map(|(_, v)| v.iter().map(|(k, _)| k.width())).max().unwrap_or(10).min(26);
        for (title, rows) in sections {
            if rows.is_empty() {
                continue;
            }
            out.push(overlays::heading(&title));
            for (k, what) in rows {
                out.push(overlays::entry(&k, &what, key_w));
            }
            out.push(Line::raw(""));
        }
        out.push(Line::from(Span::styled(
            format!(
                "Exited command panes: ⏎ restarts, x closes. Settings: {}",
                crate::paths::shorten_home(&crate::paths::config_path())
            ),
            Style::default().fg(th().dim),
        )));
        out
    }

    /// The messages log, newest first.
    pub(super) fn message_lines(&self) -> Vec<Line<'static>> {
        if self.messages.is_empty() {
            return vec![Line::from(Span::styled("No messages yet.", Style::default().fg(th().dim)))];
        }
        self.messages
            .iter()
            .rev()
            .map(|m| {
                let when = crate::text::local_datetime(crate::text::unix_seconds(m.at));
                let style = Style::default().fg(m.kind.colour(th()));
                Line::from(vec![
                    Span::styled(format!("{}  ", &when[11..]), Style::default().fg(th().dim)),
                    Span::styled(m.text.clone(), style),
                ])
            })
            .collect()
    }
}

/// One entry of the top bar: column, width, what a click does, and the label.
pub type BarEntry = (u16, u16, BarItem, Vec<Span<'static>>);

/// What the top bar shows: entries from the left, the right side, and the column of the cursor
/// while a search is typed.
#[derive(Default, PartialEq)]
pub struct TopBar {
    pub items: Vec<BarEntry>,
    pub right: Vec<Span<'static>>,
    pub cursor: Option<u16>,
}

/// The window of tabs shown from column `x` to `limit`: the active one, then neighbours while
/// they fit. The hidden ones are counted at the edges.
fn place_tabs(out: &mut Vec<BarEntry>, mut x: u16, limit: u16, tabs: Vec<(u16, Vec<Span<'static>>)>, active: usize) {
    let room = limit.saturating_sub(x + 4);
    let active = active.min(tabs.len().saturating_sub(1));
    let (mut lo, mut hi) = (active, active);
    let mut used = tabs.get(active).map_or(0, |t| t.0 + 1);
    loop {
        let marks = |lo: usize, hi: usize| if lo > 0 { 4 } else { 0 } + if hi + 1 < tabs.len() { 4 } else { 0 };
        let mut grew = false;
        if hi + 1 < tabs.len() && used + tabs[hi + 1].0 + 1 + marks(lo, hi + 1) <= room {
            hi += 1;
            used += tabs[hi].0 + 1;
            grew = true;
        }
        if lo > 0 && used + tabs[lo - 1].0 + 1 + marks(lo - 1, hi) <= room {
            lo -= 1;
            used += tabs[lo].0 + 1;
            grew = true;
        }
        if !grew {
            break;
        }
    }
    let more = |n: usize, left: bool| {
        let text = if left { format!("‹{n} ") } else { format!(" {n}›") };
        vec![Span::styled(text, Style::default().fg(th().dim))]
    };
    let n = tabs.len();
    if lo > 0 {
        out.push((x, 3, BarItem::Tab(lo - 1), more(lo, true)));
        x += 4;
    }
    for (i, (w, spans)) in tabs.into_iter().enumerate().skip(lo).take(hi + 1 - lo) {
        out.push((x, w, BarItem::Tab(i), spans));
        x += w + 1;
    }
    if hi + 1 < n {
        out.push((x, 3, BarItem::Tab(hi + 1), more(n - hi - 1, false)));
        x += 4;
    }
    if x + 3 <= limit {
        out.push((x, 3, BarItem::NewTab, vec![Span::styled(" + ", Style::default().fg(th().dim))]));
    }
}

/// Blank every cell of `buf` but those `kept` (sorted by row, then column).
fn clear_except(buf: &mut Buffer, kept: &[Kept]) {
    let area = buf.area;
    let width = usize::from(area.width);
    let mut from = 0;
    for k in kept {
        if k.y < area.y || k.x0 < area.x {
            continue;
        }
        let row = usize::from(k.y - area.y) * width;
        let (start, end) = (row + usize::from(k.x0 - area.x), row + usize::from(k.x1 - area.x));
        if let Some(cells) = buf.content.get_mut(from..start) {
            cells.iter_mut().for_each(ratatui::buffer::Cell::reset);
        }
        from = from.max(end);
    }
    if let Some(cells) = buf.content.get_mut(from..) {
        cells.iter_mut().for_each(ratatui::buffer::Cell::reset);
    }
}

pub(super) fn spans_width(spans: &[Span]) -> u16 {
    spans.iter().map(|s| s.content.width()).sum::<usize>().min(u16::MAX as usize) as u16
}

/// `spans` cut to `max` columns, ending with "…".
fn fit_spans(spans: Vec<Span<'static>>, max: u16) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut left = max as usize;
    for s in spans {
        let w = s.content.width();
        if w <= left {
            left -= w;
            out.push(s);
        } else {
            if left > 0 {
                out.push(Span::styled(crate::text::truncate(&s.content, left), s.style));
            }
            break;
        }
    }
    out
}

impl Server {
    fn account_rows(&self, mut items: Vec<super::ui::PickItem>, room: usize, now: i64) -> Vec<SideRow> {
        if room < 2 {
            return Vec::new();
        }
        let Some(add) = items.pop() else { return Vec::new() };
        let questions: Vec<_> =
            items.extract_if(.., |item| matches!(item.action, super::ui::PickAction::ReviewAccount(_))).collect();
        let question_count = questions.len().min((room - 2) / 2);
        items.truncate((room - 2) / 2 - question_count);
        items.extend(questions.into_iter().take(question_count));
        items.push(add);
        let mut rows = vec![SideRow::Section("ACCOUNTS")];
        for entry in items {
            if let super::ui::PickAction::AccountDetails(id) = &entry.action {
                let account = self.accounts.by_id(id);
                let panes: Vec<_> = self
                    .panes
                    .iter()
                    .filter(|(p, _)| self.pane_account(**p).is_some_and(|a| a.id == *id))
                    .map(|(_, pane)| pane)
                    .collect();
                let count = panes.len();
                let color = panes
                    .iter()
                    .filter_map(|pane| pane.agent.kind.as_ref().map(|_| pane.agent.state))
                    .max_by_key(agent_urgency)
                    .map_or(th().dim, |state| th().state_color(state));
                let summary = account.as_ref().map_or_else(
                    || entry.detail.clone(),
                    |a| super::account_ui::usage_summary(a, self.account_discovery.status(a), self.usage.get(a), now),
                );
                let error = matches!(summary.as_str(), "Sign-in required" | "Not installed")
                    || account.as_ref().is_some_and(|a| {
                        self.usage.peak(a, now).is_some_and(|peak| peak >= f64::from(self.accounts.threshold(a)))
                    });
                let mut detail =
                    vec![Span::styled(summary, Style::default().fg(if error { th().blocked } else { th().dim }))];
                if count > 0 {
                    let sep = if th().ascii { " - " } else { " · " };
                    detail.push(dim_span(&format!("{sep}{count} pane{}", if count == 1 { "" } else { "s" })));
                }
                let icon = th().icon(if count > 0 { AgentState::Working } else { AgentState::Idle });
                side_entry(
                    &mut rows,
                    SideItem::Account(id.clone()),
                    Span::styled(format!(" {icon} "), Style::default().fg(color)),
                    Span::styled(entry.label, Style::default().fg(th().text).add_modifier(Modifier::BOLD)),
                    detail,
                    false,
                );
                continue;
            }
            let item = match entry.action {
                super::ui::PickAction::ReviewAccount(a) => SideItem::ReviewAccount(a.id),
                super::ui::PickAction::AddAccount => SideItem::AddAccount,
                _ => SideItem::Accounts,
            };
            // Indented like the workspaces' rows, and the add row as bold as theirs.
            for (indent, text) in [(" ", entry.label), ("   ", entry.detail)].into_iter().filter(|(_, s)| !s.is_empty())
            {
                let text = format!("{indent}{text}");
                let span = if entry.dim {
                    dim_span(&text)
                } else if matches!(item, SideItem::AddAccount) {
                    Span::styled(text, Style::default().fg(th().text).add_modifier(Modifier::BOLD))
                } else {
                    Span::raw(text)
                };
                rows.push(SideRow::Entry { item: Some(item.clone()), line: Line::from(span), selected: false });
            }
        }
        rows
    }
}

fn badge_span(text: &str, color: Color) -> Span<'static> {
    Span::styled(format!(" {text} "), Style::default().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD))
}

fn dim_span(text: &str) -> Span<'static> {
    Span::styled(text.to_string(), Style::default().fg(th().dim))
}

/// Keys and what they do: "esc close · ⏎ open".
pub(super) fn key_list(list: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for (i, (k, what)) in list.iter().enumerate() {
        if i > 0 {
            out.push(dim_span(" · "));
        }
        out.push(Span::styled(overlays::shown_key(k), Style::default().fg(th().accent).add_modifier(Modifier::BOLD)));
        out.push(dim_span(&format!(" {what}")));
    }
    out
}

#[cfg(test)]
thread_local! {
    static SIDEBAR_BUILDS: Cell<usize> = const { Cell::new(0) };
    static SIDEBAR_DRAWS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::NoticeKind;

    #[test]
    fn workspace_views_share_rows_but_render_only_cached_sources_until_the_next_scan() {
        use crate::protocol::{Request, Response};
        use crate::server::ui::PickAction;
        use crate::workspace::{SOURCE_LOADS, SOURCE_SCANS, WORKSPACE_PARSES};

        let dir = crate::testing::TempDir::new("catalog-views");
        let linked = dir.write("linked.kdl", "layout { pane; }");
        let source = dir.write("source.kdl", "layout { pane; pane; }");
        let broken = dir.write("broken.kdl", "layout {");
        std::os::unix::fs::symlink(&linked, dir.0.join("alias.kdl")).unwrap();
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let current = server.clients[&client].workspace;
        server.workspaces[0].set_definition(Some(&linked));
        server.config.workspace_dirs = vec![dir.text()];
        server.workspace_order = vec![
            crate::persist::WorkspaceKey::Source(source.clone()),
            crate::persist::WorkspaceKey::Id(server.workspaces[0].stable_id.clone()),
        ];
        server.scan_definitions();
        let picker = server.workspace_picker(current);
        assert_eq!(picker.items.len(), 4);
        assert!(matches!(&picker.items[0].action, PickAction::OpenDefinition(p) if p == &source));
        assert_eq!(picker.items[0].detail, "Closed · 1 tab · 2 panes");
        assert_eq!(picker.items[1].action.workspace(), Some(current));
        assert!(matches!(&picker.items[2].action, PickAction::UnavailableDefinition(p) if p == &broken));
        assert!(picker.items[2].detail.starts_with("error: "));
        let Response::Workspaces(infos) = server.request(client, Request::ListWorkspaces).immediate().unwrap() else {
            panic!()
        };
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].name, "source");
        assert_eq!((infos[0].tabs, infos[0].panes), (1, 2));
        assert_eq!(infos[1].id.as_deref(), Some(server.workspaces[0].stable_id.as_str()));

        let items = |rows: Vec<SideRow>| {
            let mut items: Vec<_> = rows
                .into_iter()
                .filter_map(|row| match row {
                    SideRow::Entry {
                        item: Some(item @ (SideItem::Workspace(_) | SideItem::Definition(_))), ..
                    } => Some(item),
                    _ => None,
                })
                .collect();
            items.dedup();
            items
        };
        let counts = || (SOURCE_SCANS.get(), SOURCE_LOADS.get(), WORKSPACE_PARSES.get());
        let before = counts();
        assert!(before.0 > 0 && before.1 > 0 && before.2 > 0);
        std::fs::write(&source, "layout {").unwrap();
        let moved = dir.0.join("moved.kdl");
        std::fs::rename(&linked, &moved).unwrap();
        for _ in 0..3 {
            let expected = vec![SideItem::Definition(source.clone()), SideItem::Workspace(current)];
            assert_eq!(items(server.sidebar_rows(&server.clients[&client], 36, 0)), expected);
            assert_eq!(items(server.strip_rows(&server.clients[&client], 36, th())), expected);
        }
        assert_eq!(counts(), before);
        server.scan_definitions();
        assert_eq!(server.workspaces[0].definition.as_ref(), Some(&moved));
        assert_eq!(items(server.sidebar_rows(&server.clients[&client], 36, 0)), [SideItem::Workspace(current)]);
        assert_eq!(items(server.strip_rows(&server.clients[&client], 36, th())), [SideItem::Workspace(current)]);
        std::fs::remove_file(&moved).unwrap();
        server.scan_definitions();
        assert_eq!(server.workspaces[0].source_status, Some("Source file missing"));
        assert_eq!(items(server.sidebar_rows(&server.clients[&client], 36, 0)), [SideItem::Workspace(current)]);
        assert_eq!(items(server.strip_rows(&server.clients[&client], 36, th())), [SideItem::Workspace(current)]);
    }

    #[test]
    fn an_agent_without_an_account_is_named_once_in_its_frame() {
        let _batch = crate::pane::batch();
        let (tx, _rx) = super::super::event_channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let ws = server.clients[&client].workspace;
        let spec = crate::pane::PaneSpec { command: vec!["true".into()], ..Default::default() };
        let pane =
            server.spawn_pane_in(ws, None, spec, crate::protocol::Placement::Tab, Some("gemini".into())).unwrap();
        let agent = &mut server.panes.get_mut(&pane).unwrap().agent;
        agent.kind = Some("gemini".into());
        agent.state = crate::agent::AgentState::Idle;
        let details = |server: &Server| {
            server
                .frame_look(pane, Rect::new(0, 0, 124, 36), true, false, false, false)
                .unwrap()
                .details
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        let text = details(&server);
        assert!(text.contains("idle") && !text.contains("gemini"), "the title names the agent already: {text:?}");
        server.panes.get_mut(&pane).unwrap().name = Some("review".into());
        server.chrome_changed();
        assert!(details(&server).contains("idle · gemini"), "{:?}", details(&server));
    }

    #[test]
    fn a_frameless_agent_is_named_in_the_top_bar() {
        let _batch = crate::pane::batch();
        let (tx, _rx) = super::super::event_channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        server.config.pane_frames = false;
        let ws = server.clients[&client].workspace;
        let spec = crate::pane::PaneSpec { command: vec!["true".into()], ..Default::default() };
        let pane =
            server.spawn_pane_in(ws, None, spec, crate::protocol::Placement::Tab, Some("gemini".into())).unwrap();
        server.workspaces[0].tab_mut().unwrap().name = "work".into();
        let agent = &mut server.panes.get_mut(&pane).unwrap().agent;
        agent.kind = Some("gemini".into());
        agent.state = AgentState::Idle;
        let bar = server.top_bar(&server.clients[&client]);
        let text: String = bar.right.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("idle · gemini"), "the top bar must name the frameless agent: {text:?}");
    }

    #[test]
    fn strip_cells_colours_selection_and_state_changes_at_the_delivery_sizes() {
        let _batch = crate::pane::batch();
        let (tx, _rx) = super::super::event_channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        server.bottom_bar = true;
        server.primary = Some(client);
        let api = server.clients[&client].workspace;
        server.workspaces[0].name = "api".into();
        let docs = server.new_workspace("docs".into(), None);
        let mut panes = Vec::new();
        for (ws, name) in [(api, "review"), (docs, "writer")] {
            let pane = server
                .spawn_pane_in(
                    ws,
                    None,
                    crate::pane::PaneSpec { command: vec!["true".into()], ..Default::default() },
                    crate::protocol::Placement::Tab,
                    Some(name.into()),
                )
                .unwrap();
            let agent = &mut server.panes.get_mut(&pane).unwrap().agent;
            agent.kind = Some("claude".into());
            agent.state = AgentState::Working;
            panes.push(pane);
        }
        server.workspaces[0].active = 1;
        for (cols, height, size) in [(124, 36, (118, 32)), (80, 24, (74, 20)), (60, 20, (54, 16))] {
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows, c.sidebar) = (cols, height, Some(false));
            server.relayout();
            assert_eq!((server.panes[&panes[0]].cols, server.panes[&panes[0]].rows), size);
            assert_eq!(server.client_area(client).x, 4);
            for theme in [crate::theme::Theme::DEFAULT, crate::theme::Theme::ASCII, crate::theme::Theme::HIGH_CONTRAST]
            {
                for state in
                    [AgentState::Blocked, AgentState::Working, AgentState::Done, AgentState::Idle, AgentState::Unknown]
                {
                    server.panes.get_mut(&panes[0]).unwrap().agent.state = state;
                    server.mark_dirty();
                    let rows = server.strip_rows(&server.clients[&client], height - 1, theme);
                    let mut buf = Buffer::empty(Rect::new(0, 0, cols, height));
                    Server::draw_strip(&rows, Rect::new(0, 0, 4, height - 1), &mut buf, theme);
                    let line = |y| -> String { (0..4).map(|x| buf[(x, y)].symbol()).collect() };
                    let sep = if theme.ascii { "|" } else { "│" };
                    assert_eq!(line(0), format!("WS {sep}"));
                    assert_eq!(line(1), format!(" 1{}{sep}", theme.icon(AgentState::Working)));
                    assert_eq!(line(2), format!(" 2{}{sep}", theme.icon(AgentState::Working)));
                    assert_eq!(line(3), format!("AG {sep}"));
                    assert_eq!(line(4), format!(" 1{}{sep}", theme.icon(state)));
                    assert_eq!(line(5), format!(" 2{}{sep}", theme.icon(AgentState::Working)));
                    for x in 0..3 {
                        assert_eq!(buf[(x, 1)].bg, theme.selection);
                        assert_ne!(buf[(x, 2)].bg, theme.selection);
                    }
                    assert!(buf[(1, 1)].modifier.contains(Modifier::BOLD));
                    for y in [1, 4] {
                        assert_eq!(buf[(2, y)].fg, theme.state_color(state));
                    }
                    for y in [2, 5] {
                        assert_eq!(buf[(2, y)].fg, theme.working);
                    }
                    assert_eq!(line(height - 2), format!(" {} {sep}", theme.pointer()));
                    for y in 6..height - 2 {
                        assert_eq!(line(y), format!("   {sep}"));
                    }
                    assert_eq!(server.panes[&panes[0]].agent.state, state, "drawing never acknowledges done");
                    assert_eq!(server.sidebar_hit(client, 1), Some(SideItem::Workspace(api)));
                    assert_eq!(server.sidebar_hit(client, 4), Some(SideItem::Pane(panes[0])));
                    assert_eq!(server.sidebar_hit(client, height - 2), Some(SideItem::ToggleSidebar));
                    assert_eq!(server.sidebar_hit(client, height - 1), None);
                }
            }
        }
    }

    #[test]
    fn strip_overflow_keeps_original_numbers_current_workspace_and_urgent_agents() {
        let theme = th();
        let entries = |selected| {
            (0..12)
                .map(|i| StripEntry {
                    item: SideItem::Workspace(i),
                    state: None,
                    icon: AgentState::Working,
                    selected: i == selected,
                })
                .collect()
        };
        let rows = strip_entries(entries(10), 4, SideItem::Workspaces, theme);
        let labels: Vec<_> = rows
            .iter()
            .map(|r| match r {
                SideRow::Entry { line, .. } => line.to_string(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(labels, [" 1●", " 2●", "11●", " ⋮ "]);
        assert!(matches!(&rows[2], SideRow::Entry { selected: true, item: Some(SideItem::Workspace(10)), .. }));
        let agents =
            [AgentState::Idle, AgentState::Working, AgentState::Done, AgentState::Blocked, AgentState::Unknown]
                .into_iter()
                .enumerate()
                .map(|(i, state)| StripEntry {
                    item: SideItem::Pane(i as u32),
                    state: Some(state),
                    icon: state,
                    selected: false,
                })
                .collect();
        let rows = strip_entries(agents, 3, SideItem::Panes, theme);
        assert!(matches!(&rows[0], SideRow::Entry { item: Some(SideItem::Pane(2)), .. }));
        assert!(matches!(&rows[1], SideRow::Entry { item: Some(SideItem::Pane(3)), .. }));
        assert!(matches!(&rows[2], SideRow::Entry { item: Some(SideItem::Panes), .. }));
        let entries = (0..105)
            .map(|i| StripEntry {
                item: SideItem::Workspace(i),
                state: None,
                icon: AgentState::Working,
                selected: i == 100,
            })
            .collect();
        let rows = strip_entries(entries, 120, SideItem::Workspaces, theme);
        assert_eq!(rows.len(), 100);
        assert!(matches!(&rows[99], SideRow::Entry { item: Some(SideItem::Workspaces), selected: true, .. }));
        for row in rows {
            if let SideRow::Entry { line, .. } = row {
                assert_eq!(line.width(), 3);
            }
        }
    }

    #[test]
    fn strip_workspace_colour_uses_its_most_urgent_agent() {
        let _batch = crate::pane::batch();
        let (tx, _rx) = super::super::event_channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let ws = server.clients[&client].workspace;
        let states =
            [AgentState::Unknown, AgentState::Idle, AgentState::Working, AgentState::Done, AgentState::Blocked];
        let mut panes = Vec::new();
        for state in states {
            let pane = server
                .spawn_pane_in(ws, None, crate::pane::PaneSpec::default(), crate::protocol::Placement::Tab, None)
                .unwrap();
            let agent = &mut server.panes.get_mut(&pane).unwrap().agent;
            agent.kind = Some("claude".into());
            agent.state = state;
            panes.push(pane);
        }
        for (pane, state) in panes.into_iter().zip(states).rev() {
            let rows = server.strip_rows(&server.clients[&client], 20, th());
            let SideRow::Entry { line, .. } = &rows[1] else { panic!() };
            assert_eq!(line.spans[1].style.fg, Some(th().state_color(state)));
            server.panes.get_mut(&pane).unwrap().agent.kind = None;
        }
    }

    #[test]
    fn strip_and_full_sidebar_transitions_match_a_fresh_frame() {
        let _batch = crate::pane::batch();
        let (tx, _rx) = super::super::event_channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        server.primary = Some(client);
        server.bottom_bar = true;
        let pane = server
            .spawn_pane_in(
                server.clients[&client].workspace,
                None,
                crate::pane::PaneSpec::default(),
                crate::protocol::Placement::Auto,
                None,
            )
            .unwrap();
        server.pane_output(pane, b"sidebar transition\r\n");
        let mut buf = Buffer::empty(Rect::default());
        for (cols, sidebar) in [
            (124, true),
            (124, false),
            (80, false),
            (60, true),
            (39, true),
            (40, true),
            (83, true),
            (84, true),
            (84, false),
            (124, true),
        ] {
            let c = server.clients.get_mut(&client).unwrap();
            c.cols = cols;
            c.sidebar = Some(sidebar);
            server.relayout();
            server.compose(client, 1_000, &mut buf);
            let mut fresh = Buffer::empty(buf.area);
            server.clients.get_mut(&client).unwrap().memo = FrameMemo::default();
            server.compose(client, 1_000, &mut fresh);
            assert_eq!(buf, fresh, "{cols} columns, full requested: {sidebar}");
            let full = matches!(server.sidebar_presentation(&server.clients[&client]), Sidebar::Full(_));
            assert_eq!(
                server
                    .top_bar(&server.clients[&client])
                    .items
                    .iter()
                    .any(|(_, _, item, _)| *item == BarItem::Workspace),
                !full
            );
        }
    }

    #[test]
    fn strip_keeps_workspace_order_closed_markers_and_access_in_short_terminals() {
        let (tx, _rx) = super::super::event_channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let current = server.clients[&client].workspace;
        let closed = server.new_workspace("closed".into(), None);
        super::super::persistence::tests::writer(&mut server, vec![Ok(()), Ok(())]);
        server.keep_and_close_workspace(closed).unwrap();
        let source = PathBuf::from("/unopened/strip.kdl");
        server.def_files.push((source.clone(), "file".into()));
        server.workspace_order = vec![
            crate::persist::WorkspaceKey::Source(source.clone()),
            server.workspace_key(&super::super::ui::PickAction::SwitchWorkspace(closed)).unwrap(),
            server.workspace_key(&super::super::ui::PickAction::SwitchWorkspace(current)).unwrap(),
        ];
        let c = server.clients.get_mut(&client).unwrap();
        (c.cols, c.rows) = (60, 20);
        let check = |server: &Server, closed_icon| {
            let rows = server.strip_rows(&server.clients[&client], 20, th());
            for (y, item, text) in [
                (1, SideItem::Definition(source.clone()), " 1○".to_string()),
                (2, SideItem::Workspace(closed), format!(" 2{closed_icon}")),
                (3, SideItem::Workspace(current), " 3●".to_string()),
            ] {
                assert_eq!(server.sidebar_hit(client, y), Some(item));
                let SideRow::Entry { line, .. } = &rows[y as usize] else { panic!() };
                assert_eq!(line.to_string(), text);
                assert_eq!(line.spans[1].style.fg, Some(th().dim));
            }
        };
        check(&server, "○");
        server.reopen_workspace(closed).unwrap();
        check(&server, "●");
        for height in 3..8 {
            let rows = server.strip_rows(&server.clients[&client], height, th());
            assert_eq!(rows.len(), height as usize);
            assert!(matches!(&rows[0], SideRow::Entry { item: Some(SideItem::Workspaces), .. }));
            assert!(rows.iter().any(|r| matches!(r, SideRow::Entry { item: Some(SideItem::Panes), .. })));
            assert!(matches!(rows.last(), Some(SideRow::Entry { item: Some(SideItem::ToggleSidebar), .. })));
        }
        server.removed_sources.push(source.clone());
        assert!(!(0..20).any(|y| server.sidebar_hit(client, y) == Some(SideItem::Definition(source.clone()))));
    }

    #[test]
    fn notice_colours_in_bars_overlays_and_messages_for_local_and_remote_clients() {
        use crate::protocol::{ClipboardOrigin, ClipboardOutcome};

        for remote in [false, true] {
            for (cols, rows) in [(80, 24), (124, 36)] {
                for (outcome, kind, colour) in [
                    (ClipboardOutcome::Native, NoticeKind::Notice, th().text),
                    (ClipboardOutcome::Busy, NoticeKind::Warning, th().blocked),
                    (ClipboardOutcome::Failed, NoticeKind::Error, th().blocked),
                ] {
                    let (tx, _rx) = super::super::event_channel(32);
                    let mut server = super::super::tests::server(tx);
                    let (id, _peer) = super::super::tests::client(&mut server);
                    let c = server.clients.get_mut(&id).unwrap();
                    (c.cols, c.rows, c.remote) = (cols, rows, remote);
                    c.clipboard.pending = Some((1, ClipboardOrigin::User, Instant::now()));
                    server.clipboard_result(id, 1, outcome, false);
                    assert_eq!(server.clients[&id].toast.as_ref().unwrap().2, kind);
                    assert_eq!(server.messages.back().unwrap().kind, kind);

                    for mode in [Mode::Normal, Mode::Help { scroll: 0 }, Mode::Messages { scroll: 0 }] {
                        let messages = matches!(mode, Mode::Messages { .. });
                        server.clients.get_mut(&id).unwrap().mode = mode;
                        server.mark_dirty();
                        let mut buf = Buffer::empty(Rect::new(0, 0, cols, rows));
                        server.compose(id, 1_000, &mut buf);
                        let mut found = Vec::new();
                        for y in 0..rows {
                            let line: String = (0..cols).map(|x| buf[(x, y)].symbol()).collect();
                            if let Some(start) = line.find(outcome.message()) {
                                let x = line[..start].chars().count() as u16;
                                for x in x..x + outcome.message().len() as u16 {
                                    assert_eq!(buf[(x, y)].fg, colour, "{remote} {cols}x{rows}: {line}");
                                }
                                found.push(y);
                            }
                        }
                        assert!(found.contains(&0), "the notice line remains visible over overlays");
                        assert_eq!(found.len(), if messages { 2 } else { 1 });
                    }
                }
            }
        }
    }

    #[test]
    fn tunnel_sidebar_shows_state_and_click_opens_details() {
        let (tx, _rx) = super::super::event_channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let c = server.clients.get_mut(&client).unwrap();
        (c.cols, c.rows) = (124, 36);
        c.sidebar = Some(true);
        for (state, label) in [
            (super::super::net::TunnelState::Connecting, "Connecting"),
            (super::super::net::TunnelState::Up, "Ready"),
            (super::super::net::TunnelState::Down, "Error"),
        ] {
            server.tunnels = vec![super::super::net::test_tunnel(42, 15432, state)];
            server.mark_dirty();
            let rows = server.sidebar_rows(&server.clients[&client], 35, 1_000);
            let shown: Vec<_> = rows
                .iter()
                .filter_map(|row| match row {
                    SideRow::Entry { item: Some(SideItem::Tunnel(15432)), line, .. } => Some(line.to_string()),
                    _ => None,
                })
                .collect();
            assert!(shown.iter().any(|line| line.contains(label)), "{shown:?}");
            assert!(shown.iter().any(|line| line.contains("localhost:15432")), "{shown:?}");
            let y = (0..36).find(|&y| matches!(server.sidebar_hit(client, y), Some(SideItem::Tunnel(15432)))).unwrap();
            server.clients.get_mut(&client).unwrap().mode = Mode::Normal;
            server.client_input(client, format!("\x1b[<0;2;{}M", y + 1).as_bytes());
            assert!(matches!(server.clients[&client].mode, Mode::TunnelDetails(_)));
        }
    }

    #[test]
    fn printing_four_panes_does_not_repeat_account_work() {
        printing_account_work(false);
    }

    #[test]
    fn printing_agent_frames_does_not_repeat_account_work() {
        printing_account_work(true);
    }

    fn printing_account_work(agents: bool) {
        let _batch = crate::pane::batch();
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let mut account = server.accounts.get("claude", "default").unwrap();
        account.id = "printing-account".into();
        account.name = "Work".into();
        account.home = "/nonexistent/kraal-printing-account".into();
        server.accounts.add(account.clone()).unwrap();
        let panes: Vec<_> = (0..4)
            .map(|_| {
                let spec = crate::pane::PaneSpec {
                    command: vec!["true".into()],
                    account: Some("claude:Work".into()),
                    account_id: Some(account.id.clone()),
                    ..Default::default()
                };
                let pane = server.spawn_pane(None, spec).unwrap();
                if agents {
                    server.panes.get_mut(&pane).unwrap().agent.kind = Some("claude".into());
                }
                pane
            })
            .collect();
        let mut buf = Buffer::empty(Rect::new(0, 0, 124, 36));
        server.compose(client, 1_000, &mut buf);
        crate::accounts::HOME_CHECKS.with(|n| n.set(0));
        super::super::account_ui::ACCOUNT_BUILDS.with(|n| n.set(0));
        SIDEBAR_BUILDS.with(|n| n.set(0));
        SIDEBAR_DRAWS.with(|n| n.set(0));
        for frame in 0..100 {
            for &pane in &panes {
                server.pane_output(pane, b"a log line\r\n");
            }
            // The periodic tick invalidates chrome even when the accounts have not changed.
            if frame % 10 == 0 {
                server.chrome_changed();
            }
            let _frame = crate::pane::Frame::start();
            server.show_what_output_changed();
            server.compose(client, 1_000, &mut buf);
        }
        let checks = crate::accounts::HOME_CHECKS.with(Cell::get);
        let accounts = super::super::account_ui::ACCOUNT_BUILDS.with(Cell::get);
        let builds = SIDEBAR_BUILDS.with(Cell::get);
        let draws = SIDEBAR_DRAWS.with(Cell::get);
        eprintln!(
            "400 output chunks, 100 frames, 10 chrome invalidations: {checks} home checks, {accounts} account builds, {builds} sidebar builds, {draws} sidebar draws"
        );
        assert_eq!(checks, 0, "printing must not resolve account homes again");
        assert_eq!(accounts, 0, "printing must not rebuild account labels and summaries");
        assert_eq!(draws, 0, "unchanged sidebar cells must not be redrawn");
    }

    #[test]
    fn cached_account_rows_follow_available_space_and_keep_click_targets() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let mut account = server.accounts.get("claude", "default").unwrap();
        account.name = "Work".into();
        server.accounts.add(account.clone()).unwrap();
        for (width, height) in [(28, 36), (12, 24), (28, 7), (28, 36)] {
            server.config.sidebar.width = width;
            let c = &server.clients[&client];
            let rows = server.sidebar_rows(c, height, 1_000);
            assert!(rows.iter().any(|row| matches!(row, SideRow::Entry { item: Some(SideItem::AddAccount), .. })));
            if height > 7 {
                let details: Vec<_> = rows
                    .iter()
                    .filter_map(|row| match row {
                        SideRow::Entry { item: Some(SideItem::Account(id)), line, .. } if *id == account.id => {
                            Some(line.to_string())
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(details, [" ○ Claude · Work", "   No usage yet"]);
            }
            server.account_display.borrow_mut().take();
            assert!(rows == server.sidebar_rows(c, height, 1_000), "cached rows differ at {width}x{height}");
        }
    }

    #[test]
    fn screen_close_marks_the_workspace_on_screen_as_current() {
        for (cols, rows) in [(80, 24), (124, 36)] {
            let (tx, _rx) = super::super::queue::channel(32);
            let mut server = super::super::tests::server(tx);
            let (client, _peer) = super::super::tests::client(&mut server);
            let main = server.workspaces[0].id;
            server.workspaces[0].name = "main".into();
            let web = server.new_workspace("web".into(), None);
            server.show_workspace(client, web);
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = (cols, rows);
            super::super::persistence::tests::writer(&mut server, vec![Ok(()), Ok(())]);
            let mode = server.close_workspace(client, web);
            let Mode::Picker(p) = &mode else { panic!() };
            assert_eq!(p.current().unwrap().action.workspace(), Some(web));
            assert_eq!(server.clients[&client].workspace, main);
            server.clients.get_mut(&client).unwrap().mode = mode;
            let mut buf = Buffer::empty(Rect::new(0, 0, cols, rows));
            server.compose(client, 0, &mut buf);
            let shown: Vec<String> =
                buf.content.chunks(cols as usize).map(|row| row.iter().map(|c| c.symbol()).collect()).collect();
            let current: Vec<_> = shown.iter().enumerate().filter(|(_, row)| row.contains("Current")).collect();
            assert_eq!(current.len(), 1, "{shown:?}");
            let (index, row) = current[0];
            assert!(row.contains("Open") && shown[index - 1].contains("main"), "{shown:?}");
            assert!(!shown.iter().any(|row| row.contains("Closed") && row.contains("Current")));
            if cols == 80 {
                assert!(shown[0].contains("main"));
            }
        }
    }

    #[test]
    fn screen_closed_workspaces_share_the_unopened_file_marker() {
        let _batch = crate::pane::batch();
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        server.workspaces[0].name = "main".into();
        let pane = server
            .spawn_pane_in(
                server.workspaces[0].id,
                None,
                crate::pane::PaneSpec::default(),
                crate::protocol::Placement::Auto,
                None,
            )
            .unwrap();
        let agent = &mut server.panes.get_mut(&pane).unwrap().agent;
        agent.kind = Some("claude".into());
        agent.state = AgentState::Idle;
        let web = server.new_workspace("web".into(), None);
        super::super::persistence::tests::writer(&mut server, vec![Ok(()), Ok(())]);
        server.keep_and_close_workspace(web).unwrap();
        server.def_files.push(("/unopened/first.kdl".into(), "first".into()));
        let mut buf = Buffer::empty(Rect::new(0, 0, 124, 36));
        server.compose(client, 0, &mut buf);
        let rows: Vec<String> =
            buf.content.chunks(124).map(|row| row.iter().take(28).map(|c| c.symbol()).collect()).collect();
        for expected in ["● main", "○ web", "○ first"] {
            assert!(rows.iter().any(|row| row.contains(expected)), "{rows:?}");
        }
    }

    #[test]
    fn sidebar_status_omits_whole_parts_that_do_not_fit() {
        for (width, expected) in [(26, "Open · 11 tabs"), (32, "Open · 11 tabs · 45 panes"), (16, "Open")] {
            let area = Rect::new(0, 0, width, 24);
            let rows = vec![
                SideRow::Section("WORKSPACES"),
                SideRow::Entry {
                    item: Some(SideItem::Workspace(1)),
                    line: Line::from(vec![Span::raw("   "), Span::raw("Open · 11 tabs · 45 panes")]),
                    selected: true,
                },
            ];
            let mut buf = Buffer::empty(area);
            Server::draw_sidebar(&rows, area, &mut buf);
            let shown: String = (1..width - 1).map(|x| buf[(x, 1)].symbol()).collect();
            assert_eq!(shown.trim(), expected);
        }
    }

    #[test]
    fn removed_sources_are_hidden_even_before_the_cached_sidebar_is_rescanned() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let path = PathBuf::from("/missing/deleted-source.kdl");
        server.def_files.push((path.clone(), "deleted-source".into()));
        assert!(
            server
                .sidebar_rows(&server.clients[&client], 36, 0)
                .iter()
                .any(|r| matches!(r, SideRow::Entry { item: Some(SideItem::Definition(_)), .. }))
        );
        server.removed_sources.push(path);
        assert!(
            !server
                .sidebar_rows(&server.clients[&client], 36, 0)
                .iter()
                .any(|r| matches!(r, SideRow::Entry { item: Some(SideItem::Definition(_)), .. }))
        );
    }

    #[test]
    fn sidebar_workspace_drag_marks_both_destination_rows_without_changing_current() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let current = server.clients[&client].workspace;
        let other = server.new_workspace("other".into(), None);
        let row = |server: &Server, ws| {
            (0..36).find(|&r| server.sidebar_hit(client, r) == Some(SideItem::Workspace(ws))).unwrap()
        };
        let from = row(&server, other);
        let to = row(&server, current);
        server.client_input(client, format!("\x1b[<0;4;{}M", from + 1).as_bytes());
        server.client_input(client, format!("\x1b[<32;4;{}M", to + 1).as_bytes());
        let rows = server.sidebar_rows(&server.clients[&client], 36, 0);
        for r in &rows[to as usize..to as usize + 2] {
            let SideRow::Entry { line, selected, .. } = r else { panic!() };
            assert!(line.style.add_modifier.contains(Modifier::REVERSED));
            assert!(selected);
        }
        assert_eq!(server.clients[&client].workspace, current);
        assert!(server.workspace_order.is_empty());
    }

    #[test]
    fn sidebar_workspace_order_keeps_both_rows_and_the_current_marker_together() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let current = server.clients[&client].workspace;
        let other = server.new_workspace("other".into(), None);
        let source = PathBuf::from("/missing/source.kdl");
        server.def_files.push((source.clone(), "source".into()));
        server.workspace_order = vec![
            crate::persist::WorkspaceKey::Id(server.workspaces[1].stable_id.clone()),
            crate::persist::WorkspaceKey::Source(source.clone()),
            crate::persist::WorkspaceKey::Id(server.workspaces[0].stable_id.clone()),
        ];
        server.sort_workspaces();
        for height in [24, 36] {
            let rows = server.sidebar_rows(&server.clients[&client], height, 0);
            for (pair, item) in rows[1..7].as_chunks::<2>().0.iter().zip([
                SideItem::Workspace(other),
                SideItem::Definition(source.clone()),
                SideItem::Workspace(current),
            ]) {
                for row in pair {
                    let SideRow::Entry { item: Some(found), selected, .. } = row else { panic!() };
                    assert_eq!(*found, item);
                    assert_eq!(*selected, item == SideItem::Workspace(current));
                }
            }
            assert!(matches!(&rows[7], SideRow::Entry { item: Some(SideItem::AddWorkspace), .. }));
        }
    }

    #[test]
    fn the_cells_kept_are_all_but_those_changed() {
        let mut changed = Changed::new(Rect::new(0, 0, 10, 4));
        changed.row(0, 2, 4);
        changed.row(0, 3, 6);
        changed.border(Rect::new(5, 1, 3, 3));
        changed.rect(Rect::new(8, 3, 5, 1));
        let k = |y, x0, x1| Kept { y, x0, x1 };
        let want =
            vec![k(0, 0, 2), k(0, 6, 10), k(1, 0, 5), k(1, 8, 10), k(2, 0, 5), k(2, 6, 7), k(2, 8, 10), k(3, 0, 5)];
        assert_eq!(changed.kept(), want);
    }

    #[test]
    fn messages_too_long_are_cut() {
        let spans = vec![Span::raw("deploy "), Span::raw("failed on web01")];
        let cut = fit_spans(spans, 12);
        assert_eq!(spans_width(&cut), 12);
        assert_eq!(cut.iter().map(|s| s.content.as_ref()).collect::<String>(), "deploy fail…");
    }
    #[test]
    fn account_rows_follow_cached_sign_in_status_with_and_without_usage() {
        use crate::account_status::Status;
        use crate::accounts::{Account, Usage, Window};
        for used in [false, true] {
            let (tx, _rx) = super::super::queue::channel(16);
            let mut server = super::super::tests::server(tx);
            let (client, _peer) = super::super::tests::client(&mut server);
            server.config.sidebar.width = 28;
            let account =
                server.accounts.add(Account::new("claude", "gmail", "/nonexistent/gmail".into(), None, false)).unwrap();
            if used {
                server.usage.0.insert(
                    account.key(),
                    Usage {
                        five_hour: Some(Window { used_percent: 77.0, resets_at: Some(2_000) }),
                        observed_at: 1_000,
                        ..Default::default()
                    },
                );
            }
            for status in [
                None,
                Some(Status::NotSignedIn),
                Some(Status::NotInstalled),
                Some(Status::CouldNotCheck),
                Some(Status::SignedIn),
            ] {
                if let Some(status) = status {
                    server.account_discovery.set_status(&account, status);
                }
                let expected = match status {
                    Some(Status::NotSignedIn) => "Sign-in required",
                    Some(Status::NotInstalled) => "Not installed",
                    _ if used => "77% used",
                    _ => "No usage yet",
                };
                let picker = server.accounts_picker_at(Some(&account.id), 1_100);
                assert_eq!(picker.current().unwrap().detail, expected);
                let rows = server.sidebar_rows(&server.clients[&client], 36, 1_100);
                let details: Vec<_> = rows
                    .iter()
                    .filter_map(|row| match row {
                        SideRow::Entry { item: Some(SideItem::Account(id)), line, .. } if id == &account.id => {
                            Some(line)
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(details.len(), 2);
                assert_eq!(details[1].to_string().trim(), expected);
                assert_eq!(
                    details[1].spans.last().unwrap().style.fg,
                    Some(if matches!(status, Some(Status::NotSignedIn | Status::NotInstalled)) {
                        th().blocked
                    } else {
                        th().dim
                    })
                );
            }
            assert!(server.jobs.is_empty(), "building rows must not start checks");
        }
    }

    #[test]
    fn account_sidebar_rows_show_usage_counts_and_styles_at_full_and_narrow_widths() {
        use crate::accounts::{Usage, Window};
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let mut accounts = Vec::new();
        for (name, usage, count, required, threshold) in [
            ("patron.bzh", Some(87.0), 3, false, None),
            ("gmail", None, 0, false, None),
            ("Personal", Some(93.0), 1, false, None),
            ("unused", Some(42.0), 0, false, None),
            ("sign-in", None, 1, true, None),
            ("limit", Some(80.0), 0, false, Some(80)),
        ] {
            let mut a = server.accounts.get("claude", "default").unwrap();
            a.id = name.into();
            a.name = name.into();
            a.home = format!("/nonexistent/{name}");
            a.sign_in_required = required;
            a.threshold = threshold;
            if name == "Personal" {
                a.agent = "codex".into();
            }
            let a = server.accounts.add(a).unwrap();
            if let Some(used_percent) = usage {
                server.usage.0.insert(
                    a.key(),
                    Usage {
                        five_hour: Some(Window { used_percent, resets_at: Some(2_000) }),
                        observed_at: 1_000,
                        ..Default::default()
                    },
                );
            }
            for _ in 0..count {
                let id = server.alloc_id();
                let spec = crate::pane::PaneSpec {
                    account_id: Some(a.id.clone()),
                    account: Some(format!("{}:{}", a.agent, a.name)),
                    text: Some(crate::pane::ShownText { title: String::new(), text: String::new() }),
                    ..Default::default()
                };
                let pane = crate::pane::Pane::spawn(id, None, spec, 80, 24, &server.env, &server.tx).unwrap();
                server.panes.insert(id, pane);
            }
            accounts.push(a);
        }
        for width in [28, 16, 12] {
            server.config.sidebar.width = width;
            let rows = server.sidebar_rows(&server.clients[&client], 36, 1_100);
            let mut buf = Buffer::empty(Rect::new(0, 0, width, 36));
            Server::draw_sidebar(&rows, buf.area, &mut buf);
            for (a, summary, count, error) in [
                (&accounts[0], "87% used", 3, false),
                (&accounts[1], "No usage yet", 0, false),
                (&accounts[2], "93% used", 1, true),
                (&accounts[3], "42% used", 0, false),
                (&accounts[4], "Sign-in required", 1, true),
                (&accounts[5], "80% used", 0, true),
            ] {
                let y = rows
                    .iter()
                    .position(|row| {
                        matches!(row,
                    SideRow::Entry { item: Some(SideItem::Account(id)), .. } if id == &a.id)
                    })
                    .unwrap() as u16;
                let SideRow::Entry { line, .. } = &rows[y as usize] else { panic!() };
                assert_eq!(
                    line.to_string(),
                    format!(
                        " {} {} · {}",
                        if count > 0 { "●" } else { "○" },
                        if a.agent == "codex" { "Codex" } else { "Claude" },
                        a.name
                    )
                );
                assert_eq!(buf[(2, y)].fg, th().dim);
                assert!(buf[(4, y)].modifier.contains(Modifier::BOLD));
                let detail = if count == 0 {
                    summary.to_string()
                } else {
                    format!("{summary} · {count} pane{}", if count == 1 { "" } else { "s" })
                };
                let expected = if width == 28 && summary != "Sign-in required" {
                    detail.as_str()
                } else if summary.len() <= usize::from(width - 5) {
                    summary
                } else {
                    ""
                };
                let shown: String = (1..width - 1).map(|x| buf[(x, y + 1)].symbol()).collect();
                assert_eq!(shown.trim(), expected, "{} at {width} columns", a.name);
                assert_eq!(buf[(1, y + 1)].fg, th().dim, "indentation stays dim");
                if !expected.is_empty() {
                    for x in 4..4 + summary.len() as u16 {
                        assert_eq!(buf[(x, y + 1)].fg, if error { th().blocked } else { th().dim });
                    }
                }
                if width == 28 && count > 0 && summary != "Sign-in required" {
                    for x in 4 + summary.len() as u16..4 + crate::text::columns(&detail) as u16 {
                        assert_eq!(buf[(x, y + 1)].fg, th().dim, "separator and count stay dim");
                    }
                }
                for row in [y, y + 1] {
                    assert_eq!(server.sidebar_hit(client, row), Some(SideItem::Account(a.id.clone())));
                }
            }
        }
        server.config.sidebar.width = 28;
        server.accounts.limits.threshold = 85;
        let rows = server.sidebar_rows(&server.clients[&client], 36, 1_100);
        assert!(rows.iter().any(|row| matches!(row,
            SideRow::Entry { item: Some(SideItem::Account(id)), line, .. }
            if id == &accounts[0].id && line.spans.iter().any(|s| s.content == "87% used" && s.style.fg == Some(th().blocked)))));
        for state in [AgentState::Blocked, AgentState::Done, AgentState::Working, AgentState::Idle, AgentState::Unknown]
        {
            for pane in server.panes.values_mut() {
                pane.agent.kind = Some("claude".into());
                pane.agent.state = state;
            }
            let rows = server.sidebar_rows(&server.clients[&client], 36, 1_100);
            assert!(rows.iter().any(|row| matches!(row,
                SideRow::Entry { item: Some(SideItem::Account(id)), line, .. }
                if id == &accounts[0].id && line.spans[0].content == " ● "
                    && line.spans[0].style.fg == Some(th().state_color(state)))));
        }
        server.panes.clear();
        let rows = server.sidebar_rows(&server.clients[&client], 36, 1_100);
        assert!(rows.iter().any(|row| matches!(row,
            SideRow::Entry { item: Some(SideItem::Account(id)), line, .. }
            if id == &accounts[0].id && line.to_string() == " ○ Claude · patron.bzh")));
    }

    #[test]
    fn cached_account_sidebar_rows_follow_theme_changes() {
        if !crate::testing::in_own_process("server::render::tests::cached_account_sidebar_rows_follow_theme_changes") {
            return;
        }
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let mut account = server.accounts.get("claude", "default").unwrap();
        account.name = "work".into();
        account.sign_in_required = true;
        let account = server.accounts.add(account).unwrap();
        for theme in [
            crate::theme::Theme::DEFAULT,
            crate::theme::Theme { dim: Color::Blue, blocked: Color::Magenta, ..crate::theme::Theme::ASCII },
        ] {
            crate::theme::set(theme);
            let rows = server.sidebar_rows(&server.clients[&client], 36, 1_000);
            let y = rows
                .iter()
                .position(|row| {
                    matches!(row,
                SideRow::Entry { item: Some(SideItem::Account(id)), .. } if id == &account.id)
                })
                .unwrap() as u16;
            let mut buf = Buffer::empty(Rect::new(0, 0, 28, 36));
            Server::draw_sidebar(&rows, buf.area, &mut buf);
            assert_eq!(buf[(2, y)].symbol(), if theme.ascii { "o" } else { "○" });
            assert_eq!(buf[(2, y)].fg, theme.dim);
            assert_eq!(buf[(4, y + 1)].fg, theme.blocked);
        }
    }

    #[test]
    fn account_sidebar_preserves_checking_sign_in() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        server.account_discovery.set_check("claude", super::super::agents::ExistingCheck::Checking);
        for (width, expected) in [(28, "   Checking sign-in"), (14, "   Checking")] {
            server.config.sidebar.width = width;
            let rows = server.sidebar_rows(&server.clients[&client], 36, 1_000);
            let mut buf = Buffer::empty(Rect::new(0, 0, width, 36));
            Server::draw_sidebar(&rows, buf.area, &mut buf);
            assert!(crate::testing::buffer_text(&buf).contains(expected));
        }
    }

    #[test]
    fn account_sidebar_hover_marker_refreshes_and_selection_uses_the_same_row() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let (other_client, _other_peer) = super::super::tests::client(&mut server);
        let mut account = server.accounts.get("claude", "default").unwrap();
        account.name = "work".into();
        let account = server.accounts.add(account).unwrap();
        let y =
            (0..36).find(|&y| server.sidebar_hit(client, y) == Some(SideItem::Account(account.id.clone()))).unwrap();
        let mut buf = Buffer::empty(Rect::new(0, 0, 124, 36));
        server.compose(client, 1_000, &mut buf);
        assert_eq!(buf[(25, y)].symbol(), " ");
        for row in [y, y + 1] {
            server.client_input(client, format!("\x1b[<35;6;{}M", row + 1).as_bytes());
            server.compose(client, 1_000, &mut buf);
            assert_eq!(buf[(25, y)].symbol(), "⋮");
            assert_ne!(buf[(25, y + 1)].symbol(), "⋮");
            let mut fresh = Buffer::empty(buf.area);
            Server::draw_sidebar(
                &server.sidebar_rows(&server.clients[&client], 36, 1_000),
                Rect::new(0, 0, 28, 36),
                &mut fresh,
            );
            for y in 0..36 {
                for x in 0..28 {
                    assert_eq!(buf[(x, y)], fresh[(x, y)]);
                }
            }
        }
        let other = server.sidebar_rows(&server.clients[&other_client], 36, 1_000);
        assert!(
            other
                .iter()
                .all(|row| !matches!(row, SideRow::Entry { item: Some(SideItem::Account(_)), selected: true, .. }))
        );
        server.client_input(client, b"\x1b[<35;41;1M");
        server.compose(client, 1_000, &mut buf);
        assert_eq!(buf[(25, y)].symbol(), " ");
        server.clients.get_mut(&client).unwrap().mode = server.account_details(&account.id, None);
        let rows = server.sidebar_rows(&server.clients[&client], 36, 1_000);
        Server::draw_sidebar(&rows, Rect::new(0, 0, 28, 36), &mut buf);
        assert_eq!(buf[(25, y)].symbol(), "⋮");
    }

    #[test]
    fn sidebar_accounts_share_two_lines_and_keep_the_list_and_add_accessible() {
        use super::super::ui::{PickAction, PickItem};
        let items = vec![
            PickItem::new("Claude · work", "57% used · 2 panes", PickAction::AccountDetails("work-id".into())),
            PickItem::new("Codex · home", "Sign-in required", PickAction::AccountDetails("home-id".into())),
            PickItem::new("+ Add account", "", PickAction::AddAccount),
        ];
        let (tx, _rx) = super::super::queue::channel(16);
        let server = super::super::tests::server(tx);
        let rows = server.account_rows(items.clone(), 20, 0);
        assert!(matches!(rows[0], SideRow::Section("ACCOUNTS")));
        for (i, text) in [(1, " ○ Claude · work"), (2, "   57% used · 2 panes")] {
            let SideRow::Entry { item, line, .. } = &rows[i] else { panic!() };
            assert_eq!(item, &Some(SideItem::Account("work-id".into())));
            assert_eq!(line.to_string(), text);
        }
        assert!(matches!(&rows[5], SideRow::Entry { item: Some(SideItem::AddAccount), .. }));
        let rows = server.account_rows(items, 2, 0);
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0], SideRow::Section("ACCOUNTS")));
        assert!(matches!(rows[1], SideRow::Entry { item: Some(SideItem::AddAccount), .. }));
    }

    #[test]
    fn unresolved_sign_in_shows_usage_below_the_question_and_fits_the_sidebar() {
        use crate::accounts::{Account, Usage, Window};
        let (tx, _rx) = super::super::queue::channel(16);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        server.config.sidebar.width = 28;
        server.accounts.add(Account::new("claude", "work", "/nonexistent/work".into(), None, false)).unwrap();
        let existing = server.accounts.get("claude", "default").unwrap();
        server.accounts.adopt_checked(&existing, None).unwrap();
        let existing = server.accounts.reviews().pop().unwrap();
        for used in [false, true] {
            if used {
                server.usage.0.insert(
                    existing.key(),
                    Usage {
                        five_hour: Some(Window { used_percent: 77.0, resets_at: Some(2_000) }),
                        observed_at: 1_000,
                        ..Default::default()
                    },
                );
            }
            let expected = if used { "77% used" } else { "No usage yet" };
            let items = server.accounts_picker_at(None, 1_100).items;
            assert_eq!(items[1].label, "? Which account is ~/.claude");
            assert_eq!(items[1].detail, expected);
            assert!(items[1].dim);
            for room in 0..9 {
                let rows = server.account_rows(items.clone(), room, 1_100);
                assert!(rows.len() <= room);
                let question: Vec<_> = rows
                    .iter()
                    .filter_map(|row| match row {
                        SideRow::Entry { item: Some(SideItem::ReviewAccount(id)), line, .. } => {
                            assert_eq!(id, &existing.id);
                            assert_eq!(line.spans[0].style.fg, Some(th().dim));
                            Some(line.to_string())
                        }
                        _ => None,
                    })
                    .collect();
                if room >= 4 {
                    assert_eq!(question, [" ? Which account is ~/.claude".to_string(), format!("   {expected}")]);
                } else {
                    assert!(question.is_empty());
                }
                if room >= 2 {
                    assert!(matches!(rows[0], SideRow::Section("ACCOUNTS")));
                    let Some(SideRow::Entry { item: Some(SideItem::AddAccount), line, .. }) = rows.last() else {
                        panic!()
                    };
                    assert_eq!(line.to_string(), " + Add account");
                    assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
                }
            }
            let rows = server.sidebar_rows(&server.clients[&client], 36, 1_100);
            let y = rows
                .iter()
                .position(|row| {
                    matches!(row,
                SideRow::Entry { item: Some(SideItem::ReviewAccount(id)), .. } if id == &existing.id)
                })
                .unwrap();
            for (offset, expected) in [(0, "? Account for ~/.claude"), (1, expected)] {
                let SideRow::Entry { line, .. } = &rows[y + offset] else { panic!() };
                assert_eq!(line.to_string().trim(), expected);
                assert_eq!(
                    server.sidebar_hit(client, (y + offset) as u16),
                    Some(SideItem::ReviewAccount(existing.id.clone()))
                );
            }
        }
    }

    /// The accounts' rows made at one moment are those of any moment before the reset the usage
    /// says comes next (what a sidebar is kept until), and not those of that reset.
    #[test]
    fn the_accounts_rows_hold_until_a_usage_window_resets_and_ages_refresh_on_redraw() {
        use crate::accounts::{Account, Usage, Window};
        let (tx, _) = super::super::queue::channel(16);
        let mut server = super::super::tests::server(tx);
        let account = server
            .accounts
            .add(Account {
                id: crate::identity::new_uuid(),
                agent: "claude".into(),
                name: "work".into(),
                home: "/nonexistent/work".into(),
                threshold: None,
                note: None,
                sign_in_required: false,
                login_revision: String::new(),
                review: false,
                public_identity: None,
                linked_to: None,
                references: Vec::new(),
            })
            .unwrap();
        let five_hour = Some(Window { used_percent: 93.0, resets_at: Some(1_100) });
        let weekly = Some(Window { used_percent: 41.0, resets_at: Some(1_300) });
        server.usage.0.insert(account.key(), Usage { five_hour, weekly, observed_at: 1_000, ..Default::default() });
        let rows = |now| -> Vec<Line<'static>> {
            server
                .account_rows(server.accounts_picker_at(None, now).items, 20, now)
                .into_iter()
                .filter_map(|row| match row {
                    SideRow::Entry { line, .. } => Some(line),
                    _ => None,
                })
                .collect()
        };
        let shown = |now| rows(now)[1].to_string().trim().to_string();
        for (from, until, figure) in
            [(1_000, Some(1_100), "93% used"), (1_100, Some(1_300), "41% used"), (1_300, None, "Last checked 5m ago")]
        {
            assert_eq!((shown(from), server.usage.next_end(from)), (figure.to_string(), until));
            if let Some(until) = until {
                assert_eq!(rows(until - 1), rows(from));
                assert_ne!(rows(until), rows(from));
            }
        }
        // After the windows expire, the age of the observation also bounds cached rows.
        assert_eq!(server.account_summaries_until(1_300), Some(1_360));
        assert_eq!(rows(1_359), rows(1_300));
        assert_ne!(rows(1_360), rows(1_300));

        let (client, _peer) = super::super::tests::client(&mut server);
        let mut buf = Buffer::empty(Rect::new(0, 0, 124, 36));
        server.compose(client, 1_300, &mut buf);
        assert!(crate::testing::buffer_text(&buf).contains("Last checked 5m ago"));
        server.next_render = None;
        server.clients.get_mut(&client).unwrap().dirty = super::super::Dirty::No;

        // Expired windows leave no wake scheduled; a chrome tick only invalidates the cache.
        server.await_usage_reset();
        server.chrome_changed();
        assert!(server.usage_reset.is_none());
        assert!(server.next_render.is_none());
        assert!(server.clients[&client].dirty == super::super::Dirty::No);

        server.mark_dirty_client(client);
        assert!(server.next_render.is_some());
        server.compose(client, 1_360, &mut buf);
        assert!(crate::testing::buffer_text(&buf).contains("Last checked 6m ago"));
        assert!(!crate::testing::buffer_text(&buf).contains("Last checked 5m ago"));
    }

    #[test]
    fn account_frame_hit_uses_the_drawn_cells_and_drops_hidden_details() {
        let (tx, _rx) = super::super::queue::channel(16);
        let mut server = super::super::tests::server(tx);
        let mut account = server.accounts.get("claude", "default").unwrap();
        account.name = "équipe".into();
        server.accounts.add(account.clone()).unwrap();
        let spec = crate::pane::PaneSpec {
            account: Some("claude:équipe".into()),
            account_id: Some(account.id),
            text: Some(crate::pane::ShownText { title: String::new(), text: String::new() }),
            ..Default::default()
        };
        let mut pane =
            crate::pane::Pane::spawn(1, Some("review".into()), spec, 80, 24, &server.env, &server.tx).unwrap();
        pane.agent.kind = Some("claude".into());
        server.panes.insert(1, pane);
        for width in [80, 124] {
            for floating in [false, true] {
                let tile = Rect::new(3, 2, width, 10);
                let look = server.frame_look(1, tile, false, false, floating, true).unwrap();
                let mut buf = Buffer::empty(Rect::new(0, 0, 130, 36));
                Server::draw_frame(&look, &mut buf);
                let row: String = (tile.x..tile.right()).map(|x| buf[(x, tile.y)].symbol()).collect();
                let at = row[..row.find("claude·équipe").unwrap()].width() as u16 + tile.x;
                for col in tile.x..tile.right() {
                    assert_eq!(
                        server.frame_account_hit(1, tile, floating, true, col),
                        (at..at + "claude·équipe".width() as u16).contains(&col),
                        "{width}, {col}"
                    );
                }
            }
        }
        let narrow = Rect::new(0, 0, 16, 10);
        assert!((0..16).all(|x| !server.frame_account_hit(1, narrow, false, false, x)));
        server.config.pane_frames = false;
        assert!((0..80).all(|x| !server.frame_account_hit(1, Rect::new(0, 0, 80, 24), false, false, x)));
    }

    #[test]
    fn notification_actions_stay_visible_when_truncated_and_expire_with_the_message() {
        use super::super::attention::ToastAction;
        let (tx, _rx) = super::super::queue::channel(16);
        let mut server = super::super::tests::server(tx);
        server.config.notifications.mode = crate::config::NotifyMode::Off;
        let (id, _peer) = super::super::tests::client(&mut server);
        for cols in [80, 124] {
            server.clients.get_mut(&id).unwrap().cols = cols;
            for action in [
                ToastAction::UndoAccount,
                ToastAction::ChangeAccount,
                ToastAction::RestoreWorkspace("saved-workspace".into()),
                ToastAction::ReviewWorkspace(1),
                ToastAction::UndoLayout(1),
            ] {
                for kind in [NoticeKind::Notice, NoticeKind::Warning, NoticeKind::Error] {
                    server.notify_action(kind, &"A long account name ".repeat(8), 7, action.clone());
                    let bar = server.top_bar(&server.clients[&id]);
                    let label = bar.right.iter().find(|s| s.content == action.label()).unwrap();
                    assert_eq!(label.style.fg, Some(kind.colour(th())));
                    assert!(label.style.add_modifier.contains(Modifier::UNDERLINED));
                    assert_eq!(bar.right[0].style.fg, Some(kind.colour(th())));
                }
                let hits: Vec<_> = (0..cols)
                    .filter(|&x| server.tab_bar_hit(id, x) == Some(BarItem::Toast(7, action.clone())))
                    .collect();
                assert_eq!(hits.len(), action.label().len());
                let at = hits[0];
                server.clients.get_mut(&id).unwrap().toast.as_mut().unwrap().1 =
                    std::time::Instant::now() - std::time::Duration::from_secs(1);
                assert_ne!(server.tab_bar_hit(id, at), Some(BarItem::Toast(7, action)));
                server.toast(id, NoticeKind::Notice, "Another message");
                assert!(server.clients[&id].toast_action.is_none());
            }
        }
    }
}
