# Scripting, events and hooks

Scripts and agents drive kraal from inside its panes, and kraal tells them what happens.

## Driving kraal

Every pane gets `KRAAL_SOCKET`, `KRAAL_SESSION` and `KRAAL_PANE` (and `KRAAL=1`), so scripts and
agents can drive kraal from inside: commands act on the session they run in (`kraal kill` too),
and on their own pane when none is named, unless `-s` names another session. From outside a pane
they act on the session `main`, or the one `-s` or `$KRAAL_SESSION_NAME` names.

| Command | |
|---|---|
| `kraal run --right --name tests -- cargo test` | runs a command in a new pane and prints the pane's id |
| `kraal agent claude --tab` | starts an agent in a new pane and prints the pane's id ([Accounts](agents.md#accounts)) |
| `kraal send -p tests --enter "cargo test"` | types into a pane (`--enter` presses Enter after it) |
| `kraal keys -p tests ctrl+c` | presses keys: `enter`, `up`, `ctrl+c`… |
| `kraal read -p tests --lines 50` | prints a pane's screen, or with `--lines` its last lines, scrollback included |
| `kraal wait -p api --until blocked,done --timeout 600` | waits for a pane, then prints its agent's state |
| `kraal panes` | lists the panes: id, workspace, tab, name, directory or exit status (`--json` says everything) |
| `kraal focus tests` | shows a pane in the attached terminal |
| `kraal rename -p tests unit` | renames a pane |
| `kraal close tests` | closes a pane |
| `kraal agents --json` | every agent and its state, servers' too |
| `kraal agents --follow` | this session's agents, as one JSON line, again at each change |
| `kraal preview docs/design.md --floating` | shows a file or a directory in a new pane |
| `kraal notify "deploy" "finished"` | a desktop notification through the attached terminal |

Panes are referred to by id, by name, or as `workspace:name`. Most commands take theirs with
`--pane` (`-p`), `kraal close` and `kraal focus` as their argument; outside a pane, one that is
given none says which it takes.

`kraal run` and `kraal preview` put the new pane where you say: `--right` or `--down` split a
pane (the one `--target` names, else the one the command runs in, else the focused one),
`--floating` floats it, `--tab` gives it a tab; without any of them, kraal splits the pane along
its longer side. `--name` names the new pane and `--no-focus` leaves the focus where it is.
`kraal run --cwd ~/src/api` starts the command in that directory, and `--close-on-exit` closes
the pane when the command exits, where it would otherwise stay open with its exit status.

