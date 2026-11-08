//! What keys do: every command kraal has, the presets that bind them, and the lookups the
//! server, its menu, its help and its command search share.
//!
//! Commands are reached with a leader key (`ctrl+b` by default) followed by a short sequence, and
//! a menu shows what can follow. The "kraal" preset gives each theme a letter: the lowercase
//! letter runs the theme's main action (`s` connects to a server), the uppercase one opens the
//! theme's menu (`S`: upload, download, edit the servers). Inside every menu the same letters do
//! the same things (`n` new, `x` close, `e` rename, arrows for previous and next). The "classic"
//! preset keeps tmux's single keys. A few frequent commands also have direct keys that need no
//! leader.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::input::{Key, KeyCode, KeySpec, Mods};

/// A command kraal has, whatever reaches it: keys, the menu, the command search. [`ACTIONS`]
/// gives each its name in the configuration and its description.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    NewPane,
    SplitRight,
    SplitDown,
    ClosePane,
    ToggleZoom,
    ToggleFloating,
    NewFloating,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    FocusOrTabLeft,
    FocusOrTabRight,
    ResizeMode,
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
    NextLayout,
    StackLeft,
    StackRight,
    StackUp,
    StackDown,
    Unstack,
    PanePicker,
    RenamePane,
    EditPane,
    NewTab,
    NextTab,
    PrevTab,
    MoveTabLeft,
    MoveTabRight,
    CloseTab,
    RenameTab,
    Tab(u8),
    WorkspacePicker,
    NewWorkspace,
    NextWorkspace,
    PrevWorkspace,
    RenameWorkspace,
    CloseWorkspace,
    SaveWorkspace,
    Restart,
    AgentPicker,
    Accounts,
    AddAccount,
    NextAttention,
    SwitchAccount,
    SshPicker,
    AddServer,
    Tunnels,
    EditServers,
    AttachFiles,
    AttachClipboardImage,
    PurgeAttachments,
    Upload,
    Download,
    Files,
    OpenPreview,
    ToolPicker,
    Runbooks,
    GitActions,
    ScrollMode,
    Search,
    CopyLastOutput,
    Buffers,
    Hints,
    ToggleSidebar,
    ToggleBottomBar,
    Messages,
    SelectForBroadcast,
    BroadcastSelected,
    BroadcastTab,
    BroadcastWorkspace,
    BroadcastOff,
    BroadcastToggle,
    CommandPalette,
    Help,
    Detach,
    Quit,
    SendPrefix,
}

use Action::*;

/// Every action (but `Tab(n)`): its name in the configuration, and what it does.
const ACTIONS: &[(Action, &str, &str)] = &[
    (NewPane, "new_pane", "New pane"),
    (SplitRight, "split_right", "Split right"),
    (SplitDown, "split_down", "Split down"),
    (ClosePane, "close_pane", "Close pane"),
    (ToggleZoom, "toggle_zoom", "Zoom pane"),
    (ToggleFloating, "toggle_floating", "Floating panes: show or hide"),
    (NewFloating, "new_floating", "New floating pane"),
    (FocusLeft, "focus_left", "Focus left"),
    (FocusRight, "focus_right", "Focus right"),
    (FocusUp, "focus_up", "Focus up"),
    (FocusDown, "focus_down", "Focus down"),
    (FocusOrTabLeft, "focus_or_tab_left", "Focus left (previous tab at the edge)"),
    (FocusOrTabRight, "focus_or_tab_right", "Focus right (next tab at the edge)"),
    (ResizeMode, "resize_mode", "Resize panes"),
    (ResizeLeft, "resize_left", "Resize left"),
    (ResizeRight, "resize_right", "Resize right"),
    (ResizeUp, "resize_up", "Resize up"),
    (ResizeDown, "resize_down", "Resize down"),
    (NextLayout, "next_layout", "Next layout (even columns, even rows, main pane left or on top, grid)"),
    (StackLeft, "stack_left", "Stack with the pane on the left"),
    (StackRight, "stack_right", "Stack with the pane on the right"),
    (StackUp, "stack_up", "Stack with the pane above"),
    (StackDown, "stack_down", "Stack with the pane below"),
    (Unstack, "unstack", "Unstack (take the pane out of its stack)"),
    (PanePicker, "pane_picker", "Find a pane"),
    (RenamePane, "rename_pane", "Rename pane"),
    (EditPane, "edit_pane", "Edit pane"),
    (NewTab, "new_tab", "New tab"),
    (NextTab, "next_tab", "Next tab"),
    (PrevTab, "prev_tab", "Previous tab"),
    (MoveTabLeft, "move_tab_left", "Move tab left"),
    (MoveTabRight, "move_tab_right", "Move tab right"),
    (CloseTab, "close_tab", "Close tab"),
    (RenameTab, "rename_tab", "Rename tab"),
    (WorkspacePicker, "workspace_picker", "Workspaces"),
    (NewWorkspace, "new_workspace", "Add workspace"),
    (NextWorkspace, "next_workspace", "Next workspace"),
    (PrevWorkspace, "prev_workspace", "Previous workspace"),
    (RenameWorkspace, "rename_workspace", "Rename workspace"),
    (CloseWorkspace, "close_workspace", "Close workspace"),
    (SaveWorkspace, "save_workspace", "Export layout"),
    (Restart, "restart", "Restart the session (on the kraal installed now; programs stop)"),
    (AgentPicker, "agent_picker", "Start an agent"),
    (Accounts, "accounts", "Accounts"),
    (AddAccount, "add_account", "Add account"),
    (NextAttention, "next_attention", "Next agent needing you"),
    (SwitchAccount, "switch_account", "Change the agent's account"),
    (AddServer, "add_server", "Add server"),
    (Tunnels, "tunnels", "Tunnels"),
    (SshPicker, "ssh_picker", "Connect to a server"),
    (EditServers, "edit_servers", "Edit saved servers"),
    (Upload, "upload", "Upload to this pane's server"),
    (Download, "download", "Download from this pane's server"),
    (AttachFiles, "attach_files", "Attach files"),
    (AttachClipboardImage, "attach_clipboard_image", "Attach clipboard image"),
    (PurgeAttachments, "purge_attachments", "Purge attachments"),
    (Files, "files", "File manager"),
    (OpenPreview, "open_preview", "Preview a file"),
    (ToolPicker, "tools", "Tools here (where this pane is)"),
    (Runbooks, "runbooks", "Runbooks"),
    (GitActions, "git_actions", "Git and worktrees"),
    (ScrollMode, "scroll_mode", "Copy mode: scroll and select"),
    (Search, "search", "Search the output"),
    (CopyLastOutput, "copy_last_output", "Copy the last command's output"),
    (Buffers, "buffers", "Paste buffers: what was copied, to paste again"),
    (Hints, "hints", "Open a link or path (hints on screen)"),
    (ToggleSidebar, "toggle_sidebar", "Sidebar"),
    (ToggleBottomBar, "toggle_bottom_bar", "Bottom bar: the focused pane's actions and their keys"),
    (Messages, "messages", "Message log"),
    (SelectForBroadcast, "select_for_broadcast", "Mark for broadcast (this pane)"),
    (BroadcastSelected, "broadcast_selected", "Broadcast to the marked panes"),
    (BroadcastTab, "broadcast_tab", "Broadcast to this tab"),
    (BroadcastWorkspace, "broadcast_workspace", "Broadcast to this workspace"),
    (BroadcastOff, "broadcast_off", "Stop broadcasting"),
    (BroadcastToggle, "broadcast_toggle", "Broadcast on/off (this tab)"),
    (CommandPalette, "command_palette", "Search commands"),
    (Help, "help", "Help"),
    (Detach, "detach", "Detach (programs keep running)"),
    (Quit, "quit", "Quit (programs stop; the layout is saved)"),
    (SendPrefix, "send_prefix", "Send the prefix key"),
];

impl Action {
    pub fn parse(name: &str) -> Option<Action> {
        if let Some(&(a, _, _)) = ACTIONS.iter().find(|(_, n, _)| *n == name) {
            return Some(a);
        }
        // Names from before 0.1.0, which no release documented: a configuration written then
        // keeps its bindings. They can go in any release.
        match name {
            "toggle_sync" => return Some(BroadcastToggle),
            "mark_pane" => return Some(SelectForBroadcast),
            _ => {}
        }
        let n: u8 = name.strip_prefix("tab_")?.parse().ok()?;
        (1..=9).contains(&n).then_some(Tab(n))
    }

    pub fn name(self) -> String {
        match self {
            Tab(n) => format!("tab_{n}"),
            a => ACTIONS.iter().find(|(x, _, _)| *x == a).map(|(_, n, _)| n.to_string()).unwrap_or_default(),
        }
    }

