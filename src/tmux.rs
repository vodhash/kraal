//! Claude Code's agent teams as kraal panes. `kraal agent claude --teams` starts Claude as if it
//! ran inside tmux, with this binary first in its PATH under the name `tmux` ([`install_shim`]).
//! Claude opens, titles and closes its teammates' panes with tmux commands sent to the socket its
//! `TMUX` names: kraal answers them with its own panes. Any other tmux command goes to the real
//! tmux.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};

use crate::protocol::{Placement, Request, Response};
use crate::{client, paths};

/// What [`lead_env`] adds besides PATH, which goes along with a resumed agent already.
const VARS: [&str; 3] = ["CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS", "TMUX", "CLAUDE_CODE_TMUX_TRUECOLOR"];

/// The variables of a Claude whose teammates open as kraal panes, besides its account's.
pub fn lead_env(socket: &Path) -> Vec<(String, String)> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::iter::once(shim_dir()).chain(std::env::split_paths(&path).filter(|d| *d != shim_dir()));
    let path = std::env::join_paths(dirs).unwrap_or(path);
    vec![
        (VARS[0].into(), "1".into()),
        // Claude passes the first field to tmux as `-S`: kraal knows its calls by it.
        (VARS[1].into(), format!("{},0,0", socket.display())),
        // Inside tmux, Claude would keep to 256 colors.
        (VARS[2].into(), "1".into()),
        ("PATH".into(), path.to_string_lossy().into_owned()),
    ]
}

/// Claude's arguments with split-pane teammates, unless they choose a mode already.
pub fn lead_args(mut args: Vec<String>) -> Vec<String> {
    if !args.iter().any(|a| a == "--teammate-mode" || a.starts_with("--teammate-mode=")) {
        args.splice(0..0, ["--teammate-mode".into(), "auto".into()]);
    }
    args
}

/// Keep the variables of [`lead_env`] in the spec that replaces an agent's (resumed, or moved to
/// another account).
pub fn carry_env(from: &[(String, String)], to: &mut Vec<(String, String)>) {
    for (k, v) in from {
        if VARS.contains(&k.as_str()) && !to.iter().any(|(t, _)| t == k) {
            to.push((k.clone(), v.clone()));
        }
    }
}

/// Whether `env` is that of a lead [`lead_env`] made.
pub fn is_lead(env: &[(String, String)]) -> bool {
    env.iter().any(|(k, _)| k == VARS[1])
}

/// A restored lead's variables, for the socket and shim of the server it runs in now.
pub fn refresh_lead(env: &mut Vec<(String, String)>, socket: &Path) {
    if is_lead(env) {
        env.retain(|(k, _)| !VARS.contains(&k.as_str()) && k != "PATH");
        env.extend(lead_env(socket));
    }
}

/// A teammate's variables: its lead's (its account's), without the pretence of tmux.
pub fn teammate_env(lead: &[(String, String)]) -> Vec<(String, String)> {
    lead.iter().filter(|(k, _)| !VARS.contains(&k.as_str()) && k != "PATH").cloned().collect()
}

fn shim_dir() -> PathBuf {
    paths::runtime_dir().join("bin")
}

/// Put `tmux` in [`shim_dir`]: this binary, which answers as tmux when run by that name.
pub fn install_shim() -> std::io::Result<()> {
    let dir = paths::private_runtime_dir()?.join("bin");
    std::fs::create_dir_all(&dir)?;
    let exe = std::env::current_exe()?;
    let link = dir.join("tmux");
    if std::fs::read_link(&link).is_ok_and(|to| to == exe) {
        return Ok(());
    }
    // Replaced in one step: a teammate starting now finds the old link or the new one.
    let tmp = dir.join(format!(".tmux.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&exe, &tmp)?;
    std::fs::rename(&tmp, &link)
}

/// The pane of the Claude this binary answers for, as `tmux`.
struct Lead {
    socket: String,
    pane: String,
    session: String,
    /// The panes it split, one id per line.
    team: PathBuf,
}

impl Lead {
    /// Only a Claude started by `kraal agent --teams`: its `TMUX` names its session's socket.
    fn from_env() -> Option<Lead> {
        let socket = synthetic()?;
        let process = std::env::var("KRAAL_PROCESS").ok()?;
        if process.is_empty() || !process.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return None;
        }
        Some(Lead {
            socket,
            pane: std::env::var("KRAAL_PANE").ok()?,
            session: std::env::var("KRAAL_SESSION").unwrap_or_default(),
            team: paths::runtime_dir().join("teams").join(process),
        })
    }

    fn request(&self, r: Request) -> Result<Response> {
        client::request(Path::new(&self.socket), r)
    }

    /// The panes it split, still open or not.
    fn split(&self) -> Vec<String> {
        std::fs::read_to_string(&self.team).unwrap_or_default().lines().map(str::to_string).collect()
    }

    /// Its own pane, then the panes it split that are still open.
    fn team(&self) -> Result<Vec<String>> {
        let Response::Panes(open) = self.request(Request::ListPanes)? else { bail!("unexpected answer from kraal") };
        let open = |id: &String| open.iter().any(|p| p.id.to_string() == *id);
        Ok(std::iter::once(self.pane.clone()).chain(self.split().into_iter().filter(open)).collect())
    }

    fn joined(&self, pane: u32) -> std::io::Result<()> {
        std::fs::create_dir_all(paths::private_runtime_dir()?.join("teams"))?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&self.team)?;
        writeln!(f, "{pane}")
    }
}

