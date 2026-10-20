//! Interactive client modes: pickers, prompts, confirmations, help, and mouse drags.

use crate::agent::AgentState;
use crate::layout::{Border, PaneId};
use crate::theme::NoticeKind;

pub enum Mode {
    Normal,
    /// The leader key was pressed; `typed` are the keys after it so far, and the menu shows
    /// what can follow. `miss` says a key led nowhere.
    Leader {
        typed: Vec<crate::input::Key>,
        miss: Option<String>,
    },
    /// Resizing panes with the arrows until Enter or Esc.
    Resize,
    /// Browsing the focused pane's scrollback.
    Scroll,
    /// Typing a search in the focused pane's scrollback.
    Search {
        query: String,
        found: bool,
    },
    Picker(Picker),
    Prompt(Prompt),
    ServerForm(Prompt<Box<super::server_ui::ServerForm>>),
    TunnelForm(Prompt<Box<super::tunnel_ui::TunnelForm>>),
    WorkspaceForm(Prompt<Box<super::workspace_ui::WorkspaceForm>>),
    AccountForm(super::account_ui::AccountForm),
    PaneForm(Box<super::pane_ui::PaneForm>),
    AccountDetails(super::account_ui::AccountDetails),
    TunnelDetails(super::tunnel_ui::TunnelDetails),
    Help {
        scroll: u16,
    },
    /// The messages kraal showed (and the warnings it had), newest first.
    Messages {
        scroll: u16,
    },
    Confirm(Question),
    /// Labels over what can be acted on in a pane.
    Hints(HintsMode),
}

/// A question with two buttons.
pub struct Question {
    pub title: Option<String>,
    /// The question on its first line, then what there is to know before answering. A line
    /// that starts with a space quotes what the question is about (a command, a list entry):
    /// the box shows it with its spacing, all of it.
    pub message: String,
    pub action: ConfirmAction,
    /// The confirming button is selected (Enter confirms); Cancel is, at first.
    pub yes: bool,
    /// First row of the message shown, when it is too long for the box.
    pub scroll: usize,
    pub workspace_back: Option<Box<PickerState>>,
}

/// Hints in progress.
pub struct HintsMode {
    pub pane: PaneId,
    pub hints: Vec<crate::hints::Hint>,
    /// The label typed so far.
    pub typed: String,
    /// A label opens or runs its target instead of copying it.
    pub act: bool,
    /// The pane as it was when the labels were made: shown under them while the pane goes on.
    pub snapshot: Option<Box<ratatui::buffer::Buffer>>,
}

impl Mode {
    /// What the top bar says the client is doing; nothing in normal use.
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Normal => "",
            Mode::Leader { .. } => "MENU",
            Mode::Resize => "RESIZE",
            Mode::Scroll => "COPY",
            Mode::Search { .. } => "SEARCH",
            Mode::Picker(_) => "CHOOSE",
            Mode::Prompt(_)
            | Mode::ServerForm(_)
            | Mode::TunnelForm(_)
            | Mode::WorkspaceForm(_)
            | Mode::AccountForm(_)
            | Mode::PaneForm(_) => "INPUT",
            Mode::AccountDetails(_) => "ACCOUNT",
            Mode::TunnelDetails(_) => "TUNNEL",
            Mode::Help { .. } => "HELP",
            Mode::Messages { .. } => "MESSAGES",
            Mode::Confirm(_) => "CONFIRM",
            Mode::Hints(_) => "HINTS",
        }
    }

    /// The leader menu, at its start.
    pub fn leader() -> Mode {
        Mode::Leader { typed: Vec::new(), miss: None }
    }

    /// A question, Cancel selected.
    pub fn confirm(message: impl Into<String>, action: ConfirmAction) -> Mode {
        Mode::Confirm(Question {
            title: None,
            message: message.into(),
            action,
            yes: false,
            scroll: 0,
            workspace_back: None,
        })
    }
}

pub enum ConfirmAction {
    PurgeAttachments,
    AttachFiles(PaneId),
    AttachClipboardImage(PaneId),
    LoadWorkspace {
        prompt: Prompt<Box<super::workspace_ui::WorkspaceForm>>,
        inventory: Vec<super::persistence::ClosePane>,
    },
    RemoveAccount(crate::accounts::Account),
    ReplaceWorkspaceFile {
        prompt: Prompt<Box<super::workspace_ui::WorkspaceForm>>,
        save: super::definitions::DefinitionSave,
    },
    UseExisting {
        existing: Box<crate::accounts::Account>,
        back: Option<Box<PickerState>>,
        target: Option<crate::accounts::Account>,
    },
    RetrySsh(PaneId),
    ClosePane(PaneId),
    ApplyPane(Box<super::pane_ui::PaneForm>),
    CloseTab {
        workspace: u32,
        tab: u32,
    },
    CloseWorkspace {
        workspace: u32,
        inventory: super::persistence::CloseInventory,
    },
    RetryCloseWorkspace(u32),
    DeleteWorkspace {
        target: super::workspace_lifecycle::DeleteTarget,
        stop: bool,
    },
    RetryDeleteWorkspace(super::workspace_lifecycle::DeleteTarget),
    ReorderWorkspace {
        workspace: crate::persist::WorkspaceKey,
        neighbor: crate::persist::WorkspaceKey,
        before: bool,
        order: Option<Vec<crate::persist::WorkspaceKey>>,
    },
    OpenWorkspace(PickAction),
    /// Restart the session on the kraal installed now.
    Restart,
    /// Stop the session, its layout saved.
    Quit,
    LaptopCopy {
        owner: String,
        spec: crate::laptop::copy::Spec,
    },
    /// Run `kraal <args>` in a job pane.
    Job {
        label: String,
        args: Vec<String>,
    },
    StopAndSwitch {
        pane: PaneId,
        token: u32,
    },
    SignInAccount(crate::accounts::Account),
    RetryAccountSwitch {
        pane: PaneId,
        account: crate::accounts::Account,
        undo: bool,
    },
    /// Paste this text into an agent's pane, then Enter: a pull request's failed checks or
    /// review comments, as the user was shown them.
    SendToAgent {
        pane: PaneId,
        /// The repository (main checkout) and branch the text is about.
        main: std::path::PathBuf,
        branch: String,
        text: String,
    },
    /// Run a git shell script in a job pane.
    GitJob {
        label: String,
        script: String,
    },
    /// Remove exactly these worktrees, those still in the state the user was shown.
    RemoveWorktrees(Vec<(crate::git::Worktree, crate::git::WorktreeState)>),
    /// Trust this version of a repository's worktree setup, then make the worktree.
    TrustWorktreeSetup(Box<crate::server::worktrees::NewWorktree>),
    /// Start a tool on a production pane.
    RunTool {
        pane: Option<PaneId>,
        name: String,
        selection: Option<String>,
        /// The production reason the user was asked about.
        reason: crate::server::contexts::Production,
        /// Where the tool was asked for (see `Server::tool_place`): it runs there or not at all.
        place: Option<String>,
    },
    /// Edit a file of a production server in the local editor.
    Edit(Box<super::edits::EditTarget>),
    /// Trust the version of a repository runbook being prepared, then go on preparing it.
    TrustRunbook,
    /// Run the runbook being prepared (its plan is the message).
    RunRunbook,
    /// Run a block of the Markdown runbook in progress; `production` is the target's production
    /// reason the question showed (what a yes approves).
    RunBlock {
        index: usize,
        production: Option<crate::server::contexts::Production>,
    },
    /// What an agent asked to do on production ([`Server::show_production_ask`]): let it go on.
    ApproveProduction(u32),
    /// Input about to reach production panes: approve them for synced typing and send it.
    Production {
        workspace: u32,
        /// Panes and the production reason the user is asked about.
        approve: Vec<(PaneId, crate::server::contexts::Production)>,
        pending: PendingInput,
        /// The production panes the question named, and whether it was about a paste: what your
        /// answer is logged for.
        asked: Vec<(PaneId, crate::server::contexts::Production)>,
        paste: bool,
    },
}