    pub fn describe(self) -> String {
        match self {
            Tab(n) => format!("Go to tab {n}"),
            a => ACTIONS.iter().find(|(x, _, _)| *x == a).map(|(_, _, d)| d.to_string()).unwrap_or_default(),
        }
    }

    /// The description without its explanation (after a colon, or in parentheses): what the
    /// menu shows.
    pub fn label(self) -> String {
        let d = self.describe();
        let short = d.split(" (").next().unwrap_or(&d);
        short.split(':').next().unwrap_or(short).trim().to_string()
    }

    /// Every action, in the order of the reference (tabs 1-9 as one).
    pub fn all() -> impl Iterator<Item = Action> {
        ACTIONS.iter().map(|(a, _, _)| *a).chain(std::iter::once(Tab(1)))
    }
}

/// What a key sequence runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Action(Action),
    /// A tool of `[tools]`, by name.
    Tool(String),
}

/// A preset's bindings: an action's name in the configuration, and its key sequences.
type Table = &'static [(&'static str, &'static [&'static str])];

/// A menu of a preset: its name in the configuration (`menu_<id>`), the key that opens it, its
/// title, and its main action (the one its theme's lowercase letter runs).
type MenuDef = (&'static str, &'static str, &'static str, Action);

/// A set of default bindings.
struct Preset {
    /// What `keys.preset` names it by.
    name: &'static str,
    /// Sequences after the leader (keys separated by spaces), in menu order. `{id}` stands for
    /// the key of menu `id`, `{prefix}` for the prefix key.
    leader: Table,
    /// Single keys that need no leader.
    direct: Table,
    menus: &'static [MenuDef],
    /// Menus further in: the sequence that opens one, and its title.
    submenus: &'static [(&'static str, &'static str)],
    /// What a tool's `key` follows.
    tool_menu: &'static str,
}

const KRAAL: Preset = Preset {
    name: "kraal",
    leader: &[
        // The main action of each theme; its uppercase letter opens the theme's menu.
        ("ssh_picker", &["s", "{servers} s", "{servers} enter"]),
        ("broadcast_toggle", &["b", "{broadcast} b", "{broadcast} enter"]),
        ("workspace_picker", &["w", "{workspaces} w", "{workspaces} enter"]),
        ("agent_picker", &["a", "{agents} a", "{agents} enter"]),
        ("new_tab", &["t", "{tabs} t", "{tabs} n", "{tabs} enter"]),
        ("pane_picker", &["p", "{panes} p", "{panes} enter"]),
        ("scroll_mode", &["v", "{view} v", "{view} enter"]),
        ("files", &["f", "{files} f", "{files} enter"]),
        ("tools", &["r", "{run} r", "{run} enter"]),
        // Verbs and fixed keys.
        ("new_pane", &["n", "{panes} n"]),
        ("close_pane", &["x", "{panes} x"]),
        ("toggle_zoom", &["z"]),
        ("select_for_broadcast", &["m", "{broadcast} m"]),
        ("hints", &["o", "{view} o"]),
        ("search", &["g", "/", "{view} g"]),
        ("focus_left", &["left"]),
        ("focus_down", &["down"]),
        ("focus_up", &["up"]),
        ("focus_right", &["right"]),
        ("tab_1", &["1"]),
        ("tab_2", &["2"]),
        ("tab_3", &["3"]),
        ("tab_4", &["4"]),
        ("tab_5", &["5"]),
        ("tab_6", &["6"]),
        ("tab_7", &["7"]),
        ("tab_8", &["8"]),
        ("tab_9", &["9"]),
        ("command_palette", &["space"]),
        ("help", &["h", "?"]),
        ("detach", &["d"]),
        ("send_prefix", &["{prefix}"]),
        // The rest of each menu.
        ("split_right", &["{panes} right"]),
        ("split_down", &["{panes} down"]),
        ("edit_pane", &["{panes} e"]),
        ("resize_mode", &["{panes} r"]),
        ("new_floating", &["{panes} f"]),
        ("next_layout", &["{panes} l"]),
        ("stack_left", &["{panes} s left"]),
        ("stack_down", &["{panes} s down"]),
        ("stack_up", &["{panes} s up"]),
        ("stack_right", &["{panes} s right"]),
        ("unstack", &["{panes} u"]),
        ("prev_tab", &["{tabs} left"]),
        ("next_tab", &["{tabs} right"]),
        ("move_tab_left", &["{tabs} shift+left"]),
        ("move_tab_right", &["{tabs} shift+right"]),
        ("rename_tab", &["{tabs} e"]),
        ("close_tab", &["{tabs} x"]),
        ("new_workspace", &["{workspaces} n"]),
        ("add_server", &["{servers} n"]),
        ("tunnels", &["{servers} o"]),
        ("prev_workspace", &["{workspaces} left"]),
        ("next_workspace", &["{workspaces} right"]),
        ("rename_workspace", &["{workspaces} e"]),
        ("save_workspace", &["{workspaces} s"]),
        ("close_workspace", &["{workspaces} x"]),
        ("restart", &["{workspaces} r"]),
        // Next to `d` detach, and in the Workspaces menu after the restart (menus list in this
        // order).
        ("quit", &["q", "{workspaces} q"]),
        ("next_attention", &["{agents} g"]),
        ("switch_account", &["{agents} c"]),
        ("accounts", &["{agents} l"]),
        ("add_account", &["{agents} n"]),
        ("upload", &["{servers} u", "{files} u"]),
        ("download", &["{servers} d", "{files} d"]),
        ("edit_servers", &["{servers} e"]),
        ("open_preview", &["{files} p"]),
        ("runbooks", &["{run} b"]),
        ("git_actions", &["{run} g"]),
        ("toggle_floating", &["{view} f"]),
        ("toggle_sidebar", &["{view} l"]),
        ("toggle_bottom_bar", &["{view} k"]),
        ("messages", &["{view} j"]),
        ("broadcast_workspace", &["{broadcast} w"]),
        ("broadcast_selected", &["{broadcast} s"]),
        ("broadcast_off", &["{broadcast} o"]),
        ("copy_last_output", &["{view} c"]),
        // tmux's key for its list of buffers, too.
        ("buffers", &["{view} p", "="]),
    ],
    direct: &[
        ("focus_or_tab_left", &["alt+left"]),
        ("focus_or_tab_right", &["alt+right"]),
        ("focus_up", &["alt+up"]),
        ("focus_down", &["alt+down"]),
        ("new_pane", &["alt+n"]),
        ("next_attention", &["alt+a"]),
    ],
    menus: &[
        ("servers", "S", "Servers", SshPicker),
        ("broadcast", "B", "Broadcast", BroadcastToggle),
        ("workspaces", "W", "Workspaces", WorkspacePicker),
        ("agents", "A", "Agents", AgentPicker),
        ("tabs", "T", "Tabs", NewTab),
        ("panes", "P", "Panes", PanePicker),
        ("view", "V", "View", ScrollMode),
        ("files", "F", "Files", Files),
        ("run", "R", "Run", ToolPicker),
    ],
    submenus: &[("{panes} s", "Stack with")],
    tool_menu: "{run}",
};

/// tmux's keys, as kraal had them first.
const CLASSIC: Preset = Preset {
    name: "classic",
    leader: &[
        ("split_right", &["%", "|"]),
        ("split_down", &["\"", "-"]),
        ("close_pane", &["x"]),
        ("toggle_zoom", &["z"]),
        ("toggle_floating", &["t"]),
        ("new_floating", &["F"]),
        ("files", &["f"]),
        ("focus_left", &["h", "left"]),
        ("focus_right", &["l", "right"]),
        ("focus_up", &["k", "up"]),
        ("focus_down", &["j", "down"]),
        ("resize_left", &["H", "ctrl+left"]),
        ("resize_right", &["L", "ctrl+right"]),
        ("resize_up", &["K", "ctrl+up"]),
        ("resize_down", &["J", "ctrl+down"]),
        ("next_layout", &["E"]),
        ("stack_left", &["M h", "M left"]),
        ("stack_down", &["M j", "M down"]),
        ("stack_up", &["M k", "M up"]),
        ("stack_right", &["M l", "M right"]),
        ("unstack", &["u"]),
        ("new_tab", &["c"]),
        ("next_tab", &["n", "tab"]),
        ("prev_tab", &["p", "shift+tab"]),
        ("move_tab_left", &["<"]),
        ("move_tab_right", &[">"]),
        ("close_tab", &["&"]),
        ("rename_tab", &[","]),
        ("rename_pane", &["."]),
        ("rename_workspace", &["$"]),
        ("tab_1", &["1"]),
        ("tab_2", &["2"]),
        ("tab_3", &["3"]),
        ("tab_4", &["4"]),
        ("tab_5", &["5"]),
        ("tab_6", &["6"]),
        ("tab_7", &["7"]),
        ("tab_8", &["8"]),
        ("tab_9", &["9"]),
        ("next_workspace", &[")"]),
        ("prev_workspace", &["("]),
        ("workspace_picker", &["o", "w"]),
        ("ssh_picker", &["s"]),
        ("upload", &["U"]),
        ("download", &["D"]),
        ("pane_picker", &["g"]),
        ("agent_picker", &["G"]),
        ("switch_account", &["A"]),
        ("git_actions", &["R"]),
        ("tools", &["T"]),
        ("hints", &["space"]),
        ("runbooks", &["r"]),
        ("next_attention", &["a"]),
        ("toggle_sidebar", &["b"]),
        ("broadcast_toggle", &["y"]),
        ("select_for_broadcast", &["m"]),
        ("scroll_mode", &["[", "pageup", "v"]),
        ("search", &["/"]),
        ("open_preview", &["e"]),
        ("save_workspace", &["S"]),
        ("command_palette", &[":"]),
        ("detach", &["d", "q"]),
        ("quit", &["Q"]),
        ("help", &["?"]),
        ("send_prefix", &["{prefix}"]),
        ("copy_last_output", &["Y"]),
        ("buffers", &["="]),
    ],
    direct: &[
        ("focus_or_tab_left", &["alt+h", "alt+left"]),
        ("focus_or_tab_right", &["alt+l", "alt+right"]),
        ("focus_up", &["alt+k", "alt+up"]),
        ("focus_down", &["alt+j", "alt+down"]),
        ("new_pane", &["alt+n"]),
        ("toggle_floating", &["alt+f"]),
        ("next_attention", &["alt+a"]),
        ("workspace_picker", &["alt+o"]),
        ("agent_picker", &["alt+g"]),
    ],
    menus: &[],
    submenus: &[("M", "Stack with")],
    tool_menu: "",
};

const PRESETS: [&Preset; 2] = [&KRAAL, &CLASSIC];

/// `[keys]` of the configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct KeysConfig {
    /// "kraal" or "classic".
    pub preset: String,
    /// The keys that start a leader sequence; hints show the first one.
    pub prefix: Vec<String>,
    /// Per action, replacing the preset's: sequences after the prefix. `menu_<id>` gives the
    /// keys that open a menu (`menu_servers = ["M"]`).
    #[serde(default)]
    pub leader: BTreeMap<String, Vec<String>>,
    /// Per action, replacing the preset's: single keys that need no leader.
    #[serde(default)]
    pub direct: BTreeMap<String, Vec<String>>,
}