/// kraal's socket when this runs for a lead: the first field of its `TMUX`, which kraal set.
fn synthetic() -> Option<String> {
    let socket = std::env::var("KRAAL_SOCKET").ok()?;
    (std::env::var("TMUX").ok()?.split(',').next() == Some(socket.as_str())).then_some(socket)
}

/// This binary run as `tmux`.
pub fn main(args: Vec<OsString>) -> i32 {
    let words: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let answer = match route(&words, std::env::var("KRAAL_SOCKET").ok().as_deref()) {
        Route::Real => return delegate(args),
        Route::Version if real_tmux().is_some() => return delegate(args),
        Route::Version => Ok("tmux 3.4 (kraal)\n".to_string()),
        Route::Unsupported(o) => Err(anyhow!("kraal does not answer tmux -{o} (only Claude's agent teams)")),
        // Sent to kraal: answered by kraal, or refused, never passed to a real tmux.
        Route::Kraal(start) => match Lead::from_env() {
            _ if args.iter().any(|a| a.to_str().is_none()) => Err(anyhow!("kraal reads only UTF-8 tmux arguments")),
            Some(lead) => on_socket(&lead, &words[start..]),
            None => Err(anyhow!("this pane lacks the kraal variables of an agent team's lead")),
        },
    };
    match answer {
        Ok(out) => {
            print!("{out}");
            0
        }
        Err(e) => {
            eprintln!("kraal: {e:#}");
            1
        }
    }
}

/// Run the real tmux with `args`.
fn delegate(args: Vec<OsString>) -> i32 {
    let Some(tmux) = real_tmux() else {
        eprintln!("kraal: tmux is not installed (kraal answers only Claude's agent teams as tmux)");
        return 127;
    };
    let mut command = std::process::Command::new(tmux);
    // Out of the team's pretend tmux; a real tmux's own session stays where it is.
    if synthetic().is_some() {
        command.env_remove("TMUX").env_remove("TMUX_PANE");
    }
    eprintln!("kraal: running tmux: {}", command.args(args).exec());
    126
}

/// The tmux installed after this one in PATH.
fn real_tmux() -> Option<PathBuf> {
    let me = std::env::current_exe().and_then(std::fs::canonicalize).ok();
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).map(|d| d.join("tmux")).find(|t| t.is_file() && std::fs::canonicalize(t).ok() != me)
}

/// Where a `tmux` command goes.
#[derive(Debug, PartialEq)]
enum Route {
    Real,
    /// `-V`: answered by kraal when no tmux is installed.
    Version,
    /// To kraal's socket, with the command from this word on.
    Kraal(usize),
    /// To kraal's socket, with an option of tmux's own that kraal does not take.
    Unsupported(char),
}

/// kraal answers what is sent to its socket (the last `-S`, as tmux reads them), where Claude
/// sends everything; the rest is for a real tmux.
fn route(words: &[String], socket: Option<&str>) -> Route {
    let Some((global, rest)) = options(words, "cfLST") else { return Route::Real };
    if global.iter().any(|(o, _)| *o == 'V') {
        return Route::Version;
    }
    let last = global.iter().rev().find(|(o, _)| *o == 'S').and_then(|(_, v)| v.as_deref());
    match (socket, global.iter().find(|(o, _)| *o != 'S')) {
        (Some(socket), Some((o, _))) if last == Some(socket) => Route::Unsupported(*o),
        (Some(socket), None) if last == Some(socket) => Route::Kraal(words.len() - rest.len()),
        _ => Route::Real,
    }
}

