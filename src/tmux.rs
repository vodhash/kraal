//! Claude Code's agent teams as kraal panes. `kraal agent claude --teams` starts Claude as if it
//! ran inside tmux, with this binary first in its PATH under the name `tmux` ([`install_shim`]).
//! Claude opens, titles and closes its teammates' panes with tmux commands: kraal answers them
//! with its own panes. Any other tmux command goes to the real tmux.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

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
        let socket = std::env::var("KRAAL_SOCKET").ok()?;
        if std::env::var("TMUX").ok()?.split(',').next() != Some(socket.as_str()) {
            return None;
        }
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

    /// Its own pane, then the panes it split that are still open.
    fn team(&self) -> Result<Vec<String>> {
        let Response::Panes(open) = self.request(Request::ListPanes)? else { bail!("unexpected answer from kraal") };
        let split = std::fs::read_to_string(&self.team).unwrap_or_default();
        let open = |id: &&str| open.iter().any(|p| p.id.to_string() == *id);
        Ok(std::iter::once(self.pane.clone()).chain(split.lines().filter(open).map(str::to_string)).collect())
    }

    fn joined(&self, pane: u32) -> std::io::Result<()> {
        std::fs::create_dir_all(paths::private_runtime_dir()?.join("teams"))?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&self.team)?;
        writeln!(f, "{pane}")
    }
}

/// This binary run as `tmux`.
pub fn main(args: Vec<OsString>) -> i32 {
    let words: Option<Vec<String>> = args.iter().map(|a| a.to_str().map(str::to_string)).collect();
    if let (Some(lead), Some(words)) = (Lead::from_env(), words)
        && let Some(answer) = answer(&lead, &words)
    {
        return match answer {
            Ok(out) => {
                print!("{out}");
                0
            }
            Err(e) => {
                eprintln!("kraal: {e:#}");
                1
            }
        };
    }
    real_tmux(args)
}

/// The tmux installed after this one in PATH, outside the team's pretend tmux session.
fn real_tmux(args: Vec<OsString>) -> i32 {
    let me = std::env::current_exe().ok();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let found = std::env::split_paths(&path)
        .map(|d| d.join("tmux"))
        .find(|t| t.is_file() && std::fs::canonicalize(t).ok() != me);
    let Some(tmux) = found else {
        eprintln!("kraal: tmux is not installed (kraal answers only Claude's agent teams as tmux)");
        return 127;
    };
    let err = std::process::Command::new(tmux).args(args).env_remove("TMUX").env_remove("TMUX_PANE").exec();
    eprintln!("kraal: running tmux: {err}");
    126
}

/// What `tmux WORDS` asks of kraal; `None` when it is for a real tmux server (another socket, a
/// target kraal did not give, a command kraal does not answer).
fn answer(lead: &Lead, words: &[String]) -> Option<Result<String>> {
    let (global, words) = options(words, "cfLST")?;
    if global.iter().any(|(o, _)| *o == 'V') {
        return Some(Ok("tmux 3.4 (kraal)\n".into()));
    }
    if global.iter().any(|(o, v)| matches!(o, 'L' | 'f' | 'c') || *o == 'S' && v.as_deref() != Some(&lead.socket)) {
        return None;
    }
    let (command, rest) = words.split_first()?;
    let values = match command.as_str() {
        "display-message" | "display" => "cdt",
        "list-panes" | "lsp" => "Fft",
        "split-window" | "splitw" => "celptF",
        "respawn-pane" | "respawnp" => "cet",
        "kill-pane" | "killp" => "t",
        "select-pane" | "selectp" => "Tt",
        "set-option" | "set" | "set-window-option" | "setw" | "select-layout" | "selectl" => "t",
        "resize-pane" | "resizep" => "txy",
        _ => return None,
    };
    let (opts, args) = options(rest, values)?;
    let value = |o: char| opts.iter().rev().find(|(k, _)| *k == o).and_then(|(_, v)| v.clone());
    let has = |o: char| opts.iter().any(|(k, _)| *k == o);
    let pane = match value('t') {
        None => lead.pane.clone(),
        Some(t) => target(&t, lead)?,
    };
    let refuse = |allowed: &str| match opts.iter().find(|(o, _)| !allowed.contains(*o)) {
        Some((o, _)) => Err(anyhow::anyhow!("kraal does not support {command} -{o}")),
        None => Ok(()),
    };
    Some((|| match command.as_str() {
        "display-message" | "display" => {
            Ok(if has('p') { format!("{}\n", expand(&args.join(" "), &pane, lead)) } else { String::new() })
        }
        "list-panes" | "lsp" => {
            let format = value('F').unwrap_or_else(|| "#{pane_id}".into());
            Ok(lead.team()?.iter().map(|p| format!("{}\n", expand(&format, p, lead))).collect())
        }
        "split-window" | "splitw" => {
            refuse("dhvlptcPF")?;
            let opened = lead.request(Request::NewPane {
                target: Some(pane),
                command: shell_command(args),
                cwd: value('c').map(PathBuf::from).or_else(|| std::env::current_dir().ok()),
                name: None,
                placement: if has('h') { Placement::Right } else { Placement::Down },
                preview: None,
                focus: !has('d'),
                close_on_exit: true,
            })?;
            let Response::Pane(info) = opened else { bail!("unexpected answer from kraal") };
            lead.joined(info.id)?;
            let format = value('F').unwrap_or_else(|| "#{pane_id}".into());
            Ok(if has('P') { format!("{}\n", expand(&format, &info.id.to_string(), lead)) } else { String::new() })
        }
        "respawn-pane" | "respawnp" => {
            // shortcut: without -k as with it, the program running is replaced; tmux refuses then.
            refuse("kt")?;
            lead.request(Request::RespawnPane { pane, command: shell_command(args) })?;
            Ok(String::new())
        }
        "kill-pane" | "killp" => {
            refuse("t")?;
            lead.request(Request::ClosePane { pane })?;
            Ok(String::new())
        }
        "select-pane" | "selectp" => {
            refuse("tT")?;
            match value('T') {
                Some(name) => lead.request(Request::RenamePane { pane, name })?,
                None => lead.request(Request::FocusPane { pane })?,
            };
            Ok(String::new())
        }
        // Colors, borders, layout and sizes: kraal draws its panes its own way.
        _ => Ok(String::new()),
    })())
}