/// What the keys typed after the leader lead to.
#[derive(Debug, PartialEq)]
pub enum Lookup {
    /// A whole sequence, which runs this.
    Run(Command),
    /// A menu: more keys to come.
    More,
    /// No sequence goes on this way.
    Nothing,
}

/// One line of the leader menu.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuEntry {
    /// The key to type next.
    pub key: KeySpec,
    /// What it does, without the name of the menu it is in.
    pub label: String,
    /// It opens a further menu.
    pub group: bool,
    /// What it runs; `None` for a further menu.
    pub command: Option<Command>,
    /// The main action of a theme, whose menu its uppercase letter opens.
    pub main: bool,
}

/// A menu of the keymap, opened by a key after the prefix.
#[derive(Debug, Clone)]
pub struct Menu {
    /// Its name in the configuration, after `menu_`.
    pub id: &'static str,
    /// The keys that open it after the prefix; none when the user took them all away.
    pub keys: Vec<KeySpec>,
    pub title: &'static str,
    /// The action its theme's lowercase letter runs; the leader menu lists these first.
    pub main: Action,
}

/// `text` without a leading `word` (any case) and the space after it, capitalised.
fn strip_word(text: &str, word: &str) -> Option<String> {
    let head = text.get(..word.len())?;
    let rest = text[word.len()..].strip_prefix(' ')?;
    let mut chars = rest.chars();
    let first = chars.next()?;
    head.eq_ignore_ascii_case(word).then(|| first.to_uppercase().chain(chars).collect())
}

/// Resolved key bindings.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    /// The name of the preset it was built from.
    pub preset: &'static str,
    /// The keys that start a leader sequence; hints show the first one.
    pub prefix: Vec<KeySpec>,
    /// Single keys that need no leader, none of them a prefix key or bound twice.
    pub direct: Vec<(KeySpec, Action)>,
    /// Sequences after the prefix, in menu order.
    pub leader: Vec<(Vec<KeySpec>, Command)>,
    pub menus: Vec<Menu>,
    /// What a tool's key follows: each key of the tool menu, or nothing (tools then follow the
    /// prefix directly).
    tool_menu: Vec<Vec<KeySpec>>,
    /// Sequences that open a menu further in, with its title.
    submenus: Vec<(Vec<KeySpec>, &'static str)>,
}

/// The sequences `text` stands for. `{id}` is each key of menu `id` (none when the menu has no
/// key), `{prefix}` each prefix key.
fn expand(text: &str, menus: &[Menu], prefix: &[KeySpec]) -> Result<Vec<Vec<KeySpec>>, String> {
    let mut out: Vec<Vec<KeySpec>> = vec![Vec::new()];
    let mut empty = true;
    for token in text.split_whitespace() {
        empty = false;
        let named = token.strip_prefix('{').and_then(|t| t.strip_suffix('}'));
        let choices: Vec<KeySpec> = match named {
            Some("prefix") => prefix.to_vec(),
            Some(id) => menus
                .iter()
                .find(|m| m.id == id)
                .map(|m| m.keys.clone())
                .ok_or_else(|| format!("{token:?}: no such menu"))?,
            None => vec![KeySpec::parse(token).map_err(|e| format!("{token:?}: {e}"))?],
        };
        out = out
            .into_iter()
            .flat_map(|seq| {
                choices.iter().map(move |k| {
                    let mut seq = seq.clone();
                    seq.push(k.clone());
                    seq
                })
            })
            .collect();
    }
    if empty {
        return Err("empty".into());
    }
    Ok(out)
}

/// Bindings of `table`, with the user's entries replacing the preset's for their actions.
/// Entries naming a menu (`menu_<id>`) are not actions: [`Keymap::build`] reads them.
fn bindings(
    preset: Table,
    user: &BTreeMap<String, Vec<String>>,
    ctx: &str,
    (menus, prefix): (&[Menu], &[KeySpec]),
    warnings: &mut Vec<String>,
) -> Vec<(Vec<KeySpec>, Action)> {
    let is_menu = |name: &str| name.starts_with("menu_");
    let mut out = Vec::new();
    let mut add = |name: &str, seqs: &[String], warnings: &mut Vec<String>| {
        let Some(action) = Action::parse(name) else {
            warnings.push(format!("{ctx}: unknown action {name:?}"));
            return;
        };
        for s in seqs {
            match expand(s, menus, prefix) {
                Ok(all) => out.extend(all.into_iter().map(|keys| (keys, action))),
                Err(e) => warnings.push(format!("{ctx}.{name}: invalid key {e}")),
            }
        }
    };
    for (name, seqs) in preset.iter().filter(|(n, _)| !user.contains_key(*n)) {
        let seqs: Vec<String> = seqs.iter().map(|s| s.to_string()).collect();
        add(name, &seqs, warnings);
    }
    for (name, seqs) in user.iter().filter(|(n, _)| !is_menu(n)) {
        add(name, seqs, warnings);
    }
    out
}

/// Where a preset's `table` lists `action`: its row, and past the last one for an action the
/// preset gives no key.
fn place(table: Table, action: Action) -> usize {
    table.iter().position(|(name, _)| Action::parse(name) == Some(action)).unwrap_or(table.len())
}

/// A section of the reference: its title, and rows of keys with what they do.
type Section = (String, Vec<(String, String)>);