impl ConfirmAction {
    /// The confirming button: what will happen.
    pub fn verb(&self) -> String {
        match self {
            ConfirmAction::LoadWorkspace { prompt, .. } => {
                if prompt.data.undo { "Stop and undo" } else { "Stop and load" }.into()
            }
            ConfirmAction::ReplaceWorkspaceFile { .. } => "Replace file".into(),
            ConfirmAction::UseExisting { .. } => "Use account".into(),
            ConfirmAction::RemoveAccount(_) => "Remove".into(),
            ConfirmAction::RetrySsh(_) => "Try again".into(),
            ConfirmAction::ClosePane(_) => "Close pane".into(),
            ConfirmAction::ApplyPane(_) => "Stop and apply".into(),
            ConfirmAction::CloseTab { .. } => "Close tab".into(),
            ConfirmAction::CloseWorkspace { .. } => "Stop and close".into(),
            ConfirmAction::DeleteWorkspace { stop, .. } => if *stop { "Stop and delete" } else { "Delete" }.into(),
            ConfirmAction::RetryDeleteWorkspace(_) => "Retry".into(),
            ConfirmAction::ReorderWorkspace { .. } => "Retry".into(),
            ConfirmAction::RetryCloseWorkspace(_) | ConfirmAction::OpenWorkspace(_) => "Retry".into(),
            ConfirmAction::Restart => "Restart".into(),
            ConfirmAction::AttachFiles(_) | ConfirmAction::AttachClipboardImage(_) => "Attach".into(),
            ConfirmAction::PurgeAttachments => "Purge".into(),
            ConfirmAction::LaptopCopy { spec, .. } => if spec.operation == crate::laptop::copy::Operation::Move {
                "Move"
            } else if spec.allow.is_empty() {
                "Copy"
            } else {
                "Copy and replace"
            }
            .into(),
            ConfirmAction::Quit => "Quit".into(),
            ConfirmAction::Job { label, .. } | ConfirmAction::GitJob { label, .. } => {
                let mut l = label.clone();
                if let Some(first) = l.get_mut(..1) {
                    first.make_ascii_uppercase();
                }
                l
            }
            ConfirmAction::StopAndSwitch { .. } => "Stop and switch".into(),
            ConfirmAction::ApproveProduction(_) => "Allow".into(),
            ConfirmAction::SignInAccount(_) => "Sign in".into(),
            ConfirmAction::RetryAccountSwitch { .. } => "Check again".into(),
            ConfirmAction::RemoveWorktrees(list) if list.len() == 1 => "Remove worktree".into(),
            ConfirmAction::RemoveWorktrees(_) => "Remove worktrees".into(),
            ConfirmAction::TrustWorktreeSetup(_) => "Trust this version".into(),
            ConfirmAction::RunTool { .. } => "Run tool".into(),
            ConfirmAction::Edit(_) => "Edit".into(),
            ConfirmAction::TrustRunbook => "Trust this version".into(),
            ConfirmAction::RunRunbook => "Run".into(),
            ConfirmAction::RunBlock { .. } => "Send to the pane".into(),
            ConfirmAction::SendToAgent { .. } => "Send to the agent".into(),
            ConfirmAction::Production { .. } => "Send".into(),
        }
    }

    pub fn cancel_label(&self) -> &'static str {
        if matches!(self, Self::RetrySsh(_)) { "Close" } else { "Cancel" }
    }

    /// Production is involved, or the action destroys something.
    pub fn danger(&self, message: &str) -> bool {
        matches!(
            self,
            ConfirmAction::Production { .. } | ConfirmAction::RemoveWorktrees(_) | ConfirmAction::LaptopCopy { .. }
        ) || matches!(self, ConfirmAction::Job { label, .. } if label == "delete" || label.contains("replace"))
            || message.contains("PRODUCTION")
            || message.contains(" is production (")
    }
}

/// Input held back until a production confirmation is answered: keys and pastes, in order (a
/// key, a paste, or what was typed while a context was read), and the panes they were going to
/// with what made them production then. It goes to those panes, not to wherever the focus is by
/// the time of the answer, and all of it goes or none: not at all if one of them became another
/// production meanwhile.
pub struct PendingInput {
    pub items: Vec<InputItem>,
    pub targets: Vec<(PaneId, Option<crate::server::contexts::Production>)>,
}

/// A key or a paste on its way to panes.
pub enum InputItem {
    Key(crate::input::Key),
    Paste(Vec<u8>),
    /// A piece of a paste too big to hold whole, going out as it comes; the flag says the piece
    /// before it ended with CR. Never asked about: such a paste never goes to production.
    Piece(Vec<u8>, bool),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// Every command, to search and run.
    Commands,
    Workspace,
    DeletedWorkspaces,
    Ssh,
    Tunnels,
    Pane,
    Agent,
    AccountReview,
    Accounts,
    Tool,
    Runbook,
    RunbookTarget,
    RunbookParam,
    RunbookSteps,
    /// What to do with an edited file whose server's version changed meanwhile.
    EditConflict,
    /// The paste buffers.
    Buffers,
}