/// The pane a tmux target names when kraal gave it: `%ID` a pane, `@ID` the lead's team.
fn target(t: &str, lead: &Lead) -> Option<String> {
    if let Some(id) = t.strip_prefix('%').filter(|id| id.parse::<u32>().is_ok()) {
        return Some(id.to_string());
    }
    t.strip_prefix('@').filter(|id| *id == lead.pane).map(str::to_string)
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

/// `format` with the tmux variables Claude reads; the others are empty.
fn expand(format: &str, pane: &str, lead: &Lead) -> String {
    let mut out = String::new();
    let mut rest = format;
    while let Some(start) = rest.find("#{") {
        out.push_str(&rest[..start]);
        let Some(len) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        out.push_str(&match &rest[start + 2..start + len] {
            "pane_id" => format!("%{pane}"),
            "window_id" => format!("@{}", lead.pane),
            "session_name" => lead.session.clone(),
            _ => String::new(),
        });
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lead() -> Lead {
        Lead { socket: "/run/k/main".into(), pane: "4".into(), session: "main".into(), team: PathBuf::new() }
    }

    fn words(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn claude_learns_its_pane_and_team_window() {
        let l = lead();
        let ask = |s: &str| answer(&l, &words(s)).map(|r| r.unwrap());
        assert_eq!(ask("-V").as_deref(), Some("tmux 3.4 (kraal)\n"));
        assert_eq!(ask("-S /run/k/main display-message -p #{pane_id}").as_deref(), Some("%4\n"));
        assert_eq!(ask("display-message -t %4 -p #{window_id}").as_deref(), Some("@4\n"));
        assert_eq!(ask("-S /run/k/main set-option -p -t %9 pane-border-style fg=red").as_deref(), Some(""));
        assert_eq!(ask("select-layout -t @4 main-vertical").as_deref(), Some(""));
    }

    #[test]
    fn other_tmux_uses_go_to_the_real_one() {
        let l = lead();
        for call in [
            "new-session -d -s build make",
            "send-keys -t build make Enter",
            "-L claude-swarm-12 list-panes -t claude-swarm:swarm-view",
            "-S /tmp/other display-message -p #{pane_id}",
            "kill-pane -t build:0.1",
            "list-panes -t @7",
            "split-window -t",
        ] {
            assert!(answer(&l, &words(call)).is_none(), "{call}");
        }
    }

    #[test]
    fn unsupported_options_are_refused_not_ignored() {
        let l = lead();
        let e = answer(&l, &words("split-window -b -t %4 -- cat")).unwrap().unwrap_err();
        assert_eq!(e.to_string(), "kraal does not support split-window -b");
        assert!(answer(&l, &words("kill-pane -a -t %4")).unwrap().is_err());
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
    fn formats_keep_text_and_drop_unknown_variables() {
        let l = lead();
        assert_eq!(expand("#{session_name}:#{window_id}.#{pane_id} #{pane_tty}x", "7", &l), "main:@4.%7 x");
        assert_eq!(expand("a #{pane_id", "7", &l), "a #{pane_id");
    }

    #[test]
    fn teams_args_and_env() {
        assert_eq!(lead_args(words("-c")), words("--teammate-mode auto -c"));
        assert_eq!(lead_args(words("--teammate-mode=in-process")), words("--teammate-mode=in-process"));
        let env = lead_env(Path::new("/run/k/main"));
        assert!(env.contains(&("TMUX".into(), "/run/k/main,0,0".into())));
        let path = &env.iter().find(|(k, _)| k == "PATH").unwrap().1;
        assert!(path.starts_with(&shim_dir().display().to_string()));
        let mut to = vec![("TMUX".to_string(), "kept".to_string())];
        carry_env(&env, &mut to);
        assert_eq!(to.len(), 3);
        assert_eq!(to[0].1, "kept");
    }
}