/// Keys and what they do in lists, text fields, questions and modes: fixed, the same in every
/// preset.
const MODE_KEYS: &[(&str, &[(&str, &str)])] = &[
    (
        "In lists",
        &[
            ("type", "filter"),
            ("↑ ↓", "move"),
            ("⏎", "choose"),
            ("tab", "more actions on the selected item"),
            ("esc", "close"),
        ],
    ),
    (
        "In text fields",
        &[
            ("← → home end", "move (alt+← → by word)"),
            ("ctrl+w ctrl+u ctrl+k", "delete a word, to the start, to the end"),
            ("⏎ / esc", "accept / cancel"),
        ],
    ),
    (
        "In questions",
        &[
            ("y / n", "yes / no"),
            ("← → tab", "move between the answers"),
            ("⏎", "the selected answer"),
            ("↑ ↓", "scroll a long question"),
            ("esc", "no"),
        ],
    ),
    (
        "Copy mode",
        &[
            ("arrows hjkl", "move"),
            ("w b e", "next word, previous word, end of word"),
            ("0 $ home end", "start, end of the line"),
            ("g G", "top, bottom"),
            ("pageup pagedown", "a page up, down (ctrl+u ctrl+d: half a page)"),
            ("v V ctrl+v", "select characters, lines, a block"),
            ("y or ⏎", "copy and leave"),
            ("/ n N", "search, next, previous"),
            ("p P", "the prompt above, below (shells that mark commands)"),
            ("o O", "select a command's output, the whole command"),
            ("esc q", "leave"),
        ],
    ),
    (
        "Searching the output",
        &[
            ("type", "the text to find"),
            ("↑ ↓", "older, newer match"),
            ("⏎", "keep the match and go on in copy mode"),
            ("esc", "cancel"),
        ],
    ),
    ("Resizing", &[("arrows hjkl", "move the border"), ("⏎ or esc", "done")]),
    (
        "Hints",
        &[("a label", "copy what it marks, or act on it"), ("tab", "switch between copy and act"), ("esc", "cancel")],
    ),
    (
        "Mouse",
        &[
            ("click", "focus a pane, tab, workspace or agent; open a folded pane; run an action of the bottom bar"),
            ("drag a border", "resize panes"),
            ("drag in a pane", "select and copy (double: word, triple: line)"),
            ("ctrl+click", "open a link; on a frame's top: mark for broadcast"),
            ("shift + mouse", "your terminal's own selection"),
        ],
    ),
];

/// Whether two bindings would catch the same key press (`ctrl+m` and `enter` do).
fn same_key(a: &KeySpec, b: &KeySpec) -> bool {
    a.matches(&Key { code: b.code, mods: b.mods, raw: Vec::new() })
        || b.matches(&Key { code: a.code, mods: a.mods, raw: Vec::new() })
}

fn starts_with(seq: &[KeySpec], typed: &[Key]) -> bool {
    seq.len() >= typed.len() && seq.iter().zip(typed).all(|(s, k)| s.matches(k))
}

impl Keymap {
    /// Build from the configuration; also returns warnings about entries that were left out.
    pub fn build(cfg: &KeysConfig) -> (Keymap, Vec<String>) {
        let mut warnings = Vec::new();
        let preset = PRESETS.iter().find(|p| p.name == cfg.preset).copied().unwrap_or_else(|| {
            warnings.push(format!("keys.preset: no preset {:?} (kraal or classic)", cfg.preset));
            &KRAAL
        });
        let prefix: Vec<KeySpec> = cfg
            .prefix
            .iter()
            .filter_map(|s| {
                KeySpec::parse(s).map_err(|e| warnings.push(format!("keys.prefix: invalid key {s:?}: {e}"))).ok()
            })
            .collect();
        // The menus first: the leader's sequences name them.
        let mut menus = Vec::new();
        for &(id, key, title, main) in preset.menus {
            let name = format!("menu_{id}");
            let keys = match cfg.leader.get(&name) {
                Some(list) => list
                    .iter()
                    .filter_map(|k| {
                        KeySpec::parse(k).map_err(|e| warnings.push(format!("keys.leader.{name}: {e}"))).ok()
                    })
                    .collect(),
                None => KeySpec::parse(key).into_iter().collect(),
            };
            menus.push(Menu { id, keys, title, main });
        }
        for name in cfg.leader.keys().filter(|n| n.starts_with("menu_")) {
            if !menus.iter().any(|m| name[5..] == *m.id) {
                warnings.push(format!("keys.leader: no menu {name:?} in the {} preset", preset.name));
            }
        }
        let direct: Vec<(KeySpec, Action)> =
            bindings(preset.direct, &cfg.direct, "keys.direct", (&menus, &prefix), &mut warnings)
                .into_iter()
                .filter_map(|(mut keys, a)| {
                    if keys.len() > 1 {
                        warnings.push(format!("keys.direct.{}: one key, not a sequence", a.name()));
                        return None;
                    }
                    Some((keys.remove(0), a))
                })
                .collect();
        // A direct key that is also the prefix, or bound twice, would shadow the other.
        let mut kept: Vec<(KeySpec, Action)> = Vec::new();
        for (k, a) in direct {
            if prefix.iter().any(|p| same_key(p, &k)) {
                warnings.push(format!("keys.direct.{}: {k} is the prefix key", a.name()));
            } else if let Some((_, other)) = kept.iter().find(|(x, _)| same_key(x, &k)) {
                warnings.push(format!("keys.direct.{}: {k} is already {:?}", a.name(), other.describe()));
            } else {
                kept.push((k, a));
            }
        }
        // The keys the user gave an action are listed where the preset lists that action (the
        // menu, the help, `kraal config keys`). Only the listing: a key two actions ask for
        // went to the preset's, as before.
        kept.sort_by_key(|(_, a)| place(preset.direct, *a));
        let leader = bindings(preset.leader, &cfg.leader, "keys.leader", (&menus, &prefix), &mut warnings);
        let tool_menu = match preset.tool_menu {
            "" => vec![Vec::new()],
            t => expand(t, &menus, &prefix).unwrap_or_default(),
        };
        let submenus = preset
            .submenus
            .iter()
            .flat_map(|(seq, title)| expand(seq, &menus, &prefix).unwrap_or_default().into_iter().map(|k| (k, *title)))
            .collect();
        let mut km =
            Keymap { preset: preset.name, prefix, direct: kept, menus, tool_menu, submenus, ..Keymap::default() };
        for (keys, a) in leader {
            km.add(keys, Command::Action(a), &mut warnings);
        }
        km.leader.sort_by_key(|(_, command)| match command {
            Command::Action(a) => place(preset.leader, *a),
            Command::Tool(_) => preset.leader.len(),
        });
        (km, warnings)
    }

    /// Bind a sequence, unless it collides with one already bound.
    fn add(&mut self, keys: Vec<KeySpec>, command: Command, warnings: &mut Vec<String>) -> bool {
        let shown = |k: &[KeySpec]| k.iter().map(KeySpec::to_string).collect::<Vec<_>>().join(" ");
        let what = |c: &Command| match c {
            Command::Action(a) => a.describe(),
            Command::Tool(t) => format!("tool {t}"),
        };
        for (other, c) in &self.leader {
            let n = other.len().min(keys.len());
            if other[..n].iter().zip(&keys[..n]).all(|(a, b)| same_key(a, b)) {
                warnings.push(if other.len() == keys.len() {
                    format!("key {:?} is already {:?}", shown(&keys), what(c))
                } else {
                    format!("key {:?} clashes with {:?} ({})", shown(&keys), shown(other), what(c))
                });
                return false;
            }
        }
        self.leader.push((keys, command));
        true
    }

    /// Bind the `key` of each tool (after the preset's tool menu); warnings for invalid or taken
    /// keys.
    pub fn bind_tools(&mut self, tools: &BTreeMap<String, crate::tools::ToolDef>) -> Vec<String> {
        let mut warnings = Vec::new();
        for (name, t) in tools.iter().filter(|(_, t)| t.enabled) {
            let Some(key) = &t.key else { continue };
            match KeySpec::parse(key) {
                Ok(k) => {
                    for menu in self.tool_menu.clone() {
                        let mut keys = menu;
                        keys.push(k.clone());
                        let mut why = Vec::new();
                        if !self.add(keys, Command::Tool(name.clone()), &mut why) {
                            warnings.extend(why.into_iter().map(|w| format!("tools.{name}: {w}")));
                        }
                    }
                }
                Err(e) => warnings.push(format!("tools.{name}: invalid key {key:?}: {e}")),
            }
        }
        warnings
    }

    /// Whether `key` is the prefix: by the character it types, else by its place on the
    /// keyboard (see [`Key::by_place`]) when no direct key takes it as typed.
    pub fn is_prefix(&self, key: &Key) -> bool {
        let prefix = |k: &Key| self.prefix.iter().any(|p| p.matches(k));
        prefix(key) || (self.direct_typed(key).is_none() && key.by_place().is_some_and(|k| prefix(&k)))
    }

    /// The action of a direct key: by the character typed, else by the key's place when the
    /// prefix does not take it as typed.
    pub fn direct(&self, key: &Key) -> Option<Action> {
        self.direct_typed(key).or_else(|| {
            let typed_prefix = self.prefix.iter().any(|p| p.matches(key));
            key.by_place().filter(|_| !typed_prefix).and_then(|k| self.direct_typed(&k))
        })
    }

    fn direct_typed(&self, key: &Key) -> Option<Action> {
        self.direct.iter().find(|(k, _)| k.matches(key)).map(|(_, a)| *a)
    }

