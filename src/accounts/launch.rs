use anyhow::{Context, Result};

use super::{Account, AgentKind, agent_kind, key};
use crate::paths;

/// A command line for a pane, with its environment.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Launch {
    /// The program, then its arguments.
    pub argv: Vec<String>,
    /// Variables set for it, over what it inherits.
    pub env: Vec<(String, String)>,
    /// Variables removed from what it inherits, so that the account's own login applies.
    pub env_remove: Vec<String>,
}

impl Launch {
    pub fn command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.argv[0]);
        cmd.args(&self.argv[1..]);
        for key in &self.env_remove {
            cmd.env_remove(key);
        }
        cmd.envs(self.env.iter().cloned());
        cmd
    }
}

/// How to start `a`'s agent: resuming `resume` if given, else a new conversation with
/// `new_session` as its id when the agent supports choosing it.
pub fn launch(a: &Account, args: &[String], resume: Option<&str>, new_session: Option<&str>) -> Result<Launch> {
    let kind = agent_kind(&a.agent).context("unknown agent")?;
    let mut l = Launch { argv: vec![kind.command.to_string()], ..Default::default() };
    let home = paths::expand_tilde(&a.home);
    if home != paths::expand_tilde(kind.default_home) {
        l.env.push((kind.home_env.to_string(), home.display().to_string()));
    } else {
        l.env_remove.push(kind.home_env.to_string());
    }
    l.env.push(("KRAAL_ACCOUNT".into(), key(&a.agent, &a.name)));
    l.env.push(("KRAAL_ACCOUNT_ID".into(), a.id.clone()));
    l.env_remove.extend(kind.clear_env.iter().map(|s| s.to_string()));
    match (resume, new_session, kind.session_flag) {
        (Some(id), _, _) => l.argv.extend(kind.resume.iter().map(|s| s.replace("{session}", id))),
        (None, Some(id), Some(flag)) => l.argv.extend([flag.to_string(), id.to_string()]),
        _ => {}
    }
    l.argv.extend(args.iter().cloned());
    Ok(l)
}

/// The arguments an agent was started with, without those [`launch`] adds (the conversation it
/// resumes or the id it gives a new one): what to start it with again.
pub fn launch_args(kind: &AgentKind, argv: &[String]) -> Vec<String> {
    let rest = argv.get(1..).unwrap_or_default();
    let resumes = rest.len() >= kind.resume.len()
        && kind.resume.iter().zip(rest).all(|(t, a)| t.contains("{session}") || *t == a.as_str());
    let skip = if resumes {
        kind.resume.len()
    } else if kind.session_flag.is_some_and(|f| rest.first().is_some_and(|a| a == f)) {
        2
    } else {
        0
    };
    rest.get(skip..).unwrap_or_default().to_vec()
}

/// Keep launch options and prompts, but never select a conversation from the copied pane.
pub fn duplicate_args(kind: &AgentKind, args: &[String]) -> Vec<String> {
    let mut kept = Vec::new();
    let mut prompt = Vec::new();
    let mut args = args.iter().peekable();
    let mut conversation = false;
    let mut selected = false;
    let mut positional = false;
    while let Some(arg) = args.next() {
        let (flag, attached) = arg.split_once('=').map_or((arg.as_str(), false), |(flag, _)| (flag, true));
        if arg == "--" {
            kept.push(arg.clone());
            if conversation && !selected {
                args.next();
            }
            kept.append(&mut prompt);
            kept.extend(args.cloned());
            break;
        }
        if kind.name == "claude" {
            match flag {
                "--session-id" | "--resume" | "-r" | "--from-pr" | "--teleport" => {
                    if !attached {
                        args.next_if(|v| !v.starts_with('-'));
                    }
                    continue;
                }
                "--continue" | "-c" | "--fork-session" => continue,
                _ => {}
            }
        } else if kind.name == "codex" {
            if !positional && matches!(flag, "exec" | "e") {
                kept.push(arg.clone());
                continue;
            }
            if !positional && CODEX_CONVERSATIONS.contains(&flag) {
                conversation = true;
                positional = true;
                continue;
            }
            if conversation && CODEX_CHOOSERS.contains(&flag) {
                selected |= flag == "--last";
                continue;
            }
            if conversation && !arg.starts_with('-') {
                if selected {
                    prompt.push(arg.clone());
                } else {
                    selected = true;
                }
                continue;
            }
        }
        kept.push(arg.clone());
        let (single, several, optional) = if kind.name == "claude" {
            (CLAUDE_VALUE, CLAUDE_VALUES, CLAUDE_OPTIONAL)
        } else {
            (CODEX_VALUE, CODEX_VALUES, &[][..])
        };
        if !attached && single.contains(&flag) {
            kept.extend(args.next().cloned());
        } else if !attached && optional.contains(&flag) {
            kept.extend(args.next_if(|v| !v.starts_with('-')).cloned());
        } else if several.contains(&flag) {
            while let Some(value) = args.next_if(|v| !v.starts_with('-')) {
                kept.push(value.clone());
            }
        } else if !arg.starts_with('-') {
            positional = true;
        }
    }
    // Removing resume/fork exposes the prompt to subcommand parsing. Keep options before
    // the separator so they still apply, even when they originally followed the prompt.
    if prompt.first().is_some_and(|p| CODEX_COMMANDS.contains(&p.as_str()) || CODEX_CONVERSATIONS.contains(&p.as_str()))
    {
        kept.push("--".into());
    }
    kept.extend(prompt);
    kept
}