/// A tmux command sent to kraal's socket: one of those Claude sends for its teammates' panes,
/// on its own pane or theirs. Anything else is refused, never pretended done.
fn on_socket(lead: &Lead, words: &[String]) -> Result<String> {
    let Some((command, rest)) = words.split_first() else { bail!("kraal answers only tmux commands of agent teams") };
    let unsupported = || anyhow!("kraal does not answer tmux {command} (only Claude's agent teams)");
    let values = match command.as_str() {
        "display-message" | "display" => "cdt",
        "list-panes" | "lsp" => "Fft",
        "split-window" | "splitw" => "celptF",
        "respawn-pane" | "respawnp" => "cet",
        "kill-pane" | "killp" | "set-option" | "set" | "select-layout" | "selectl" => "t",
        "select-pane" | "selectp" => "Tt",
        "resize-pane" | "resizep" => "txy",
        _ => return Err(unsupported()),
    };
    let (opts, args) = options(rest, values).ok_or_else(unsupported)?;
    let value = |o: char| opts.iter().rev().find(|(k, _)| *k == o).and_then(|(_, v)| v.clone());
    let has = |o: char| opts.iter().any(|(k, _)| *k == o);
    let only = |allowed: &str| match opts.iter().find(|(o, _)| !allowed.contains(*o)) {
        Some((o, _)) => Err(anyhow!("kraal does not answer tmux {command} -{o} (only Claude's agent teams)")),
        None => Ok(()),
    };
    let pane = match value('t') {
        None => lead.pane.clone(),
        Some(t) => target(&t, lead)?,
    };
    // A word tmux would refuse there (`kill-pane %9`, missing its -t), refused too.
    let most = match command.as_str() {
        "kill-pane" | "killp" | "list-panes" | "lsp" | "select-pane" | "selectp" => 0,
        "select-layout" | "selectl" | "resize-pane" | "resizep" => 1,
        "set-option" | "set" => 2,
        _ => usize::MAX,
    };
    if args.len() > most {
        bail!("kraal does not answer tmux {command} {}", args.join(" "));
    }
    match command.as_str() {
        "display-message" | "display" => {
            only("tp")?;
            Ok(if has('p') { format!("{}\n", expand(&args.join(" "), &pane, lead)?) } else { String::new() })
        }
        "list-panes" | "lsp" => {
            only("tF")?;
            let format = value('F').unwrap_or_else(|| "#{pane_id}".into());
            lead.team()?.iter().map(|p| Ok(format!("{}\n", expand(&format, p, lead)?))).collect()
        }
        "split-window" | "splitw" => {
            only("dhvlptcPF")?;
            // Only the pane a teammate starts in: anything else would run with the server's
            // variables, not the lead's account.
            if args != ["cat"] {
                bail!("kraal splits a pane only for a teammate (split-window -- cat)");
            }
            let format = value('F').unwrap_or_else(|| "#{pane_id}".into());
            expand(&format, &pane, lead)?;
            let opened = lead.request(Request::NewPane {
                target: Some(pane),
                command: vec!["cat".into()],
                cwd: value('c').map(PathBuf::from).or_else(|| std::env::current_dir().ok()),
                name: None,
                placement: if has('h') { Placement::Right } else { Placement::Down },
                preview: None,
                focus: !has('d'),
                close_on_exit: true,
            })?;
            let Response::Pane(info) = opened else { bail!("unexpected answer from kraal") };
            if let Err(e) = lead.joined(info.id) {
                // A pane the lead cannot name would stay for good.
                let _ = lead.request(Request::ClosePane { pane: info.id.to_string() });
                bail!("recording the teammate's pane: {e}");
            }
            Ok(if has('P') { format!("{}\n", expand(&format, &info.id.to_string(), lead)?) } else { String::new() })
        }
        "respawn-pane" | "respawnp" => {
            only("kt")?;
            if !has('k') || args.is_empty() {
                bail!("kraal starts a teammate with respawn-pane -k and its command");
            }
            if pane == lead.pane {
                bail!("kraal starts teammates only in the panes their lead split");
            }
            lead.request(Request::StartTeammate { pane, lead: lead.pane.clone(), command: shell_command(args) })?;
            Ok(String::new())
        }
        "kill-pane" | "killp" => {
            only("t")?;
            lead.request(Request::ClosePane { pane })?;
            Ok(String::new())
        }
        "select-pane" | "selectp" => {
            only("tT")?;
            match value('T') {
                Some(name) => lead.request(Request::RenamePane { pane, name })?,
                None => lead.request(Request::FocusPane { pane })?,
            };
            Ok(String::new())
        }
        // Colors and borders of the team's panes: kraal draws its panes its own way, and keeps a
        // teammate's pane open when it fails (Claude asks `remain-on-exit failed`).
        "set-option" | "set" => {
            only("pwt")?;
            let shown = ["window-style", "pane-border-style", "pane-active-border-style", "pane-border-format"];
            match args {
                [name, _] if (has('p') || has('w')) && value('t').is_some() && shown.contains(&name.as_str()) => {}
                [name, _] if has('w') && name == "pane-border-status" => {}
                [name, v] if has('p') && name == "remain-on-exit" && v == "failed" => {}
                _ => bail!("kraal answers only the border and remain-on-exit options of agent teams"),
            }
            Ok(String::new())
        }
        _ => {
            only(values)?;
            Ok(String::new())
        }
    }
}