    /// `key` as the keymap reads it after `typed`: as typed when a sequence goes on with it,
    /// else as the key at its place on the keyboard (the digit row, a non-Latin layout) when one
    /// does, so a binding written for a character never loses its key to another's place.
    pub fn leader_key(&self, typed: &[Key], key: Key) -> Key {
        let reads = |k: &Key| {
            let mut keys = typed.to_vec();
            keys.push(k.clone());
            self.lookup(&keys) != Lookup::Nothing
        };
        if reads(&key) {
            return key;
        }
        let place = key.by_place().or_else(|| digit_row(&key));
        match place {
            Some(place) if reads(&place) => place,
            _ => key,
        }
    }

    /// What the keys typed after the leader so far lead to.
    pub fn lookup(&self, typed: &[Key]) -> Lookup {
        let mut more = false;
        for (seq, cmd) in &self.leader {
            if !starts_with(seq, typed) {
                continue;
            }
            if seq.len() == typed.len() {
                return Lookup::Run(cmd.clone());
            }
            more = true;
        }
        if more { Lookup::More } else { Lookup::Nothing }
    }

    /// The keys that can follow `typed`, for the menu: commands and further menus.
    pub fn menu(&self, typed: &[Key]) -> Vec<MenuEntry> {
        let typed_specs: Vec<KeySpec> = typed.iter().map(|k| KeySpec::new(k.code, k.mods)).collect();
        let within = self.group_title(&typed_specs);
        let mut out: Vec<MenuEntry> = Vec::new();
        for (seq, cmd) in &self.leader {
            if seq.len() <= typed.len() || !starts_with(seq, typed) {
                continue;
            }
            let key = seq[typed.len()].clone();
            let group = seq.len() > typed.len() + 1;
            // One line per key, and per command (its first key).
            if out.iter().any(|e| e.key == key || (!group && e.command.as_ref() == Some(cmd))) {
                continue;
            }
            let label = if group {
                self.group_title(&seq[..=typed.len()]).unwrap_or("…").to_string()
            } else {
                let label = match cmd {
                    Command::Action(a) => a.label(),
                    Command::Tool(t) => t.clone(),
                };
                // The menu's name is not repeated: in Broadcast, "Broadcast to this workspace" is
                // "To this workspace".
                match within.and_then(|title| strip_word(&label, title)) {
                    Some(rest) => rest,
                    None => label,
                }
            };
            let main = typed.is_empty() && !group && matches!(cmd, Command::Action(a) if self.is_main(*a));
            out.push(MenuEntry { key, label, group, command: (!group).then(|| cmd.clone()), main });
        }
        // The main actions of the themes, then the other commands, then the menus.
        out.sort_by_key(|e| (e.group, !e.main));
        out
    }

    /// Whether `action` is the main action of one of the menus.
    fn is_main(&self, action: Action) -> bool {
        self.menus.iter().any(|m| m.main == action && !m.keys.is_empty())
    }

    /// The title of the menu `keys` open, if it has one.
    pub fn group_title(&self, keys: &[KeySpec]) -> Option<&'static str> {
        let menu = match keys {
            [k] => self.menus.iter().find(|m| m.keys.contains(k)).map(|m| m.title),
            _ => None,
        };
        menu.or_else(|| self.submenus.iter().find(|(seq, _)| seq == keys).map(|(_, title)| *title))
    }

    /// `ctrl+b B`: how to open menu `id`.
    pub fn menu_hint(&self, id: &str) -> Option<String> {
        let key = self.menus.iter().find(|m| m.id == id)?.keys.first()?;
        Some(format!("{} {key}", self.prefix_label()))
    }

    /// Whether each of `actions` has one key right after the prefix, the one given with it:
    /// keys the reference can write as a range.
    fn bound_as(&self, actions: &[(Action, KeyCode)]) -> bool {
        actions.iter().all(|(action, code)| {
            let command = Command::Action(*action);
            let mut keys = self.leader.iter().filter(|(keys, c)| keys.len() == 1 && *c == command);
            let its_key = keys.next().is_some_and(|(keys, _)| keys[0] == KeySpec::new(*code, Mods::NONE));
            its_key && keys.next().is_none()
        })
    }

    /// The whole reference, for the help screen and `kraal config keys`: sections of (keys,
    /// what they do). The keys after the prefix come first, then each menu, the keys without the
    /// prefix, and the fixed keys of lists, fields, questions and modes. In a section, the help
    /// shows a command once, with its first key; with `every`, each key bound has its row.
    pub fn reference(&self, ascii: bool, every: bool) -> Vec<Section> {
        let p = self.prefix_label();
        let seq = |keys: &[KeySpec]| self.after_prefix(keys, |k| pretty(k, ascii));
        let root = format!("After {p}");
        // The arrows and the digits are written as a range: among every key, only where they
        // are bound as that range and nothing else.
        let sides = [FocusLeft, FocusDown, FocusUp, FocusRight].into_iter();
        let sides: Vec<_> = sides.zip([KeyCode::Left, KeyCode::Down, KeyCode::Up, KeyCode::Right]).collect();
        let tabs: Vec<_> = (1..=9).map(|n| (Tab(n), KeyCode::Char(char::from(b'0' + n)))).collect();
        let (arrows, digits) = (!every || self.bound_as(&sides), !every || self.bound_as(&tabs));
        // Each section, with the commands it lists.
        let mut sections: Vec<(Section, Vec<Command>)> = vec![((root.clone(), Vec::new()), Vec::new())];
        for (keys, cmd) in &self.leader {
            let title = match keys.len() {
                1 => root.clone(),
                _ => match self.group_title(&keys[..1]) {
                    Some(t) => format!("{t} ({p} {})", pretty(&keys[0], ascii)),
                    None => format!("{p} {} …", pretty(&keys[0], ascii)),
                },
            };
            let row = match cmd {
                Command::Action(Tab(n)) if digits && keys.len() == 1 => {
                    (*n == 1).then(|| (format!("{p} 1…9"), "Go to tab 1-9".to_string()))
                }
                Command::Action(side @ (FocusLeft | FocusDown | FocusUp | FocusRight)) if arrows && keys.len() == 1 => {
                    let keys = format!("{p} {}", if ascii { "arrows" } else { "←↓↑→" });
                    (*side == FocusLeft).then(|| (keys, "Focus the pane on that side".to_string()))
                }
                Command::Action(a) => Some((seq(keys), a.describe())),
                Command::Tool(t) => Some((seq(keys), format!("Tool: {t}"))),
            };
            let Some(row) = row else { continue };
            match sections.iter_mut().find(|((t, _), _)| *t == title) {
                Some(((_, rows), seen)) => {
                    if every || !seen.contains(cmd) {
                        seen.push(cmd.clone());
                        rows.push(row);
                    }
                }
                None => sections.push(((title, vec![row]), vec![cmd.clone()])),
            }
        }
        let mut out: Vec<Section> =
            sections.into_iter().map(|(section, _)| section).filter(|(_, rows)| !rows.is_empty()).collect();
        let direct: Vec<(String, String)> = self.direct.iter().map(|(k, a)| (k.to_string(), a.describe())).collect();
        if !direct.is_empty() {
            out.push(("Without the prefix".into(), direct));
        }
        for (title, rows) in MODE_KEYS {
            let shown = |k: &str| {
                if !ascii {
                    return k.to_string();
                }
                [('⏎', "enter"), ('↑', "up"), ('↓', "down"), ('←', "left"), ('→', "right")]
                    .iter()
                    .fold(k.to_string(), |s, (from, to)| s.replace(*from, to))
            };
            out.push((title.to_string(), rows.iter().map(|(k, w)| (shown(k), w.to_string())).collect()));
        }
        out
    }

    /// `ctrl+b p r`, or the direct key: how to reach an action, for hints and help.
    pub fn hint(&self, action: Action) -> Option<String> {
        if let Some((k, _)) = self.direct.iter().find(|(_, a)| *a == action) {
            return Some(k.to_string());
        }
        self.leader_hint(&Command::Action(action))
    }

    /// [`Keymap::hint`] as menus show keys: `ctrl+b P →`.
    pub fn shown_hint(&self, action: Action, ascii: bool) -> Option<String> {
        if let Some((k, _)) = self.direct.iter().find(|(_, a)| *a == action) {
            return Some(pretty(k, ascii));
        }
        let (seq, _) = self.leader.iter().find(|(_, c)| *c == Command::Action(action))?;
        Some(self.after_prefix(seq, |k| pretty(k, ascii)))
    }

    /// The leader sequence of a command, with the prefix.
    pub fn leader_hint(&self, command: &Command) -> Option<String> {
        let (seq, _) = self.leader.iter().find(|(_, c)| c == command)?;
        Some(self.after_prefix(seq, KeySpec::to_string))
    }

    /// Every way to reach `command`: direct keys, then leader sequences.
    pub fn all_hints(&self, command: &Command) -> Vec<String> {
        let mut out: Vec<String> = match command {
            Command::Action(a) => self.direct.iter().filter(|(_, x)| x == a).map(|(k, _)| k.to_string()).collect(),
            Command::Tool(_) => Vec::new(),
        };
        out.extend(
            self.leader.iter().filter(|(_, c)| c == command).map(|(seq, _)| self.after_prefix(seq, KeySpec::to_string)),
        );
        out
    }

    /// A leader sequence after the prefix, each key as `show` writes it.
    fn after_prefix(&self, seq: &[KeySpec], show: impl Fn(&KeySpec) -> String) -> String {
        std::iter::once(self.prefix_label()).chain(seq.iter().map(show)).collect::<Vec<_>>().join(" ")
    }

    /// The prefix as hints show it: its first key.
    pub fn prefix_label(&self) -> String {
        self.prefix.first().map(|k| k.to_string()).unwrap_or_else(|| "prefix".into())
    }
}