// The agents' options, from their own `--help`, by what follows them: a word after an option is
// its value or the prompt given on the command line, which `resumable_args` tells apart.

/// Claude's options that take one value.
const CLAUDE_VALUE: &[&str] = &[
    "--model",
    "--fallback-model",
    "--permission-mode",
    "--permission-prompts",
    "--append-system-prompt",
    "--system-prompt",
    "--system-prompt-snapshot",
    "--settings",
    "--setting-sources",
    "--output-format",
    "--input-format",
    "--agents",
    "--agent",
    "--autocompact",
    "--environment",
    "--max-turns",
    "--permission-prompt-tool",
    "--session-id",
    "--effort",
    "--json-schema",
    "--max-budget-usd",
    "--debug-file",
    "--plugin-dir",
    "--plugin-url",
    "--remote",
    "--remote-control-session-name-prefix",
    "--teammate-mode",
    "-n",
    "--name",
];
/// Claude's options whose value may be left out: a word right after one is its value, as Claude
/// reads it (`--remote-control Phone` names the session).
const CLAUDE_OPTIONAL: &[&str] = &["--remote-control", "--rc"];
/// Claude's options that take no value: a word after one of them is a prompt.
const CLAUDE_FLAGS: &[&str] = &[
    "--verbose",
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "--ax-screen-reader",
    "--bare",
    "--bg",
    "--background",
    "--brief",
    "--chrome",
    "--no-chrome",
    "--desktop",
    "--exclude-dynamic-system-prompt-sections",
    "--forward-subagent-text",
    "--ide",
    "--include-hook-events",
    "--strict-mcp-config",
    "--include-partial-messages",
    "--replay-user-messages",
    "--restricted",
    "--safe-mode",
    "--tmux",
    "--fork-session",
    "--mcp-debug",
    "--continue",
    "-c",
    "--no-session-persistence",
    "--disable-slash-commands",
];
/// Codex's options that take no value.
const CODEX_FLAGS: &[&str] = &[
    "--oss",
    "--search",
    "--full-auto",
    "--approve-for-me",
    "--dangerously-bypass-approvals-and-sandbox",
    "--dangerously-bypass-hook-trust",
    "--yolo",
    "--skip-git-repo-check",
    "--strict-config",
    "--worktree",
    "--no-alt-screen",
    "--no-daemon",
];
/// Claude's options that take every word up to the next option.
const CLAUDE_VALUES: &[&str] = &[
    "--add-dir",
    "--mcp-config",
    "--allowedTools",
    "--allowed-tools",
    "--disallowedTools",
    "--disallowed-tools",
    "--betas",
    "--file",
    "--tools",
];
/// Codex's options that take one value.
const CODEX_VALUE: &[&str] = &[
    "-o",
    "--output-last-message",
    "--output-schema",
    "--color",
    "-m",
    "--model",
    "-p",
    "--profile",
    "-c",
    "--config",
    "-s",
    "--sandbox",
    "-a",
    "--ask-for-approval",
    "-C",
    "--cd",
    "--add-dir",
    "--local-provider",
    "--enable",
    "--disable",
    "--remote",
    "--remote-auth-token-env",
];
/// Codex's options that take every word up to the next option.
const CODEX_VALUES: &[&str] = &["-i", "--image"];
/// Claude's options that make a run of its own, not a conversation (`claude -p`).
const CLAUDE_ONE_SHOT: &[&str] = &["-p", "--print", "-h", "--help", "-v", "--version"];
/// Codex's options that make a run of its own, not a conversation.
const CODEX_ONE_SHOT: &[&str] = &["-h", "--help", "-V", "--version"];
/// Claude subcommands: runs of their own, not a conversation.
const CLAUDE_COMMANDS: &[&str] = &[
    "agents",
    "attach",
    "auth",
    "auto-mode",
    "doctor",
    "gateway",
    "import",
    "install",
    "logs",
    "mcp",
    "plugin",
    "plugins",
    "purge",
    "respawn",
    "rm",
    "setup-token",
    "stop",
    "kill",
    "ultrareview",
    "update",
    "upgrade",
];
/// Codex subcommands: runs of their own, not a conversation.
const CODEX_COMMANDS: &[&str] = &[
    "exec",
    "e",
    "review",
    "login",
    "logout",
    "mcp",
    "mcp-server",
    "plugin",
    "app-server",
    "exec-server",
    "remote-control",
    "completion",
    "update",
    "doctor",
    "sandbox",
    "debug",
    "apply",
    "a",
    "queue",
    "archive",
    "unarchive",
    "delete",
    "migrate-rollouts",
    "agents",
    "proto",
    "cloud",
    "features",
    "help",
];
/// Codex subcommands that open an interactive conversation of their own: `resume [ID] [PROMPT]`,
/// and `fork [ID] [PROMPT]`, whose conversation is the fork's, not the one it was taken from.
/// Their id and prompt choose what to start; resuming the conversation they made replaces both.
const CODEX_CONVERSATIONS: &[&str] = &["resume", "fork"];
/// Options of those subcommands that choose the conversation, without a value.
const CODEX_CHOOSERS: &[&str] = &["--last", "--all"];