/// The pane a tmux target names on kraal's socket: `%ID` the lead or a pane it split, `@ID` the
/// lead's team.
fn target(t: &str, lead: &Lead) -> Result<String> {
    let id = match (t.strip_prefix('%'), t.strip_prefix('@')) {
        (Some(id), _) if id == lead.pane || lead.split().iter().any(|s| s == id) => id,
        (_, Some(id)) if id == lead.pane => id,
        _ => bail!("{t} is not a pane of this agent team"),
    };
    Ok(id.to_string())
}

/// What tmux runs for a command given as words: the shell for none, one through `sh -c`.
fn shell_command(args: &[String]) -> Vec<String> {
    match args {
        [one] => vec!["sh".into(), "-c".into(), one.clone()],
        _ => args.to_vec(),
    }
}

/// tmux's options: those before the first other word (or `--`), each a letter, with a value
/// when it is one of `values` (in the same word or the next one); `None` if one lacks it.
type Opts = Vec<(char, Option<String>)>;
fn options<'a>(words: &'a [String], values: &str) -> Option<(Opts, &'a [String])> {
    let mut opts = Vec::new();
    let mut i = 0;
    while let Some(w) = words.get(i) {
        if w == "--" {
            i += 1;
            break;
        }
        let Some(letters) = w.strip_prefix('-').filter(|l| !l.is_empty()) else { break };
        i += 1;
        for (at, o) in letters.char_indices() {
            if values.contains(o) {
                let attached = &letters[at + o.len_utf8()..];
                let v = if attached.is_empty() {
                    i += 1;
                    words.get(i - 1)?.clone()
                } else {
                    attached.to_string()
                };
                opts.push((o, Some(v)));
                break;
            }
            opts.push((o, None));
        }
    }
    Some((opts, &words[i..]))
}