/// A key as menus show it: arrows as arrows (unless `ascii`).
pub fn pretty(key: &KeySpec, ascii: bool) -> String {
    if !ascii && key.mods == Default::default() {
        let arrow = match key.code {
            KeyCode::Left => Some("←"),
            KeyCode::Right => Some("→"),
            KeyCode::Up => Some("↑"),
            KeyCode::Down => Some("↓"),
            KeyCode::Enter => Some("⏎"),
            _ => None,
        };
        if let Some(a) = arrow {
            return a.to_string();
        }
    }
    key.to_string()
}

/// The digit a key of the digit row types on AZERTY keyboards (French and Belgian) without
/// Shift, when the terminal did not say where the key is: a key typed right after the prefix
/// can arrive before kraal asks the terminal for places.
fn digit_row(key: &Key) -> Option<Key> {
    let KeyCode::Char(c) = key.code else { return None };
    // A key whose place the terminal gave is read by it; Shift types the digit itself.
    if key.has_place() || key.mods.ctrl || key.mods.alt || key.mods.shift {
        return None;
    }
    let digit = match c {
        '&' => '1',
        'é' => '2',
        '"' => '3',
        '\'' => '4',
        '(' => '5',
        '-' | '§' => '6',
        'è' => '7',
        '_' | '!' => '8',
        'ç' => '9',
        'à' => '0',
        _ => return None,
    };
    Some(Key { code: KeyCode::Char(digit), mods: crate::input::Mods::NONE, raw: key.raw.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(user: &str) -> crate::config::Config {
        crate::config::Config::from_user_toml(user).unwrap()
    }

    fn key(c: char) -> Key {
        Key { code: KeyCode::Char(c), mods: Mods::NONE, raw: vec![c as u8] }
    }

    #[test]
    fn clipboard_image_is_in_the_palette_with_only_opt_in_bindings() {
        for preset in PRESETS {
            let (km, warnings) = Keymap::build(&cfg(&format!("[keys]\npreset = '{}'", preset.name)).keys);
            assert!(warnings.is_empty());
            assert!(!km.direct.iter().any(|(_, action)| *action == AttachClipboardImage));
            assert!(!km.leader.iter().any(|(_, command)| *command == Command::Action(AttachClipboardImage)));
        }
        assert!(ACTIONS.contains(&(AttachClipboardImage, "attach_clipboard_image", "Attach clipboard image")));
        let (km, warnings) = Keymap::build(&cfg("[keys.direct]\nattach_clipboard_image = ['ctrl+v']").keys);
        assert!(warnings.is_empty());
        assert!(km.direct.iter().any(|(_, action)| *action == AttachClipboardImage));
        let (km, warnings) = Keymap::build(&cfg("[keys.leader]\nattach_clipboard_image = ['I']").keys);
        assert!(warnings.is_empty());
        assert_eq!(km.lookup(&[key('I')]), Lookup::Run(Command::Action(AttachClipboardImage)));
    }

    #[test]
    fn presets_bind_cleanly() {
        for preset in PRESETS {
            let (km, warnings) = Keymap::build(&cfg(&format!("[keys]\npreset = \"{}\"\n", preset.name)).keys);
            assert!(warnings.is_empty(), "{}: {warnings:?}", preset.name);
            assert!(km.leader.iter().any(|(_, c)| *c == Command::Action(Tab(9))));
            assert!(km.leader.iter().any(|(_, c)| *c == Command::Action(Help)));
            assert!(km.direct.iter().any(|(_, a)| *a == NextAttention));
        }
        for (a, name, _) in ACTIONS {
            assert_eq!(Action::parse(name), Some(*a));
        }
    }

    #[test]
    fn a_theme_letter_runs_its_main_action_and_its_capital_opens_its_menu() {
        let (km, _) = Keymap::build(&cfg("").keys);
        assert_eq!(km.preset, "kraal");
        let run = |a: Action| Lookup::Run(Command::Action(a));
        for (c, a) in [
            ('s', SshPicker),
            ('b', BroadcastToggle),
            ('w', WorkspacePicker),
            ('a', AgentPicker),
            ('t', NewTab),
            ('p', PanePicker),
            ('v', ScrollMode),
            ('f', Files),
            ('r', ToolPicker),
        ] {
            assert_eq!(km.lookup(&[key(c)]), run(a), "{c}");
            // The same letter inside its menu does the same.
            let menu = key(c.to_ascii_uppercase());
            assert_eq!(km.lookup(std::slice::from_ref(&menu)), Lookup::More, "{c}");
            assert_eq!(km.lookup(&[menu, key(c)]), run(a), "{c} in its menu");
        }
        for (c, a) in [('n', NewPane), ('x', ClosePane), ('z', ToggleZoom), ('m', SelectForBroadcast), ('o', Hints)] {
            assert_eq!(km.lookup(&[key(c)]), run(a), "{c}");
        }
        assert_eq!(km.lookup(&[key('g')]), run(Search));
        assert_eq!(km.lookup(&[key('/')]), run(Search));
        assert_eq!(km.lookup(&[key('?')]), run(Help));
        assert_eq!(km.lookup(&[key('V'), key('p')]), run(Buffers));
        assert_eq!(km.lookup(&[key('=')]), run(Buffers));
        assert!(km.menu(&[key('V')]).iter().any(|e| e.label == "Paste buffers"));
        let right = Key { code: KeyCode::Right, mods: Mods::NONE, raw: Vec::new() };
        assert_eq!(km.lookup(&[key('P'), right]), run(SplitRight));
        // The same verbs in every menu.
        for menu in ['P', 'T', 'W'] {
            assert!(matches!(km.lookup(&[key(menu), key('x')]), Lookup::Run(_)), "{menu} x");
            assert!(matches!(km.lookup(&[key(menu), key('e')]), Lookup::Run(_)), "{menu} e");
        }
    }

    #[test]
    fn the_menu_lists_main_actions_then_keys_then_menus() {
        let (km, _) = Keymap::build(&cfg("").keys);
        let root = km.menu(&[]);
        let first_plain = root.iter().position(|e| !e.main).unwrap();
        let first_menu = root.iter().position(|e| e.group).unwrap();
        assert!(root[..first_plain].iter().all(|e| e.main) && first_plain == 9, "{root:?}");
        assert!(root[first_plain..first_menu].iter().all(|e| !e.main && !e.group));
        assert!(root[first_menu..].iter().all(|e| e.group));
        assert_eq!(root[0].label, "Connect to a server");
        let panes = root.iter().find(|e| e.key == KeySpec::parse("P").unwrap()).unwrap();
        assert!(panes.group && panes.label == "Panes");
        assert!(km.menu(&[key('P')]).iter().any(|e| e.label == "Resize panes"));
        let b: Vec<String> = km.menu(&[key('B')]).into_iter().map(|e| e.label).collect();
        assert_eq!(b[0], "On/off");
        assert!(b.contains(&"To this workspace".to_string()) && b.contains(&"Stop broadcasting".to_string()), "{b:?}");
        assert_eq!(km.hint(SplitRight).as_deref(), Some("ctrl+b P right"));
        assert_eq!(km.shown_hint(SplitRight, false).as_deref(), Some("ctrl+b P →"));
        assert_eq!(km.shown_hint(SplitRight, true).as_deref(), Some("ctrl+b P right"));
        assert_eq!(km.shown_hint(NewPane, false).as_deref(), Some("alt+n"));
        assert_eq!(km.hint(ToggleBottomBar).as_deref(), Some("ctrl+b V k"));
        assert!(km.menu(&[key('V')]).iter().any(|e| e.label == "Bottom bar"));
        assert_eq!(km.hint(SshPicker).as_deref(), Some("ctrl+b s"));
        assert_eq!(km.hint(AddServer).as_deref(), Some("ctrl+b S n"));
        assert_eq!(km.hint(Tunnels).as_deref(), Some("ctrl+b S o"));
        assert_eq!(km.hint(NewPane).as_deref(), Some("alt+n"));
        assert_eq!(km.menu_hint("broadcast").as_deref(), Some("ctrl+b B"));
        // A restart is about the whole session: in the Workspaces menu, under its first letter.
        assert_eq!(km.hint(Restart).as_deref(), Some("ctrl+b W r"));
        assert!(km.menu(&[key('W')]).iter().any(|e| e.label == "Restart the session"));
        // The reference covers the menus and the modes.
        let reference = km.reference(false, false);
        assert!(reference.iter().any(|(t, _)| t == "Servers (ctrl+b S)"), "{reference:?}");
        assert!(reference.iter().any(|(t, _)| t == "Copy mode"));
    }

    #[test]
    fn quit_is_next_to_detach_and_beside_the_restart() {
        let run = |a: Action| Lookup::Run(Command::Action(a));
        let (km, _) = Keymap::build(&cfg("").keys);
        assert_eq!(km.lookup(&[key('d')]), run(Detach));
        assert_eq!(km.lookup(&[key('q')]), run(Quit));
        assert_eq!(km.lookup(&[key('W'), key('q')]), run(Quit));
        assert_eq!(km.hint(Quit).as_deref(), Some("ctrl+b q"));
        let workspaces: Vec<String> = km.menu(&[key('W')]).into_iter().map(|e| e.label).collect();
        assert!(workspaces.ends_with(&["Restart the session".into(), "Quit".into()]), "{workspaces:?}");
        // Classic: `q` detaches too, and `Q` quits.
        let (classic, _) = Keymap::build(&cfg("[keys]\npreset = \"classic\"\n").keys);
        assert_eq!(classic.lookup(&[key('q')]), run(Detach));
        assert_eq!(classic.lookup(&[key('Q')]), run(Quit));
        assert_eq!(classic.hint(Quit).as_deref(), Some("ctrl+b Q"));
    }

    #[test]
    fn the_prefix_twice_sends_it_whatever_it_is() {
        let (km, warnings) = Keymap::build(&cfg("[keys]\nprefix = [\"ctrl+a\", \"ctrl+space\"]\n").keys);
        assert!(warnings.is_empty(), "{warnings:?}");
        let ctrl = |c: char| Key { code: KeyCode::Char(c), mods: Mods::CTRL, raw: Vec::new() };
        assert_eq!(km.lookup(&[ctrl('a')]), Lookup::Run(Command::Action(SendPrefix)));
        assert_eq!(km.lookup(&[ctrl(' ')]), Lookup::Run(Command::Action(SendPrefix)));
        assert_eq!(km.lookup(&[ctrl('b')]), Lookup::Nothing);
    }

    #[test]
    fn menus_move_like_actions() {
        let (km, warnings) = Keymap::build(&cfg("[keys.leader]\nmenu_servers = [\"M\"]\nmenu_view = []\n").keys);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(km.lookup(&[key('M'), key('u')]), Lookup::Run(Command::Action(Upload)));
        assert_eq!(km.lookup(&[key('S')]), Lookup::Nothing);
        assert_eq!(km.group_title(&[KeySpec::parse("M").unwrap()]), Some("Servers"));
        // Without its menu, the View theme keeps its single keys.
        assert_eq!(km.lookup(&[key('V')]), Lookup::Nothing);
        assert_eq!(km.lookup(&[key('v')]), Lookup::Run(Command::Action(ScrollMode)));
        assert!(!km.is_main(ScrollMode));
        let (_, warnings) = Keymap::build(&cfg("[keys.leader]\nmenu_nope = [\"N\"]\n").keys);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        // A menu on a key taken by an action is reported.
        let (_, warnings) = Keymap::build(&cfg("[keys.leader]\nmenu_tabs = [\"n\"]\n").keys);
        assert!(!warnings.is_empty());
    }

    #[test]
    fn user_bindings_replace_the_preset_and_collisions_are_reported() {
        let (km, warnings) = Keymap::build(&cfg("[keys.leader]\nsplit_right = [\"%\"]\nnew_tab = [\"P\"]\n").keys);
        assert_eq!(km.lookup(&[key('%')]), Lookup::Run(Command::Action(SplitRight)));
        assert!(km.all_hints(&Command::Action(SplitRight)).iter().all(|h| !h.contains("right")));
        assert_eq!(warnings.len(), 1, "P is the Panes menu: {warnings:?}");
        let (_, warnings) = Keymap::build(&cfg("[keys]\npreset = \"emacs\"\n").keys);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn the_keys_given_to_an_action_are_listed_where_the_preset_lists_it() {
        // The order of a preset is the order of the listings: no action has two places in it.
        for preset in PRESETS {
            for table in [preset.leader, preset.direct] {
                for (row, (name, _)) in table.iter().enumerate() {
                    assert_eq!(place(table, Action::parse(name).unwrap()), row, "{}: {name}", preset.name);
                }
            }
        }
        // Keys moved, added and taken away, and two actions the preset gives no key.
        let user = "[keys]\npreset = \"classic\"\n\
                    [keys.leader]\nsplit_down = [\"-\", \"_\"]\ntoggle_zoom = [\"z\", \"space\"]\nfiles = [\"e\"]\n\
                    open_preview = []\nhints = [\"f\"]\nmessages = [\"i\"]\n\
                    [keys.direct]\nfocus_up = [\"alt+up\", \"alt+p\"]\nnext_tab = [\"alt+.\"]\n";
        let (km, warnings) = Keymap::build(&cfg(user).keys);
        assert!(warnings.is_empty(), "{warnings:?}");
        // The keys of the first section, without the prefix.
        let after_prefix = |km: &Keymap, every: bool| -> Vec<String> {
            let (title, rows) = km.reference(true, every).remove(0);
            assert_eq!(title, "After ctrl+b");
            rows.into_iter().map(|(keys, _)| keys.strip_prefix("ctrl+b ").unwrap().to_string()).collect()
        };
        let every = after_prefix(&km, true);
        assert_eq!(every[..10], ["%", "|", "-", "_", "x", "z", "space", "t", "F", "e"], "{every:?}");
        let place_of = |key: &str| every.iter().position(|k| k == key).unwrap_or_else(|| panic!("{key}: {every:?}"));
        assert_eq!(place_of("f"), place_of("T") + 1, "hints, after the tools: {every:?}");
        assert_eq!(every.last().map(String::as_str), Some("i"), "what the preset does not bind comes last");
        // The help shows an action once, there too.
        let help = after_prefix(&km, false);
        assert_eq!(help[..8], ["%", "-", "x", "z", "t", "F", "e", "arrows"], "{help:?}");
        // The menu, which has a line per action, lists them in that order as well.
        let menu: Vec<String> = km.menu(&[]).into_iter().map(|e| e.key.to_string()).collect();
        assert_eq!(menu[..7], ["%", "-", "x", "z", "t", "F", "e"], "{menu:?}");
        let direct: Vec<String> = km.direct.iter().map(|(k, _)| k.to_string()).collect();
        assert_eq!(direct[..8], ["alt+h", "alt+left", "alt+l", "alt+right", "alt+up", "alt+p", "alt+j", "alt+down"]);
        assert_eq!(direct.last().map(String::as_str), Some("alt+."));

        // A key two actions ask for still goes to the preset's, wherever the other is listed.
        let (km, warnings) =
            Keymap::build(&cfg("[keys]\npreset = \"classic\"\n[keys.leader]\nsplit_right = [\"x\"]\n").keys);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(km.lookup(&[key('x')]), Lookup::Run(Command::Action(ClosePane)));
    }

    #[test]
    fn every_key_is_listed_and_a_range_only_where_it_is_one() {
        let rows = |km: &Keymap, every: bool| -> Vec<(String, String)> {
            km.reference(true, every).into_iter().flat_map(|(_, rows)| rows).collect()
        };
        let has = |rows: &[(String, String)], keys: &str, what: &str| rows.iter().any(|(k, w)| k == keys && w == what);
        // The default keys: the arrows and the digits are ranges, and a second key has its row.
        let (km, _) = Keymap::build(&cfg("").keys);
        let (help, every) = (rows(&km, false), rows(&km, true));
        for listing in [&help, &every] {
            assert!(has(listing, "ctrl+b arrows", "Focus the pane on that side"), "{listing:?}");
            assert!(has(listing, "ctrl+b 1…9", "Go to tab 1-9"), "{listing:?}");
            assert!(has(listing, "ctrl+b g", "Search the output"), "{listing:?}");
        }
        assert!(has(&every, "ctrl+b /", "Search the output") && !has(&help, "ctrl+b /", "Search the output"));
        assert!(has(&every, "ctrl+b T enter", "New tab") && !has(&help, "ctrl+b T enter", "New tab"));
        // Other keys for a side or a tab: each has its row among every key.
        let (km, _) = Keymap::build(&cfg("[keys.leader]\nfocus_left = [\"left\", \"H\"]\ntab_3 = [\"#\"]\n").keys);
        let every = rows(&km, true);
        for (keys, what) in [("left", "Focus left"), ("H", "Focus left"), ("down", "Focus down")] {
            assert!(has(&every, &format!("ctrl+b {keys}"), what), "{keys}: {every:?}");
        }
        for (keys, what) in [("1", "Go to tab 1"), ("#", "Go to tab 3"), ("9", "Go to tab 9")] {
            assert!(has(&every, &format!("ctrl+b {keys}"), what), "{keys}: {every:?}");
        }
        assert!(!every.iter().any(|(k, _)| k == "ctrl+b arrows" || k == "ctrl+b 1…9"), "{every:?}");
    }

    #[test]
    fn tools_go_in_the_run_menu() {
        let c = cfg("[tools.lazygit]\nkey = \"l\"\n[tools.mine]\ncommand = \"tig\"\nkey = \"g\"\n");
        let (mut km, _) = Keymap::build(&c.keys);
        let warnings = km.bind_tools(&c.tools);
        assert_eq!(warnings.len(), 1, "R g is git: {warnings:?}");
        assert_eq!(km.lookup(&[key('R'), key('l')]), Lookup::Run(Command::Tool("lazygit".into())));
        // Classic: a tool key follows the prefix directly, and may not take a command's key.
        let classic_cfg = cfg(
            "[keys]\npreset = \"classic\"\n[tools.lazygit]\nkey = \"i\"\n[tools.mine]\ncommand = \"tig\"\nkey = \"l\"\n",
        );
        let (mut classic, _) = Keymap::build(&classic_cfg.keys);
        let warnings = classic.bind_tools(&classic_cfg.tools);
        assert!(warnings.len() == 1 && warnings[0].contains("mine"), "l is focus right: {warnings:?}");
        assert_eq!(classic.lookup(&[key('i')]), Lookup::Run(Command::Tool("lazygit".into())));
        assert_eq!(classic.lookup(&[key('l')]), Lookup::Run(Command::Action(FocusRight)));
        // Keys that send the same byte collide (ctrl+m is enter).
        let c = cfg("[tools.mine]\ncommand = \"tig\"\nkey = \"ctrl+m\"\n");
        let (mut km, _) = Keymap::build(&c.keys);
        assert_eq!(km.bind_tools(&c.tools).len(), 1, "R ctrl+m is R enter");
        let (_, warnings) = Keymap::build(&cfg("[keys.direct]\nnew_tab = [\"ctrl+b\"]\n").keys);
        assert!(warnings.iter().any(|w| w.contains("prefix")), "{warnings:?}");
    }

    #[test]
    fn keys_by_their_place_on_the_keyboard() {
        let received = |bytes: &[u8]| match crate::input::InputParser::new().feed(bytes).as_slice() {
            [crate::input::InputEvent::Key(k)] => k.clone(),
            other => panic!("{other:?}"),
        };
        // AZERTY: `&` at the place of `1` is tab 1 in a keymap that does not bind `&`.
        let (km, _) = Keymap::build(&cfg("[keys.leader]\nnew_tab = [\"k\"]\n").keys);
        let amp = received(b"\x1b[38::49;;38u");
        let one = km.leader_key(&[], amp.clone());
        assert_eq!(km.lookup(&[one]), Lookup::Run(Command::Action(Tab(1))));
        // One that binds `&` keeps the key for it: a place never takes a key from a character.
        let (classic, _) = Keymap::build(&cfg("[keys]\npreset = \"classic\"\n").keys);
        let typed = classic.leader_key(&[], amp);
        assert_eq!(classic.lookup(&[typed]), Lookup::Run(Command::Action(CloseTab)));
        // A Russian layout: `л` at the place of `k`, Ctrl+`и` at the place of Ctrl+B.
        let l = km.leader_key(&[], received(b"\x1b[1083::107;;1083u"));
        assert_eq!(km.lookup(&[l]), Lookup::Run(Command::Action(NewTab)));
        assert!(km.is_prefix(&received(b"\x1b[1080::98;5u")));
        assert!(!km.is_prefix(&received(b"\x1b[1080;5u")), "without its place, a Russian letter is itself");
        // Direct keys too: Alt+`т` at the place of Alt+N.
        assert_eq!(km.direct(&received(b"\x1b[1090::110;3u")), Some(NewPane));
        // A Latin letter is the letter it types, wherever it is: AZERTY's `a` at the place of `q`.
        let a = km.leader_key(&[], received(b"\x1b[97::113;;97u"));
        assert_eq!(a.code, KeyCode::Char('a'));
        // Before the terminal says where keys are, AZERTY's digit row still reads as digits,
        // unless the character itself is bound.
        let one = km.leader_key(&[], received(b"&"));
        assert_eq!(km.lookup(&[one]), Lookup::Run(Command::Action(Tab(1))));
        let typed = classic.leader_key(&[], received(b"&"));
        assert_eq!(classic.lookup(&[typed]), Lookup::Run(Command::Action(CloseTab)));
        // Not when the terminal said where the key is: Shift+7 on a US keyboard types `&`.
        let shifted = km.leader_key(&[], received(b"\x1b[55:38:55;2;38u"));
        assert_ne!(km.lookup(&[shifted]), Lookup::Run(Command::Action(Tab(1))));
    }

    #[test]
    fn stacks_and_layouts_are_in_the_panes_menu() {
        let arrow = |code: KeyCode| Key { code, mods: Mods::NONE, raw: Vec::new() };
        let run = |a: Action| Lookup::Run(Command::Action(a));
        let (km, _) = Keymap::build(&cfg("").keys);
        assert_eq!(km.lookup(&[key('P'), key('e')]), run(EditPane));
        assert_eq!(km.lookup(&[key('P'), key('l')]), run(NextLayout));
        assert_eq!(km.lookup(&[key('P'), key('u')]), run(Unstack));
        assert_eq!(km.lookup(&[key('P'), key('s')]), Lookup::More);
        for (code, a) in [
            (KeyCode::Left, StackLeft),
            (KeyCode::Down, StackDown),
            (KeyCode::Up, StackUp),
            (KeyCode::Right, StackRight),
        ] {
            assert_eq!(km.lookup(&[key('P'), key('s'), arrow(code)]), run(a));
        }
        // The keys after `s` have a menu of their own, saying what they stack with.
        let panes = km.menu(&[key('P')]);
        let s = panes.iter().find(|e| e.key == KeySpec::parse("s").unwrap()).unwrap();
        assert!(s.group && s.label == "Stack with", "{s:?}");
        assert!(panes.iter().any(|e| e.label == "Next layout") && panes.iter().any(|e| e.label == "Unstack"));
        let sides: Vec<String> = km.menu(&[key('P'), key('s')]).into_iter().map(|e| e.label).collect();
        assert_eq!(sides, ["The pane on the left", "The pane below", "The pane above", "The pane on the right"]);
        assert_eq!(km.hint(StackUp).as_deref(), Some("ctrl+b P s up"));
        let reference = km.reference(false, false);
        let (_, rows) = reference.iter().find(|(t, _)| t == "Panes (ctrl+b P)").unwrap();
        assert!(rows.iter().any(|(k, w)| k == "ctrl+b P s ←" && w == "Stack with the pane on the left"), "{rows:?}");
        assert!(rows.iter().any(|(k, w)| k == "ctrl+b P l" && w.starts_with("Next layout")), "{rows:?}");
        // Classic: a key to go through layouts, `M` and a side to stack, `u` to unstack.
        let (classic, warnings) = Keymap::build(&cfg("[keys]\npreset = \"classic\"\n").keys);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(classic.lookup(&[key('E')]), run(NextLayout));
        assert_eq!(classic.lookup(&[key('.')]), run(RenamePane));
        assert_eq!(Action::parse("rename_pane"), Some(RenamePane));
        assert_eq!(classic.lookup(&[key('u')]), run(Unstack));
        assert_eq!(classic.lookup(&[key('M'), key('h')]), run(StackLeft));
        assert_eq!(classic.lookup(&[key('M'), arrow(KeyCode::Down)]), run(StackDown));
        let m = classic.menu(&[]).into_iter().find(|e| e.key == KeySpec::parse("M").unwrap()).unwrap();
        assert!(m.group && m.label == "Stack with", "{m:?}");
    }
}