#[derive(Clone)]
pub enum PickAction {
    AccountDetails(String),
    AddAccount,
    ReviewAccount(crate::accounts::Account),
    CheckExisting {
        agent: String,
    },
    UseExisting {
        existing: Box<crate::accounts::Account>,
        target: Option<crate::accounts::Account>,
    },
    CancelSwitch {
        pane: PaneId,
        token: u32,
    },
    SwitchWorkspace(u32),
    ReopenWorkspace(u32),
    RetryWorkspaceClose(u32),
    RetryWorkspaceDelete(u32),
    DeletedWorkspaces,
    RestoreWorkspace(String),
    /// A definition not running, by its file.
    OpenDefinition(std::path::PathBuf),
    UnavailableDefinition(std::path::PathBuf),
    AddWorkspace,
    AddServer(Option<Box<crate::servers::Server>>),
    AddTunnel,
    Tunnel {
        id: u32,
        retry: bool,
    },
    /// A server name or `user@host[:port]`.
    Ssh(String),
    SshGroup(String),
    FocusPane {
        pane: PaneId,
        agent: bool,
        undo: bool,
    },
    OpenAnotherAccount(PaneId),
    /// An agent of a server's kraal session: the server, and its pane's id there.
    RemoteAgent {
        server: String,
        pane: u32,
    },
    AgentLaunch {
        agent: String,
        account: Option<String>,
    },
    AgentSwitch {
        pane: PaneId,
        agent: String,
        account: String,
        current: bool,
    },
    Worktree {
        pane: PaneId,
        op: String,
    },
    /// A runbook file to run.
    Runbook(std::path::PathBuf),
    /// Where the runbook being prepared runs: a server or `@group`.
    RunbookTarget(String),
    /// The value of the parameter being asked.
    RunbookValue(String),
    /// Run a block of the Markdown runbook in progress.
    RunBlock(usize),
    /// Back to the list of runbooks.
    RunbookList,
    /// Start a tool for a pane.
    Tool {
        pane: Option<PaneId>,
        name: String,
    },
    /// Run a command of the keymap.
    Command(crate::keymap::Command),
    /// The answer about an edited file whose server's version changed meanwhile.
    EditAnswer {
        edit: u32,
        answer: super::edits::Answer,
    },
    /// A paste buffer, by its id.
    Buffer(u64),
    /// Point a file manager panel at `~` (local) or `server:~`.
    FilesLaptop {
        pane: PaneId,
        side: crate::files::Side,
    },
    FilesSet {
        pane: PaneId,
        side: crate::files::Side,
        target: String,
    },
}

impl PickAction {
    pub fn definition(&self) -> Option<&std::path::Path> {
        match self {
            Self::OpenDefinition(path) | Self::UnavailableDefinition(path) => Some(path),
            _ => None,
        }
    }