/// `format` with the tmux variables Claude reads; any other is refused.
fn expand(format: &str, pane: &str, lead: &Lead) -> Result<String> {
    let mut out = String::new();
    let mut rest = format;
    while let Some(start) = rest.find("#{") {
        out.push_str(&rest[..start]);
        let Some(len) = rest[start..].find('}') else { bail!("unfinished tmux format {format:?}") };
        out.push_str(&match &rest[start + 2..start + len] {
            "pane_id" => format!("%{pane}"),
            "window_id" => format!("@{}", lead.pane),
            "session_name" => lead.session.clone(),
            other => bail!("kraal does not answer #{{{other}}} (only Claude's agent teams)"),
        });
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn lead(dir: &TempDir) -> Lead {
        Lead { socket: "/run/k/main".into(), pane: "4".into(), session: "main".into(), team: dir.write("team", "9\n") }
    }

    fn words(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    /// `tmux -S <kraal's socket> CALL`, as kraal answers it.
    fn ask(l: &Lead, call: &str) -> Result<String> {
        let w = words(&format!("-S /run/k/main {call}"));
        let Route::Kraal(start) = route(&w, Some("/run/k/main")) else { panic!("{call} is for kraal") };
        on_socket(l, &w[start..])
    }

    #[test]
    fn claude_learns_its_pane_and_team_window() {
        let dir = TempDir::new("tmux-learns");
        let l = lead(&dir);
        let ask = |s: &str| ask(&l, s).unwrap();
        assert_eq!(ask("display-message -p #{pane_id}"), "%4\n");
        assert_eq!(ask("display-message -t %4 -p #{window_id}"), "@4\n");
        assert_eq!(ask("set-option -p -t %9 pane-border-style fg=red"), "");
        assert_eq!(ask("set-option -p -t %9 remain-on-exit failed"), "");
        assert_eq!(ask("set-option -w -t @4 pane-border-status top"), "");
        assert_eq!(ask("select-layout -t @4 main-vertical"), "");
        assert_eq!(ask("resize-pane -t %4 -x 30%"), "");
    }

    #[test]
    fn what_is_not_sent_to_kraal_goes_to_the_real_tmux() {
        let ours = Some("/run/k/main");
        for call in [
            "new-session -d -s build make",
            "kill-pane -t %9",
            "-L claude-swarm-12 list-panes -t claude-swarm:swarm-view",
            "-S /tmp/other display-message -p #{pane_id}",
            "-S /run/k/main -S /tmp/tools.sock kill-pane -t %4",
            "-S",
        ] {
            assert_eq!(route(&words(call), ours), Route::Real, "{call}");
        }
        assert_eq!(route(&words("-S /run/k/main display-message -p #{pane_id}"), None), Route::Real);
        assert_eq!(route(&words("-S /tmp/tools.sock -S /run/k/main lsp"), ours), Route::Kraal(4));
        assert_eq!(route(&words("-V"), ours), Route::Version);
        assert_eq!(route(&words("-S /run/k/main -h kill-pane -t %4"), ours), Route::Unsupported('h'));
    }

    #[test]
    fn kraal_refuses_what_it_does_not_do() {
        let dir = TempDir::new("tmux-refuses");
        let l = lead(&dir);
        let refused = |s: &str| ask(&l, s).unwrap_err().to_string();
        assert_eq!(
            refused("split-window -b -t %4 -- cat"),
            "kraal does not answer tmux split-window -b (only Claude's agent teams)"
        );
        for call in [
            "kill-pane -t %7",
            "kill-pane -a -t %4",
            "list-panes -t @7",
            "show-environment -g",
            "set-option -g default-shell /bin/zsh",
            "set-option -p -t %9 remain-on-exit on",
            "set-option -w -t @4 definitely-not-an-option x",
            "display-message -p #{version}",
            "respawn-pane -k -t %4 -- claude",
            "respawn-pane -t %9 -- claude",
            "respawn-pane -k -t %9",
            "kill-pane %9",
            "split-window -d -- claude -p review",
            "split-window -d",
            "select-pane -t %9 researcher",
            "split-window -t",
        ] {
            assert!(!refused(call).is_empty(), "{call}");
        }
        assert!(refused("kill-pane -t %7").contains("not a pane of this agent team"));
    }

    #[test]
    fn options_read_like_getopt() {
        let w = words("-dh -l70% -t %3 -P -F #{pane_id} -- cat");
        let (opts, rest) = options(&w, "celptF").unwrap();
        let o: Vec<(char, Option<&str>)> = opts.iter().map(|(c, v)| (*c, v.as_deref())).collect();
        assert_eq!(
            o,
            [('d', None), ('h', None), ('l', Some("70%")), ('t', Some("%3")), ('P', None), ('F', Some("#{pane_id}"))]
        );
        assert_eq!(rest, ["cat"]);
        assert!(options(&words("-t"), "t").is_none());
        let line = ["cd /x && claude".to_string()];
        assert_eq!(shell_command(&line), ["sh", "-c", "cd /x && claude"].map(String::from));
    }

    #[test]
    fn formats_keep_text_and_refuse_unknown_variables() {
        let dir = TempDir::new("tmux-formats");
        let l = lead(&dir);
        assert_eq!(expand("#{session_name}:#{window_id}.#{pane_id} x", "7", &l).unwrap(), "main:@4.%7 x");
        assert!(expand("#{pane_tty}", "7", &l).is_err());
        assert!(expand("a #{pane_id", "7", &l).is_err());
    }

    #[test]
    fn teams_args_and_env() {
        assert_eq!(lead_args(words("-c")), words("--teammate-mode auto -c"));
        assert_eq!(lead_args(words("--teammate-mode=in-process")), words("--teammate-mode=in-process"));
        let env = lead_env(Path::new("/run/k/main"));
        assert!(is_lead(&env) && env.contains(&("TMUX".into(), "/run/k/main,0,0".into())));
        let path = &env.iter().find(|(k, _)| k == "PATH").unwrap().1;
        assert!(path.starts_with(&shim_dir().display().to_string()));
        let mut to = vec![("TMUX".to_string(), "kept".to_string())];
        carry_env(&env, &mut to);
        assert_eq!(to.len(), 3);
        assert_eq!(to[0].1, "kept");
        let mut moved = vec![("TMUX".to_string(), "/old,0,0".to_string()), ("PATH".into(), "/old/bin".into())];
        refresh_lead(&mut moved, Path::new("/run/k/main"));
        assert!(
            moved.contains(&("TMUX".into(), "/run/k/main,0,0".into()))
                && !moved.iter().any(|(_, v)| v.contains("/old"))
        );
        let mut account = env.clone();
        account.push(("CLAUDE_CONFIG_DIR".into(), "/a".into()));
        assert_eq!(teammate_env(&account), [("CLAUDE_CONFIG_DIR".to_string(), "/a".to_string())]);
    }
}