/// What may be repeated when a conversation is resumed, of the arguments an agent was started
/// with: its options, not a prompt given on the command line (resuming would send it again).
/// `None` for a one-shot run (`claude -p`, `--help`, a Codex subcommand such as `exec`): it is
/// not resumed. The flag is false when a prompt was left out, or when a word could not be told
/// from a prompt (after an option kraal does not know, or one whose value is optional): then
/// everything from that option on is kept exactly as given, since dropping a value breaks the
/// command, and the command must be shown before it runs: it may send a prompt again. A Codex
/// `resume` or `fork` is a conversation: what chose it (its id, `--last`) and its prompt go.
pub fn resumable_args(kind: &AgentKind, args: &[String]) -> Option<(Vec<String>, bool)> {
    type Table = &'static [&'static str];
    let (one_shot, single, several, flags, commands, conversations): (Table, Table, Table, Table, Table, Table) =
        match kind.name {
            "claude" => (CLAUDE_ONE_SHOT, CLAUDE_VALUE, CLAUDE_VALUES, CLAUDE_FLAGS, CLAUDE_COMMANDS, &[]),
            "codex" => (CODEX_ONE_SHOT, CODEX_VALUE, CODEX_VALUES, CODEX_FLAGS, CODEX_COMMANDS, CODEX_CONVERSATIONS),
            _ => return None,
        };
    // A one-shot run says so anywhere among the options, even past one whose value kraal cannot
    // tell from a prompt (`--debug api --print`): such a run is never taken for a conversation.
    let options = &args[..args.iter().position(|a| a == "--").unwrap_or(args.len())];
    if options.iter().any(|a| one_shot.contains(&a.split('=').next().unwrap_or(a))) {
        return None;
    }
    let mut kept = Vec::new();
    let mut exact = true;
    // A word was seen where the subcommand would be (a prompt, or the subcommand itself).
    let mut positional = false;
    // In `codex resume|fork`: how many of its words (id, then prompt) were left out, and whether
    // `--last` chose the conversation (then its first word may be a prompt too).
    let mut conversation: Option<(usize, bool)> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            // What follows is the prompt.
            exact &= i + 1 == args.len();
            break;
        }
        if a.starts_with('-') && a.len() > 1 {
            let (name, attached) = match a.split_once('=') {
                Some((n, _)) => (n, true),
                None => (a.as_str(), false),
            };
            i += 1;
            if let Some((_, last)) = conversation.as_mut()
                && CODEX_CHOOSERS.contains(&name)
            {
                // The conversation resumed now is the one it made.
                *last |= name == "--last";
                continue;
            }
            kept.push(a.clone());
            if flags.contains(&name) || (attached && single.contains(&name)) {
                continue;
            }
            if kind.name == "claude" && CLAUDE_OPTIONAL.contains(&name) {
                if let Some(v) = args.get(i).filter(|v| !attached && !v.starts_with('-')) {
                    kept.push(v.clone());
                    i += 1;
                }
                continue;
            }
            if single.contains(&name) {
                kept.extend(args.get(i).cloned());
                i += 1;
            } else if several.contains(&name) {
                // All its values, the first one possibly given with `=`.
                while let Some(v) = args.get(i).filter(|v| !v.starts_with('-')) {
                    kept.push(v.clone());
                    i += 1;
                }
            } else if args.get(i).is_some_and(|v| !v.starts_with('-')) {
                // An option kraal does not know (or one whose value is optional, such as
                // `--debug [filter]` or `--worktree [name]`), then a word: its value or a prompt
                // (or another value, after `--opt=value`), which only the agent's own parser
                // knows. So is whether a later word is a subcommand (`--worktree update` names a
                // worktree): the rest stays exactly as given, to be shown before it runs.
                kept.extend(args[i..].iter().cloned());
                exact = false;
                break;
            }
        } else {
            i += 1;
            match conversation.as_mut() {
                Some((words, last)) => {
                    // Its id goes; a prompt after it (or after `--last`) is left out.
                    if *words > 0 || *last {
                        exact = false;
                    }
                    *words += 1;
                }
                None if !positional && conversations.contains(&a.as_str()) => conversation = Some((0, false)),
                None if !positional && commands.contains(&a.as_str()) => return None,
                None => {
                    positional = true;
                    exact = false;
                }
            }
        }
    }
    Some((kept, exact))
}