    pub fn workspace(&self) -> Option<u32> {
        match self {
            Self::SwitchWorkspace(ws)
            | Self::ReopenWorkspace(ws)
            | Self::RetryWorkspaceClose(ws)
            | Self::RetryWorkspaceDelete(ws) => Some(*ws),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct PickItem {
    pub label: String,
    pub detail: String,
    pub action: PickAction,
    pub state: Option<AgentState>,
    /// Shown dimmed when an entry is secondary.
    pub dim: bool,
}

impl PickItem {
    pub fn new(label: impl Into<String>, detail: impl Into<String>, action: PickAction) -> PickItem {
        PickItem { label: label.into(), detail: detail.into(), action, state: None, dim: false }
    }

    pub fn dim(self) -> PickItem {
        PickItem { dim: true, ..self }
    }

    pub fn with_state(self, state: Option<AgentState>) -> PickItem {
        PickItem { state, ..self }
    }
}

pub struct Picker {
    pub kind: PickerKind,
    pub title: String,
    pub query: String,
    /// Cursor in `query`, in characters.
    pub cursor: usize,
    pub items: Vec<PickItem>,
    /// Keep room for rows removed by a background refresh, so the list does not move under a click.
    pub min_items: usize,
    pub two_line: bool,
    /// An add row stays last even when the query matches no entries.
    pub pinned: Option<usize>,
    /// Indices into `items` matching the query, best first.
    pub filtered: Vec<usize>,
    pub selected: usize,
    /// The list of actions on the selected item is open, at this one.
    pub actions: Option<usize>,
    pub drag: Option<WorkspaceDrag>,
    /// What the picker waits for (a list read off the event loop), by request: keys typed
    /// meanwhile stay here instead of reaching a pane.
    pub loading: Option<u32>,
    pub worktree: bool,
    pub message: Option<String>,
    pub message_kind: NoticeKind,
    pub undo: Option<String>,
    pub source_workspaces: Vec<u32>,
    pub review_workspaces: Vec<u32>,
    pub layout_undos: Vec<u32>,
    pub back: Option<Box<PickerState>>,
    /// Actions opened without a list must not make that list the form's return destination.
    pub standalone_actions: bool,
    /// Results may select a new row; Close restores the list that opened the form.
    pub close_to: Option<Box<PickerState>>,
}

#[derive(Clone)]
pub struct PickerState {
    pub kind: PickerKind,
    query: String,
    cursor: usize,
    selected: Option<String>,
    pane: Option<PaneId>,
    workspace: Option<u32>,
    definition: Option<std::path::PathBuf>,
    /// Keep an open menu's actions, target and neighbors as they were when shown.
    actions: Option<(usize, Vec<PickItem>)>,
    source_workspaces: Vec<u32>,
    review_workspaces: Vec<u32>,
    layout_undos: Vec<u32>,
    message: Option<String>,
    message_kind: NoticeKind,
    min_items: usize,
    undo: Option<String>,
    back: Option<Box<PickerState>>,
    restored: Option<String>,
    standalone_actions: bool,
    close_to: Option<Box<PickerState>>,
}

impl Picker {
    pub fn new(kind: PickerKind, title: &str, items: Vec<PickItem>) -> Picker {
        let mut p = Picker {
            kind,
            title: title.into(),
            query: String::new(),
            cursor: 0,
            min_items: items.len(),
            two_line: matches!(kind, PickerKind::Accounts | PickerKind::Workspace | PickerKind::DeletedWorkspaces),
            pinned: items.iter().position(|i| {
                matches!(
                    i.action,
                    PickAction::AddAccount
                        | PickAction::AddWorkspace
                        | PickAction::AddServer(None)
                        | PickAction::AddTunnel
                )
            }),
            items,
            filtered: Vec::new(),
            selected: 0,
            actions: None,
            drag: None,
            loading: None,
            worktree: false,
            message: None,
            message_kind: NoticeKind::Notice,
            undo: None,
            source_workspaces: Vec::new(),
            review_workspaces: Vec::new(),
            layout_undos: Vec::new(),
            back: None,
            standalone_actions: false,
            close_to: None,
        };
        p.refilter();
        p
    }

    pub fn form_back(&self) -> Option<PickerState> {
        if self.kind == PickerKind::AccountReview {
            return self.close_to.as_deref().cloned();
        }
        if self.standalone_actions || self.kind == PickerKind::Commands {
            return None;
        }
        let mut state = self.state();
        state.actions = None;
        Some(state)
    }

    pub fn state(&self) -> PickerState {
        PickerState {
            kind: self.kind,
            standalone_actions: self.standalone_actions,
            close_to: self.close_to.clone(),
            query: self.query.clone(),
            cursor: self.cursor,
            selected: self.current().map(|i| i.label.clone()),
            pane: self.current().and_then(|i| match i.action {
                PickAction::FocusPane { pane, .. } => Some(pane),
                _ => None,
            }),
            workspace: self.current().and_then(|i| i.action.workspace()),
            definition: self.current().and_then(|i| i.action.definition().map(std::path::Path::to_path_buf)),
            actions: self.actions.map(|at| (at, self.items.clone())),
            source_workspaces: self.source_workspaces.clone(),
            review_workspaces: self.review_workspaces.clone(),
            layout_undos: self.layout_undos.clone(),
            message: self.message.clone(),
            message_kind: self.message_kind,
            min_items: self.min_items,
            undo: self.undo.clone(),
            back: self.back.clone(),
            restored: self.current().and_then(|i| match &i.action {
                PickAction::RestoreWorkspace(id) => Some(id.clone()),
                _ => None,
            }),
        }
    }

    pub fn restore(&mut self, state: PickerState) {
        self.standalone_actions = state.standalone_actions;
        self.close_to = state.close_to;
        self.min_items = self.min_items.max(state.min_items);
        self.message = state.message;
        self.message_kind = state.message_kind;
        self.undo = state.undo;
        self.back = state.back;
        self.query = state.query;
        self.cursor = state.cursor;
        if let Some((at, items)) = state.actions {
            self.source_workspaces = state.source_workspaces;
            self.review_workspaces = state.review_workspaces;
            self.layout_undos = state.layout_undos;
            self.items = items;
            self.pinned = self.items.iter().position(|i| {
                matches!(
                    i.action,
                    PickAction::AddAccount
                        | PickAction::AddWorkspace
                        | PickAction::AddServer(None)
                        | PickAction::AddTunnel
                )
            });
            self.actions = Some(at);
        }
        self.refilter();
        if let Some(id) = state.restored {
            self.select(|i| matches!(&i.action, PickAction::RestoreWorkspace(w) if *w == id));
        } else if let Some(ws) = state.workspace {
            self.select(|i| i.action.workspace() == Some(ws));
        } else if let Some(path) = state.definition {
            self.select(|i| i.action.definition() == Some(path.as_path()));
        } else if let Some(pane) = state.pane {
            self.select(|i| matches!(i.action, PickAction::FocusPane { pane: p, .. } if p == pane));
        } else if let Some(label) = state.selected {
            self.select(|i| i.label == label);
        }
    }

    pub fn select(&mut self, matches: impl Fn(&PickItem) -> bool) {
        self.selected = self.filtered.iter().position(|&i| matches(&self.items[i])).unwrap_or(0);
    }

    /// Clear the search only when no matching row is visible, even if the target is absent.
    /// Keep an open action menu; callers decide whether their result closes it.
    pub fn reveal(&mut self, matches: impl Fn(&PickItem) -> bool) {
        if !self.filtered.iter().any(|&i| matches(&self.items[i])) {
            self.query.clear();
            self.cursor = 0;
            self.refilter();
        }
        self.select(matches);
    }

    pub fn result(mut self, kind: NoticeKind, message: impl Into<String>) -> Mode {
        self.message = Some(message.into());
        self.message_kind = kind;
        Mode::Picker(self)
    }

    /// An empty key keeps an action in the menu without adding a direct shortcut.
    pub fn action_keys(&self) -> Vec<(&'static str, &'static str)> {
        use PickAction as A;
        use PickerKind as K;
        let Some(item) = self.current() else { return Vec::new() };
        let keys: &[(&str, &str)] = match (self.kind, &item.action) {
            (K::Ssh, A::Ssh(_)) => &[
                ("enter", "Connect"),
                ("ctrl+t", "Connect in a new tab"),
                ("ctrl+f", "Browse its files"),
                ("ctrl+r", "Run a command on it"),
                ("ctrl+o", "Add tunnel"),
                ("ctrl+e", "Edit saved servers"),
                ("ctrl+x", "Remove saved server"),
            ],
            (K::Ssh, A::SshGroup(_)) => {
                &[("enter", "Open group"), ("ctrl+r", "Run a command on it"), ("ctrl+e", "Edit saved servers")]
            }
            (K::Agent, A::AgentLaunch { account: None, .. }) if self.worktree => {
                &[("ctrl+t", "Start in new tab"), ("ctrl+w", "Start in worktree"), ("ctrl+d", "Accounts")]
            }
            (K::Agent, A::AgentLaunch { account: None, .. }) => {
                &[("ctrl+t", "Start in new tab"), ("ctrl+d", "Accounts")]
            }
            (K::Agent, A::AgentLaunch { .. }) if self.worktree => {
                &[("ctrl+t", "Start in new tab"), ("ctrl+w", "Start in worktree"), ("ctrl+d", "Account details")]
            }
            (K::Agent, A::AgentLaunch { .. }) => &[("ctrl+t", "Start in new tab"), ("ctrl+d", "Account details")],
            (_, A::AccountDetails(_)) => &[("ctrl+r", "Rename"), ("ctrl+l", "Sign in"), ("ctrl+x", "Remove")],
            (_, A::CancelSwitch { .. }) => &[("enter", "Cancel account change")],
            (_, A::AgentSwitch { current: true, .. }) => &[("enter", "Keep current account")],
            (_, A::AgentSwitch { .. }) => &[("enter", "Switch account")],
            (_, A::ReviewAccount(_)) => &[("enter", "Review")],
            (_, A::CheckExisting { .. }) => &[("enter", "Check again")],
            (_, A::UseExisting { .. }) => &[("enter", "Choose")],
            (_, A::AddAccount) => &[("enter", "Add account")],
            (K::Workspace, A::RetryWorkspaceClose(_)) => &[("enter", "Retry Close")],
            (K::Workspace, A::RetryWorkspaceDelete(_)) => &[("enter", "Retry Delete")],
            (K::Workspace, A::ReopenWorkspace(_)) => &[
                ("enter", "Open"),
                ("ctrl+r", "Rename"),
                ("", "Duplicate"),
                ("ctrl+s", "Export layout"),
                ("", "Delete"),
            ],
            (K::Workspace, A::SwitchWorkspace(_)) => &[
                ("enter", "Open"),
                ("ctrl+r", "Rename"),
                ("", "Duplicate"),
                ("ctrl+s", "Export layout"),
                ("ctrl+x", "Close"),
                ("", "Delete"),
            ],
            (_, A::OpenDefinition(_)) => &[("enter", "Open"), ("", "Delete")],
            (_, A::DeletedWorkspaces) => &[("enter", "View")],
            (_, A::RestoreWorkspace(_)) => &[("enter", "Restore")],
            (_, A::Tunnel { retry: true, .. }) => &[("enter", "Details"), ("ctrl+r", "Retry now"), ("ctrl+x", "Stop")],
            (_, A::Tunnel { .. }) => &[("enter", "Details"), ("ctrl+x", "Stop")],
            (_, A::AddWorkspace | A::AddServer(_) | A::AddTunnel) => &[("enter", "Add")],
            (K::Runbook, A::Runbook(_)) => &[("enter", "Run"), ("ctrl+e", "Edit")],
            (K::RunbookSteps, A::RunBlock(_)) => &[("enter", "Run step"), ("ctrl+y", "Copy step")],
            (_, A::RunbookList) => &[("enter", "Back to runbooks")],
            (_, A::Command(_)) => &[("enter", "Run")],
            (_, A::FocusPane { undo: true, .. }) => &[
                ("enter", "Go to pane"),
                ("ctrl+e", "Edit pane"),
                ("ctrl+a", "Change account"),
                ("ctrl+u", "Undo account change"),
            ],
            (_, A::FocusPane { agent: true, .. }) => {
                &[("enter", "Go to pane"), ("ctrl+e", "Edit pane"), ("ctrl+a", "Change account")]
            }
            (_, A::FocusPane { .. }) => &[("enter", "Go to pane"), ("ctrl+e", "Edit pane")],
            (_, A::RemoteAgent { .. }) => &[("enter", "Go to pane")],
            (_, A::OpenAnotherAccount(_)) => &[("enter", "Open with another account")],
            (_, A::FilesSet { .. } | A::FilesLaptop { .. }) => &[("enter", "Open directory")],
            (_, A::Tool { .. }) => &[("enter", "Start tool")],
            (K::Buffers, A::Buffer(_)) => &[
                ("enter", "Paste into the pane"),
                ("ctrl+y", "Copy to the clipboard again"),
                ("ctrl+o", "Show in a preview pane"),
                ("ctrl+x", "Delete"),
            ],
            (_, A::UnavailableDefinition(_)) => &[("enter", "Review")],
            _ => &[("enter", "Choose")],
        };
        let mut keys = keys.to_vec();
        if let Some(ws) = item.action.workspace()
            && matches!(item.action, A::SwitchWorkspace(_) | A::ReopenWorkspace(_))
        {
            if self.source_workspaces.contains(&ws) {
                let insert = keys.iter().position(|(_, action)| matches!(*action, "Close" | "Delete")).unwrap();
                keys.insert(insert, ("", "Load from file"));
                if self.review_workspaces.contains(&ws) {
                    keys.insert(insert + 1, ("", "Review"));
                }
            }
            if self.layout_undos.contains(&ws) {
                let insert = keys.iter().position(|(_, action)| matches!(*action, "Close" | "Delete")).unwrap();
                keys.insert(insert, ("", "Undo load"));
            }
        }
        if self.kind == K::Workspace
            && matches!(item.action, A::SwitchWorkspace(_) | A::ReopenWorkspace(_) | A::OpenDefinition(_))
        {
            let rows: Vec<_> = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, i)| i.action.workspace().is_some() || i.action.definition().is_some())
                .map(|(i, _)| i)
                .collect();
            let at = rows.iter().position(|i| Some(i) == self.filtered.get(self.selected)).unwrap();
            let insert = keys.iter().position(|(_, action)| matches!(*action, "Export layout" | "Delete")).unwrap();
            let mut moves = Vec::new();
            if rows.len() > 1 && !self.query.is_empty() {
                moves.push(("", "Clear search to reorder"));
            } else {
                if at > 0 {
                    moves.push(("", "Move up"));
                }
                if at + 1 < rows.len() {
                    moves.push(("", "Move down"));
                }
            }
            keys.splice(insert..insert, moves);
        }
        keys
    }

    pub fn refilter(&mut self) {
        let mut scored: Vec<(i32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != self.pinned)
            .filter_map(|(i, it)| {
                if matches!(it.action, PickAction::DeletedWorkspaces) {
                    return Some((i32::MIN, i));
                }
                // The label matters most; details only rescue items whose label does not match.
                fuzzy_score(&self.query, &it.label)
                    .or_else(|| fuzzy_score(&self.query, &it.detail).map(|s| s - 1000))
                    .map(|s| (s, i))
            })
            .collect();
        if !self.query.is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        self.filtered = scored.into_iter().map(|(_, i)| i).collect();
        if let Some(i) = self.pinned.filter(|&i| i < self.items.len()) {
            self.filtered.push(i);
        }
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    pub fn current(&self) -> Option<&PickItem> {
        self.filtered.get(self.selected).map(|&i| &self.items[i])
    }

    pub fn move_by(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let n = self.filtered.len() as i32;
        self.selected = (self.selected as i32 + delta).rem_euclid(n) as usize;
    }
}

pub enum PromptAction {
    RenameTab {
        workspace: u32,
        tab: u32,
    },
    RenamePane(PaneId),
    OpenPreview,
    /// The value is local paths to upload to `server`.
    UploadPick {
        server: String,
    },
    /// The value is `server:path` to download into the download directory.
    Download,
    /// The value is the account's new name.
    RenameAccount {
        agent: String,
        account: String,
    },
    /// The value is the worktree's name.
    NewWorktree {
        agent: String,
        account: Option<String>,
    },
    /// The value is a commit message for the pane's worktree.
    Commit {
        pane: PaneId,
    },
    /// The value is a command to run on these servers (`kraal exec`).
    RunOn {
        targets: Vec<String>,
    },
    /// The value of the runbook parameter being asked.
    RunbookValue,
}

impl PromptAction {
    /// What Enter does, for the prompt's footer.
    pub fn verb(&self) -> &'static str {
        match self {
            PromptAction::RenameTab { .. } | PromptAction::RenamePane(_) | PromptAction::RenameAccount { .. } => {
                "rename"
            }
            PromptAction::NewWorktree { .. } => "create",
            PromptAction::OpenPreview => "open",
            PromptAction::UploadPick { .. } => "upload",
            PromptAction::Download => "download",
            PromptAction::Commit { .. } => "commit",
            PromptAction::RunOn { .. } => "run",
            PromptAction::RunbookValue => "next",
        }
    }
}

pub struct Prompt<T = PromptAction> {
    pub title: String,
    pub value: String,
    /// Cursor in `value`, in characters.
    pub cursor: usize,
    pub data: T,
    /// Why the value was not accepted; the prompt stays open to fix it.
    pub error: Option<String>,
    pub scroll: usize,
    /// What is typed is not shown (a secret).
    pub masked: bool,
    pub back: Option<PickerState>,
}

impl<T> Prompt<T> {
    pub fn new(title: &str, value: &str, data: T) -> Self {
        Self {
            title: title.into(),
            value: value.into(),
            cursor: value.chars().count(),
            data,
            error: None,
            scroll: 0,
            masked: false,
            back: None,
        }
    }
}

/// Edit a one-line text field with `k`: cursor moves, deletions, typing. Returns false when `k`
/// is not an editing key. `cursor` counts characters.
pub fn edit_line(text: &mut String, cursor: &mut usize, k: &crate::input::Key) -> bool {
    use crate::input::KeyCode as K;
    let chars: Vec<char> = text.chars().collect();
    let mut at = (*cursor).min(chars.len());
    let word_left = |at: usize| {
        let mut i = at;
        while i > 0 && !chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    };
    let word_right = |at: usize| {
        let mut i = at;
        while i < chars.len() && !chars[i].is_alphanumeric() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_alphanumeric() {
            i += 1;
        }
        i
    };
    let (ctrl, alt) = (k.mods.ctrl, k.mods.alt);
    let mut next: Vec<char> = chars.clone();
    match k.code {
        K::Left if alt || ctrl => at = word_left(at),
        K::Right if alt || ctrl => at = word_right(at),
        K::Left => at = at.saturating_sub(1),
        K::Right => at = (at + 1).min(chars.len()),
        K::Home => at = 0,
        K::End => at = chars.len(),
        K::Backspace if alt || ctrl => {
            let from = word_left(at);
            next.drain(from..at);
            at = from;
        }
        K::Backspace if at > 0 => {
            next.remove(at - 1);
            at -= 1;
        }
        K::Backspace => {}
        K::Delete if at < chars.len() => {
            next.remove(at);
        }
        K::Delete => {}
        K::Char(c) if ctrl => match c {
            'a' => at = 0,
            'e' => at = chars.len(),
            'b' => at = at.saturating_sub(1),
            'f' => at = (at + 1).min(chars.len()),
            'd' if at < chars.len() => {
                next.remove(at);
            }
            'h' if at > 0 => {
                next.remove(at - 1);
                at -= 1;
            }
            'w' => {
                // Back to the previous space or slash, like a shell.
                let mut from = at;
                while from > 0 && matches!(chars[from - 1], ' ' | '/') {
                    from -= 1;
                }
                while from > 0 && !matches!(chars[from - 1], ' ' | '/') {
                    from -= 1;
                }
                next.drain(from..at);
                at = from;
            }
            'u' => {
                next.drain(..at);
                at = 0;
            }
            'k' => {
                next.truncate(at);
            }
            _ => return false,
        },
        K::Char('b') if alt => at = word_left(at),
        K::Char('f') if alt => at = word_right(at),
        K::Char('d') if alt => {
            let to = word_right(at);
            next.drain(at..to);
        }
        K::Char(_) if alt => return false,
        K::Char(_) if chars.len() >= MAX_FIELD => {}
        K::Char(c) => {
            next.insert(at, c);
            at += 1;
        }
        _ => return false,
    }
    *text = next.into_iter().collect();
    *cursor = at;
    true
}

/// The longest a one-line field gets: a path, a command, a name.
const MAX_FIELD: usize = 4096;

/// Insert pasted text (its first line) at the cursor; refused whole when it does not fit.
pub fn paste_line(text: &mut String, cursor: &mut usize, pasted: &str) -> Result<(), String> {
    let line = pasted.lines().next().unwrap_or("");
    if text.chars().count() + line.chars().count() > MAX_FIELD {
        return Err(format!("Too long to paste: a field holds at most {MAX_FIELD} characters."));
    }
    let at: usize = text.char_indices().nth(*cursor).map(|(i, _)| i).unwrap_or(text.len());
    text.insert_str(at, line);
    *cursor += line.chars().count();
    Ok(())
}

pub struct WorkspaceDrag {
    pub workspace: crate::persist::WorkspaceKey,
    pub order: Vec<crate::persist::WorkspaceKey>,
    pub from: (u16, u16),
    /// The first visible picker row when the gesture began.
    pub list_start: usize,
    pub moved: bool,
    pub target: Option<crate::persist::WorkspaceKey>,
    pub hover: Option<usize>,
}

/// A mouse drag in progress.
pub enum Drag {
    Workspace {
        drag: WorkspaceDrag,
        picker: Box<Picker>,
    },
    /// Moving a split: the border, and how far before it the pointer took hold of it.
    Border(Border, u16),
    /// Selecting text: `from` is where the button went down; `moved`, whether the selection
    /// covers more than that press (a drag, or a double/triple click).
    Select {
        pane: PaneId,
        from: (u16, u16),
        moved: bool,
    },
    MoveFloat {
        pane: PaneId,
        grab_x: u16,
        grab_y: u16,
    },
    ResizeFloat {
        pane: PaneId,
    },
    /// Press went to a program that handles the mouse itself; drag and release follow it.
    Forward {
        pane: PaneId,
        button: crate::input::MouseButton,
    },
}

/// Subsequence fuzzy match. `None` if `query` does not match; higher is better.
fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let q: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0;
    let mut qi = 0;
    let mut prev_match: Option<usize> = None;
    for (ti, ch) in t.iter().enumerate() {
        if qi < q.len() && *ch == q[qi] {
            score += 1;
            if prev_match == Some(ti.wrapping_sub(1)) {
                score += 5;
            }
            if ti == 0 || matches!(t[ti - 1], ' ' | '-' | '_' | '.' | '/' | '@' | '›') {
                score += 8;
            }
            prev_match = Some(ti);
            qi += 1;
        }
    }
    if qi < q.len() {
        return None;
    }
    // Prefer shorter texts (tighter matches).
    Some(score * 4 - t.len() as i32 / 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveal_clears_only_a_hidden_target_search_and_preserves_open_actions() {
        for (query, target, expected_query, expected_row) in [
            ("b", "beta", "b", "beta"),
            ("alpha", "beta", "", "beta"),
            ("alpha", "missing", "", "alpha"),
            ("missing", "+ Add server", "missing", "+ Add server"),
        ] {
            let mut picker = Picker::new(
                PickerKind::Ssh,
                "Servers",
                vec![
                    PickItem::new("alpha", "", PickAction::Ssh("alpha".into())),
                    PickItem::new("beta", "", PickAction::Ssh("beta".into())),
                    PickItem::new("+ Add server", "", PickAction::AddServer(None)),
                ],
            );
            picker.query = query.into();
            picker.cursor = 1;
            picker.refilter();
            picker.actions = Some(1);
            picker.reveal(|item| item.label == target);
            assert_eq!(picker.query, expected_query);
            assert_eq!(picker.cursor, usize::from(!expected_query.is_empty()));
            assert_eq!(picker.current().unwrap().label, expected_row);
            assert_eq!(picker.actions, Some(1));
            assert_eq!(picker.filtered.last(), Some(&2));
        }
        let mut picker = Picker::new(PickerKind::Ssh, "Servers", Vec::new());
        picker.query = "missing".into();
        picker.cursor = 7;
        picker.reveal(|_| true);
        assert_eq!((picker.query.as_str(), picker.cursor, picker.selected), ("", 0, 0));
        assert!(picker.current().is_none());
    }

    #[test]
    fn workspace_selection_survives_renames_and_distinguishes_same_named_files() {
        for actions in [
            [PickAction::SwitchWorkspace(1), PickAction::ReopenWorkspace(2)],
            [PickAction::OpenDefinition("one/same.kdl".into()), PickAction::OpenDefinition("two/same.kdl".into())],
        ] {
            let mut old = Picker::new(
                PickerKind::Workspace,
                "Workspaces",
                actions.iter().cloned().map(|a| PickItem::new("same", "", a)).collect(),
            );
            old.selected = 1;
            let mut refreshed = Picker::new(
                PickerKind::Workspace,
                "Workspaces",
                vec![PickItem::new("renamed", "", actions[1].clone()), PickItem::new("same", "", actions[0].clone())],
            );
            refreshed.restore(old.state());
            assert_eq!(refreshed.current().unwrap().label, "renamed");
        }
    }

    #[test]
    fn list_refresh_and_result_keep_the_query_selection_and_room_for_removed_rows() {
        let item = |name: &str| PickItem::new(name, "", PickAction::AccountDetails(name.into()));
        let mut old = Picker::new(PickerKind::Accounts, "Accounts", vec![item("One"), item("Two"), item("Other")]);
        old.query = "o".into();
        old.cursor = 1;
        old.refilter();
        old.select(|i| i.label == "Two");
        old.message = Some("Account added".into());
        old.message_kind = NoticeKind::Notice;
        let mut refreshed = Picker::new(PickerKind::Accounts, "Accounts", vec![item("Two"), item("One")]);
        refreshed.restore(old.state());
        assert_eq!(refreshed.query, "o");
        assert_eq!(refreshed.cursor, 1);
        assert_eq!(refreshed.current().unwrap().label, "Two");
        assert_eq!(refreshed.min_items, 3);
        assert_eq!(refreshed.message.as_deref(), Some("Account added"));
        let mut missing = Picker::new(PickerKind::Accounts, "Accounts", vec![item("One")]);
        missing.restore(refreshed.state());
        assert_eq!(missing.current().unwrap().label, "One");
        let Mode::Picker(result) = refreshed.result(NoticeKind::Notice, "Done") else { panic!() };
        assert_eq!(result.query, "o");
        assert_eq!(result.current().unwrap().label, "Two");
        assert_eq!(result.message.as_deref(), Some("Done"));
        for kind in [NoticeKind::Notice, NoticeKind::Warning, NoticeKind::Error] {
            let Mode::Picker(result) = missing.result(kind, "result") else { panic!() };
            missing = Picker::new(PickerKind::Accounts, "Accounts", vec![item("One")]);
            missing.restore(result.state());
            assert_eq!(missing.message_kind, kind);
            assert_eq!(missing.message.as_deref(), Some("result"));
        }
    }

    #[test]
    fn a_pinned_list_row_and_two_line_geometry_do_not_depend_on_the_account_kind() {
        let mut p = Picker::new(
            PickerKind::Tool,
            "List",
            vec![
                PickItem::new("One", "Details", PickAction::UnavailableDefinition("invalid.kdl".into())),
                PickItem::new("Add", "", PickAction::UnavailableDefinition("invalid.kdl".into())),
            ],
        );
        p.pinned = Some(1);
        p.two_line = true;
        p.query = "no matches".into();
        p.refilter();
        assert_eq!(p.filtered, [1]);
        assert_eq!(p.current().unwrap().label, "Add");
        p.query.clear();
        p.refilter();
        assert_eq!(p.filtered, [0, 1]);
        let layout = super::super::overlays::picker_layout(&p, ratatui::layout::Rect::new(0, 0, 80, 24));
        assert_eq!(layout.row_height, 2);
        assert_eq!(layout.row_at(layout.list.x, layout.list.y), Some(0));
        assert_eq!(layout.row_at(layout.list.x, layout.list.y + 1), Some(0));
        assert_eq!(layout.row_at(layout.list.x, layout.list.y + 2), Some(1));
    }

    #[test]
    fn fuzzy_prefers_word_starts_and_runs() {
        assert!(fuzzy_score("tp", "team-prod-nginx").unwrap() > fuzzy_score("tp", "settings-path").unwrap_or(-100));
        assert!(fuzzy_score("xyz", "tig").is_none());
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        assert!(fuzzy_score("worker1", "admin@worker10.lan").is_some());
    }

    #[test]
    fn line_editing() {
        use crate::input::{Key, KeyCode, Mods};
        let key = |code: KeyCode, mods: Mods| Key { code, mods, raw: Vec::new() };
        let ch = |c: char| key(KeyCode::Char(c), Mods::NONE);
        let ctrl = |c: char| key(KeyCode::Char(c), Mods::CTRL);
        let (mut t, mut c) = (String::from("kraal web"), 9);
        assert!(edit_line(&mut t, &mut c, &key(KeyCode::Left, Mods::ALT)));
        assert_eq!(c, 6);
        edit_line(&mut t, &mut c, &ch('X'));
        assert_eq!((t.as_str(), c), ("kraal Xweb", 7));
        edit_line(&mut t, &mut c, &ctrl('a'));
        edit_line(&mut t, &mut c, &key(KeyCode::Delete, Mods::NONE));
        assert_eq!(t, "raal Xweb");
        edit_line(&mut t, &mut c, &ctrl('e'));
        edit_line(&mut t, &mut c, &ctrl('w'));
        assert_eq!(t, "raal ");
        edit_line(&mut t, &mut c, &key(KeyCode::Home, Mods::NONE));
        edit_line(&mut t, &mut c, &ctrl('k'));
        assert_eq!((t.as_str(), c), ("", 0));
        assert!(!edit_line(&mut t, &mut c, &key(KeyCode::Enter, Mods::NONE)));
        let (mut t, mut c) = (String::from("é日"), 1);
        paste_line(&mut t, &mut c, "ab\ncd").unwrap();
        assert!(paste_line(&mut t, &mut c, &"x".repeat(MAX_FIELD)).is_err(), "refused whole");
        assert_eq!((t.as_str(), c), ("éab日", 3));
    }

    #[test]
    fn native_agent_actions_keep_tab_and_worktree_launches() {
        let mut picker = Picker::new(
            PickerKind::Agent,
            "Start an agent",
            vec![PickItem::new(
                "Gemini",
                "Accounts are managed in Gemini",
                PickAction::AgentLaunch { agent: "gemini".into(), account: None },
            )],
        );
        assert_eq!(picker.action_keys(), [("ctrl+t", "Start in new tab"), ("ctrl+d", "Accounts")]);
        picker.worktree = true;
        assert_eq!(
            picker.action_keys(),
            [("ctrl+t", "Start in new tab"), ("ctrl+w", "Start in worktree"), ("ctrl+d", "Accounts"),]
        );
    }

    #[test]
    fn workspace_search_keeps_a_uniform_add_row() {
        let items = vec![
            PickItem {
                label: "perso".into(),
                detail: String::new(),
                action: PickAction::SwitchWorkspace(1),
                state: None,
                dim: false,
            },
            PickItem {
                label: "+ Add workspace".into(),
                detail: String::new(),
                action: PickAction::AddWorkspace,
                state: None,
                dim: false,
            },
        ];
        let mut p = Picker::new(PickerKind::Workspace, "ws", items);
        assert_eq!(p.filtered.len(), 2);
        p.query = "blog".into();
        p.refilter();
        assert_eq!(p.filtered.len(), 1);
        assert_eq!(p.current().unwrap().label, "+ Add workspace");
        assert_eq!(p.action_keys(), &[("enter", "Add")]);
    }

    #[test]
    fn picker_actions_follow_the_selected_line() {
        let mut p = Picker::new(
            PickerKind::Agent,
            "Agents",
            vec![PickItem::new(
                "Claude · work",
                "",
                PickAction::AgentLaunch { agent: "claude".into(), account: Some("work".into()) },
            )],
        );
        assert_eq!(p.action_keys(), &[("ctrl+t", "Start in new tab"), ("ctrl+d", "Account details")]);
        p.worktree = true;
        assert_eq!(
            p.action_keys(),
            &[("ctrl+t", "Start in new tab"), ("ctrl+w", "Start in worktree"), ("ctrl+d", "Account details")]
        );
        let p = Picker::new(
            PickerKind::Accounts,
            "Accounts",
            vec![PickItem::new("Claude · work", "", PickAction::AccountDetails("id".into()))],
        );
        assert_eq!(p.action_keys(), &[("ctrl+r", "Rename"), ("ctrl+l", "Sign in"), ("ctrl+x", "Remove")]);

        for (current, what) in [(false, "Switch account"), (true, "Keep current account")] {
            let p = Picker::new(
                PickerKind::Agent,
                "Change account",
                vec![PickItem::new(
                    "codex·work",
                    "",
                    PickAction::AgentSwitch { pane: 1, agent: "codex".into(), account: "work".into(), current },
                )],
            );
            assert_eq!(p.action_keys(), &[("enter", what)]);
        }
    }

    #[test]
    fn source_actions_stay_stable_while_their_menu_is_open() {
        let list = || {
            Picker::new(
                PickerKind::Workspace,
                "Workspaces",
                vec![PickItem::new("personal", "Open", PickAction::SwitchWorkspace(1))],
            )
        };
        let mut shown = list();
        assert!(!shown.action_keys().iter().any(|(_, label)| *label == "Load from file"));
        shown.source_workspaces.push(1);
        shown.review_workspaces.push(1);
        shown.layout_undos.push(1);
        let expected = shown.action_keys();
        for action in ["Load from file", "Review", "Undo load"] {
            assert!(expected.contains(&("", action)));
        }
        shown.actions = Some(expected.len() - 1);
        let mut refreshed = list();
        refreshed.restore(shown.state());
        assert_eq!(refreshed.action_keys(), expected);
        assert_eq!(refreshed.actions, shown.actions);
        refreshed.actions = None;
        let mut fresh = list();
        fresh.restore(refreshed.state());
        assert!(!fresh.action_keys().iter().any(|(_, label)| *label == "Undo load"));
    }

    #[test]
    fn picker_actions_require_the_line_their_handlers_use() {
        let cases = [
            (
                PickerKind::Ssh,
                PickAction::SshGroup("web".into()),
                &[("enter", "Open group"), ("ctrl+r", "Run a command on it"), ("ctrl+e", "Edit saved servers")][..],
            ),
            (
                PickerKind::Workspace,
                PickAction::SwitchWorkspace(1),
                &[
                    ("enter", "Open"),
                    ("ctrl+r", "Rename"),
                    ("", "Duplicate"),
                    ("ctrl+s", "Export layout"),
                    ("ctrl+x", "Close"),
                    ("", "Delete"),
                ],
            ),
            (PickerKind::Workspace, PickAction::OpenDefinition("w.kdl".into()), &[("enter", "Open"), ("", "Delete")]),
            (PickerKind::RunbookSteps, PickAction::RunBlock(0), &[("enter", "Run step"), ("ctrl+y", "Copy step")]),
            (PickerKind::RunbookSteps, PickAction::RunbookList, &[("enter", "Back to runbooks")]),
            (
                PickerKind::Pane,
                PickAction::FilesSet { pane: 1, side: crate::files::Side::Left, target: "~".into() },
                &[("enter", "Open directory")],
            ),
            (PickerKind::Pane, PickAction::Worktree { pane: 1, op: "review".into() }, &[("enter", "Choose")]),
            (PickerKind::Workspace, PickAction::UnavailableDefinition("invalid.kdl".into()), &[("enter", "Review")]),
        ];
        for (kind, action, expected) in cases {
            let p = Picker::new(kind, "List", vec![PickItem::new("item", "", action)]);
            assert_eq!(p.action_keys(), expected);
        }
        let p = Picker::new(PickerKind::Ssh, "Servers", vec![PickItem::new("web", "", PickAction::Ssh("web".into()))]);
        assert_eq!(
            p.action_keys().iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            ["enter", "ctrl+t", "ctrl+f", "ctrl+r", "ctrl+o", "ctrl+e", "ctrl+x"]
        );
    }

    #[test]
    fn a_picker_without_a_selected_line_offers_no_actions() {
        let mut p = Picker::new(PickerKind::Agent, "Agents", vec![]);
        assert!(p.action_keys().is_empty());
        p.items.push(PickItem::new(
            "claude",
            "",
            PickAction::AgentLaunch { agent: "claude".into(), account: Some("work".into()) },
        ));
        p.query = "no match".into();
        p.refilter();
        assert!(p.action_keys().is_empty());
    }
}