For `kraal wait`, `--until` takes the states of an agent (`working`, `blocked`, `done`, `idle`),
`exit` (the pane's program ended) and `quiet` (it printed nothing for two seconds); without it,
the wait ends on `blocked`, `done`, `idle` or `exit`. It fails when `--timeout` (in seconds)
passes first, or when the pane closes. Disconnecting the waiting command removes its wait;
it does not close the pane. The event stream likewise ends when its subscriber disconnects.

Commands that prepare work in the background keep their request open until that operation
answers: for example, a remote edit waits for its file and editor, and a runbook waits for
its plan or for its recorded run to start. If a helper worker cannot start or panics, the
command receives an error and its request closes. This also applies to the Git lookup before
opening a pane or an agent. Disconnecting the command does not cancel those
operations. `kraal kill` answers after the final save attempt and reports a save failure.

Commands require the same protocol version as the running session. After an update that changes
the protocol, a command fails with both versions and the restart or update command to run before
retrying. `kraal kill` can still save and stop a 0.2.0 or 0.3.0 session.
Restarting it is refused before stopping it, with `run kraal 0.4 once first`
([Compatibility](compatibility.md)).

`kraal agents` also lists the agents of the kraal sessions on your servers that kraal watches (see
[Agents on your servers](servers.md#agents-on-your-servers)), after this session's own, as
`gpu › api` in the workspace column. In `--json`, such an agent has `"kind": "remote"`, `server`
(the server's name) and `session` (the kraal session there), and its `id` is its pane's id in that
session, not a pane here: `kraal -s main read -p 3` runs on the server, `kraal attach --host gpu
--pane 3` shows it here. `kraal agents --follow` lists this session's own agents only: it is what
another kraal watching this session reads.

## Paste buffers

What kraal copies is kept in paste buffers ([Keys](keys.md#paste-buffers)), and scripts and agents
reach them too:

| Command | |
|---|---|
| `kraal buffer list` | newest first: number or name, size, age, first line (`kraal buffer list --json` for scripts) |
| `kraal buffer show` | prints the newest buffer's text, as it is |
| `kraal buffer show 2` | the third newest numbered one (0 is the newest) |
| `kraal buffer set review` | keeps what it reads on its standard input (a pipe, a file) as the buffer `review` |
| `kraal buffer set notes --from-file notes.md` | or the text of a file |
| `kraal buffer set` | without a name: a numbered buffer, like a copy |
| `kraal buffer paste review -p api` | pastes into a pane, as your terminal would |
| `kraal buffer delete review` | deletes one buffer (the newest without a name or number) |
| `kraal buffer delete --all` | deletes them all |

Numbered buffers rotate: 0 is always the newest, and the oldest go past `[buffers] limit` or
`max_mb`. Named buffers stay until deleted; a name is one word of at most 64 characters (letters,
digits, `- _ . : @ +`), never a number. Setting a name again replaces that buffer. `kraal buffer paste` sends the text as a
paste, bracketed when the program asked for it; like `kraal send`, it refuses a production pane
unless given `--production` ([Production](production.md)). A paste goes whole or not at all: one
larger than the program can be sent at once (16 MiB, less what it has not read yet) is refused.

Buffers hold text, in the server's memory only unless `[buffers] persist = true`
([Configuration](configuration.md)).

## Events

| Command | |
|---|---|
| `kraal events` | everything that happens, as JSON lines, until you stop it |
| `kraal events 'agent.*' pane.failed` | some types only (quoted, or your shell reads the `*`) |
| `kraal events --list` | the types |
| `kraal events --since 1200` | first what came after event 1200, if still kept |
| `kraal events --since 1200 --boot 1760184000000` | the same, with the `boot` read along with that `seq`: if the server restarted since, everything it kept comes |

```
{"v":1,"seq":1201,"boot":1760184000000,"type":"agent.blocked","pane":12,"name":"api","workspace":"dev","agent":"claude",…}
{"v":1,"seq":1202,"boot":1760184000000,"type":"pane.failed","pane":7,"name":"tests","code":1,"command":["cargo","test"],…}
{"v":1,"seq":1203,"boot":1760184000000,"type":"ssh.started","pane":3,"server":"web-prod-1","host":"deploy@web-prod-1","production":true,…}
```

Types: `workspace.opened/closed`, `pane.opened/exited/failed/closed`, `agent.working/blocked/done/
idle`, `ssh.started/ended` (an `ssh` process appeared or ended in a pane, whether or not it got
connected), `production.entered/left` (`entered` again when the reason changes),
`production.approved/refused` (an agent's production action, allowed or refused; see
[Agents](production.md#agents)),
`notification`, `tunnel.up/down`, `port.opened/closed`, `account.limit`, `account.changed` (an
agent's pane moved to another account, or back with Undo: `account`, `from`, `undo`),
`runbook.started/finished`, `command.finished` (a command the shell marks ended: its `command`
line, `exit` status and `seconds`; see [Shell commands](terminal.md#shell-commands)).

Duplicating or restoring a closed workspace emits no `workspace.opened` event and runs no
matching hooks. A successful Open emits it once; selecting an already open workspace does not.

Local agent events and pane JSON include an optional `account_id` alongside the readable
`account` label. The identity stays the same across a rename and is kept after removal; a new
account that reuses the name gets a different identity. Older sessions may omit this field.

An agent of a server's session watched here changes state as a local one does: `agent.working`,
`agent.blocked`, `agent.done` and `agent.idle` come for it too, with `server` and `session` (and
`pane`, its id in that session; `previous` is null the first time it is seen). An `agent.*` hook
that acts on the pane checks `$KRAAL_SERVER` first: it is empty for an agent of this session.

```
{"v":1,"seq":1204,"boot":1760184000000,"type":"agent.blocked","pane":3,"name":"api","workspace":"dev","agent":"claude","previous":"working","server":"gpu","session":"main",…}
```

`seq` grows with each event and `boot` changes when the server restarts. The server keeps the last
1024 events: a script that restarted can come back with `--since` its last `seq`; an `events.lost`
line says when some are gone.

kraal never waits for a reader, and a reader may take the time it needs for each event. It is
dropped when it falls too far behind: once 64 events wait for it in kraal, after those the system
holds for it in its pipe and its connection (a few hundred on Linux, fewer on macOS). It still
reads every event it was sent, then a last line, and `kraal events` fails (status 1) with the
reason on its standard error:

```
{"v":1,"type":"events.dropped","boot":1760184000000,"since":1262,"reason":"too slow"}
```

`kraal events --since 1262 --boot 1760184000000` goes on from there, and misses nothing while the
server still has the events. A stream that ends without that line, with status 0, ended with the
session: kraal stopped or restarted. Closing its last pane or workspace keeps the session and
its event streams running.
[notify-blocked.sh](../examples/scripts/notify-blocked.sh) is a script on this stream that goes on
where it stopped.

`[hooks]` runs a command when one happens, with the event in `$KRAAL_EVENT_JSON` and its fields as
`$KRAAL_<FIELD>`; output goes to `~/.local/state/kraal/hooks.log`:

```toml config
[hooks]
"agent.blocked" = 'curl -s -d "$KRAAL_AGENT needs you in $KRAAL_NAME" ntfy.sh/my-topic'
"pane.failed" = 'notify-send "$KRAAL_NAME failed ($KRAAL_CODE)"'
"ssh.started" = 'logger -t kraal "ssh $KRAAL_HOST production=$KRAAL_PRODUCTION"'
```

A key is an event type, or all those of a kind (`"agent.*"`); `kraal config check` names one that
is no event. The command runs through `sh -c`. `$KRAAL_EVENT` is the type; a field that is true
is `1`, and one that is false or empty is the empty string. A hook can call kraal: it runs with
the session's `KRAAL_SOCKET`, so `kraal read -p "$KRAAL_PANE"` reads the pane the event is about.
`$KRAAL_SESSION` is the session's name, unless the event has a `session` field of its own (an
agent on a server names the session it runs in there): the event's field wins. `KRAAL_SOCKET`,
`KRAAL_EVENT_JSON` and `KRAAL_HOOK` are always kraal's, whatever the event's fields.
[hooks.toml](../examples/config/hooks.toml) is this table as a file to copy.

At most 20 hooks start per second and 32 run at once. A hook counts as running while any process
in its group is still alive, even after its shell exits. Kraal keeps the shell's process id
reserved until supervision ends, so a later stop cannot target an unrelated group that reused
that id. After 10 minutes, or when the session stops, kraal kills the hook's process group.
Processes that leave that group are outside this supervision.
64 KiB of each hook's output are kept, in a log only you can read,
renamed `hooks.log.1` once past 1 MB. Hooks and scripts reading the stream run as you, like any
script: kraal does not sandbox them.