/// A command line that started agent `kind` directly (`claude --model opus`, not through an
/// account), rewritten to resume conversation `session` with the same options (see
/// [`resumable_args`] for the flag). `None` when it cannot be done safely (an odd id, a
/// one-shot run).
pub fn resume_argv(kind: &AgentKind, argv: &[String], session: &str) -> Option<(Vec<String>, bool)> {
    let safe = !session.is_empty() && session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let program = argv.first()?;
    if !safe || program.rsplit('/').next() != Some(kind.command) {
        return None;
    }
    let mut rest: Vec<String> = Vec::new();
    let mut prompt_left_out = false;
    let args = argv[1..].iter();
    match kind.name {
        "claude" => {
            // Whatever chose the conversation before goes: this one is resumed. `--resume`'s
            // value is optional (alone, it opens a picker).
            let mut args = args.peekable();
            while let Some(a) = args.next() {
                let selector = match a.as_str() {
                    "--session-id" => {
                        args.next();
                        true
                    }
                    "--resume" | "-r" => {
                        args.next_if(|v| !v.starts_with('-'));
                        true
                    }
                    "--continue" | "-c" => true,
                    _ => a.starts_with("--resume=") || a.starts_with("--session-id="),
                };
                if !selector {
                    rest.push(a.clone());
                } else if rest.last().is_some_and(|l| CLAUDE_OPTIONAL.contains(&l.as_str()))
                    && args.next_if(|v| !v.starts_with('-')).is_some()
                {
                    // The selector kept the word after it (a prompt) from being that option's
                    // value; a resume leaves the prompt out, as `resumable_args` would.
                    prompt_left_out = true;
                }
            }
        }
        // `codex resume|fork …` is told by [`resumable_args`].
        "codex" => rest.extend(args.cloned()),
        _ => return None,
    }
    let (kept, exact) = resumable_args(kind, &rest)?;
    let exact = exact && !prompt_left_out;
    let mut out = vec![program.clone()];
    out.extend(kind.resume.iter().map(|t| t.replace("{session}", session)));
    out.extend(kept);
    Some((out, exact))
}

/// The agent's own login command for an account.
pub fn login(a: &Account) -> Result<Launch> {
    let kind = agent_kind(&a.agent).context("unknown agent")?;
    agent_command(a, kind.login)
}

/// The agent's own command that tells whether an account is logged in.
pub fn login_status(a: &Account) -> Result<Launch> {
    let kind = agent_kind(&a.agent).context("unknown agent")?;
    agent_command(a, kind.login_status)
}

/// The agent's program with `args`, in an account's environment.
fn agent_command(a: &Account, args: &[&str]) -> Result<Launch> {
    let kind = agent_kind(&a.agent).context("unknown agent")?;
    let mut l = launch(a, &[], None, None)?;
    l.argv = std::iter::once(kind.command.to_string()).chain(args.iter().map(|s| s.to_string())).collect();
    Ok(l)
}
