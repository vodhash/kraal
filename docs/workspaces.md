# Sessions and workspaces

Workspaces are saved automatically. KDL files import and export reusable layouts.
Kraal keeps the organization and launch information it knows, not unsaved editor buffers,
process memory or every command typed in a shell.

## Everyday use

Open **Workspaces** with `ctrl+b w`. Search filters; **+ Add workspace** creates.
Select a workspace and press Tab, or use its sidebar actions, for the operations below.

| Task | Path and result |
|---|---|
| Create | **+ Add workspace**, edit **Name**, **Add**. A shell opens in the displayed folder. Success is confirmed after saving. |
| Rename | **Rename**, edit **Name**, confirm. Open and closed workspaces keep their identity and source file. |
| Close and return | **Close** stops programs and keeps the layout as **Closed**. Enter on its row reopens it under the [resume rules](#what-resumes). |
| Delete and recover | **Delete**, confirm, then **Undo** in the result or **Deleted workspaces**, **Restore** later. The restored workspace is closed; project and source files stay intact. |
| Copy the organization | **Duplicate**, confirm the new name. The independent copy is closed; commands wait for Enter when opened, and agents start new conversations. |
| Import | **+ Add workspace**, **From file...**, enter **File**, review **Preview**, then **Add**. Commands wait for Enter. |
| Share | **Export layout**, choose **File**, **Export**. Existing files need explicit replacement; a backup is kept. |

A form error keeps the entered values. Correct the reported cause and use its retry action.
Text fields scroll horizontally around the cursor, counting wide characters in terminal columns.
When a result selects a workspace hidden by the current search, the list clears that search
to reveal the workspace. A result whose row is already visible keeps the search.
Closing an individual pane removes it from the layout; use **Close** on the workspace to keep
all its panes for reopening. The last pane's removal leaves an empty, open workspace with
**+ Add pane**, rather than deleting it. Quit (`ctrl+b q`) stops the session.

## Concepts

The **Workspaces** list (`ctrl+b w`) shows **Open** or **Closed**, retained tab and pane counts,
and **Current** for the workspace on screen. Enter or one click opens the selected workspace.
Search only filters the list. **+ Add workspace** stays visible even when nothing matches.
The bottom-right count includes only workspaces, showing matches over the total while searching.
Tab or a right click offers actions for that row: **Rename** and **Duplicate** for a saved workspace, and **Close**
when it is open. **Delete** is available for saved workspaces and workspace files that have
not been imported. **Move up** and **Move down** change the order of saved workspaces and
unimported files. An unimported file also offers **Open**.

Workspace actions keep the same order for open and closed rows: Open, Rename, Duplicate,
Move up/down, Export layout, source actions when available, then Close and Delete.
Close appears only while open. Sidebar status omits trailing parts that do not fit, so
**Open · 11 tabs** stays readable when the pane count would be cut off.

**+ Add workspace** and `ctrl+b W n` open the same **Add workspace** form. It proposes an unused
name (or the search text) and shows the folder inherited from the current pane. Edit **Name**,
then Enter or click **Add**. **Add**, **From file...** and **Cancel** share one row when they fit.
Tab moves between the field and buttons. Esc or **Cancel** returns to the list with its search
and selection when the form was opened from that list. From the sidebar
(**+ Add workspace** or a row's actions), command palette or a direct shortcut, it closes to
the panes. Add, Rename, Duplicate, From file and Export layout use this same return path,
including **Close** on a result.
Add, Rename, Duplicate, Restore and import use the same name rules, including from the command
line: names cannot be empty, contain control characters (such as tabs or newlines), or be taken
by another workspace. Spaces, Unicode, `/` and leading dots are allowed. Names are checked
against the whole session, including closed workspaces, workspaces with restore errors and
files not yet imported. Rename may keep its current name; import may use its own file's name.
An invalid or taken name keeps **Add** (or **Rename**) as the action: correct the name and submit
again. **Starts in** stays visible alongside an Add error.

An immediate shell or save failure keeps the form and its input, with **Retry** and **Cancel**.
No workspace is added on that failure. **Workspace added** appears only after its state is saved.
Renaming also waits for the saved name: if saving fails, the old name stays and the form keeps
the proposed name for retry. A rename from the list selects the renamed row; if its new name
no longer matches the search, the search clears to show it.

If saving a reversible workspace change fails, kraal restores the previous local state and
saves it again. A writer that timed out may still finish, so this second save must follow it.
If that rollback cannot be confirmed, the error explains the uncertainty about the saved
state and asks you to fix the save error and retry. This does not restart stopped programs
or recover unsaved program work.

- A **session** is one background server. It owns the processes; clients attach and detach.
  `kraal` uses the session `main`; `kraal -s work` (or `KRAAL_SESSION_NAME=work` in the
  environment) another one, and inside a pane every command uses that pane's session.
- A session holds **workspaces**, each with **tabs** (named after their focused pane until you
  rename them), each with **panes**:
  tiled (some may share a place as a stack, one open at a time), or floating on top.
- The **sidebar**, at the left from the first screen on, lists open and closed workspaces and
  unimported workspace files in the same order as Workspaces. Then come the agents, tunnels
  and ports, with the agents' accounts at the bottom once one is listed or used.
  The full sidebar appears from three times its configured width, 84 columns by default.
  `ctrl+b V l` folds it to a **four-column strip**, separator included, or expands it again.
  The choice belongs to this attached terminal until it detaches. `[sidebar] visible = false`
  starts folded, with the strip still available. There is no setting to hide it entirely.
  With the default width, the strip appears automatically at 40 to 83 columns; below 40 it disappears.
  Growing the terminal restores the full sidebar if it was requested; a manual fold stays folded.
  In the strip, the top bar names the current workspace. Click that name to open **Workspaces**.
  With the full sidebar visible, its **WORKSPACES** heading opens the same list, **+ Add workspace**
  opens the form, and a row's actions button or a right click opens that workspace's actions.
  Closed workspaces use the same empty dot as workspace files that have not been opened;
  open workspaces use a filled dot (the ASCII theme uses `o` and `*`).
  The workspace dot takes the colour of its most urgent agent: blocked, done, working, idle,
  then unknown. Without an agent it is dim.
- The strip has **WS** then **AG** sections, with no empty space reserved between them.
  WS opens Workspaces; AG opens the pane picker. Each row has its original list number and
  state icon. The current workspace has the selection background and a bold number.
  Numbers are positions, not identifiers or shortcuts: `ctrl+b 1` to `9` still select tabs.
  Click a workspace to open or select it, right click for its actions, or click an agent to
  reveal its pane. These clicks keep the strip. Reordering is in Workspaces or the full sidebar.
  When rows run out, `⋮` (`...` in ASCII) opens the corresponding list. The current workspace
  and blocked then done agents take priority. Numbers above 99 are reached through that list,
  never shortened to ambiguous digits. If that includes the current workspace, its list row
  keeps the selection background. The bottom `▸` (`>` in ASCII) runs `ctrl+b V l`:
  when the full sidebar cannot fit, it opens Workspaces without changing the folding preference.
  Accounts (`ctrl+b A l`), Tunnels (`ctrl+b S o`) and ports remain in the full sidebar and their
  existing views; accounts and ports have no miniature rows in the strip.
- Several terminals can be attached at once. The panes have the size that fits the one used last
  (attached, resized or typed in), and each terminal shows them beside its own sidebar: a smaller
  one shows what fits.
- A pane is a terminal (a shell, or a command such as `ssh host`), or a **preview** of a file:
  rendered Markdown, an image, text, or a directory listing. Previews reload when the file changes.
- A **command pane** stays open when its program exits, showing the exit code: press Enter to run
  it again (handy when an SSH connection drops) or `x` to close it.
- An account's **login pane** closes after successful sign-in is verified. After failure or
  cancellation, Enter closes it; only the labelled `r` action tries the login again.

| Command | |
|---|---|
| `kraal` | attaches to the session, starting it if needed |
| `kraal shop` (or `kraal attach shop`) | the same, on the workspace `shop`, reopened from its saved layout, or imported from its file the first time |
| `kraal attach files` | a workspace named like a command of kraal: `kraal files` is the command, and `kraal config check` names such a file |
| `kraal ls` | lists the sessions: running, with the kraal each one runs, or saved |
| `kraal kill` | stops the session's server and its programs; the layout is saved |
| `kraal restart` | restarts the server on the kraal installed now (see below) |
| `kraal forget` | deletes what a stopped session saved |

### Duplicating a workspace

Select an open or closed saved workspace, press Tab and choose **Duplicate**. The shared Name
form proposes an unused name: `api-copy`, then `api-copy-2`, and so on. Edit it if needed and
press Enter or click **Duplicate**. Esc or **Cancel** returns to the same search and selection
from the list, or to the panes from sidebar actions.
An invalid or taken name stays in the form so you can correct it.

The result selects the new **Closed** workspace and says **Workspace duplicated · Enter Open**.
It is saved before that result appears. Opening it is optional; the workspace currently shown
in each attached terminal stays unchanged. `workspace.opened` hooks run only when the copy is
opened. A save failure keeps the name and offers **Retry**.
The failed copy is removed; if a timed-out write's rollback also fails, the form says the saved
session may still contain it and asks you to fix the save error and retry.

The copy keeps the layout, names, pane folders, commands, previews and account bindings.
It has its own workspace identity and gets new tab and pane numbers when opened. Shells can
start on Open. All commands, including SSH and managed agents, wait for Enter. Managed agents
start a new conversation; conversation selections and account-switch Undo history are removed.
Scrollback and shell history shown in panes are not copied. Typing is not broadcast.

The source stays unchanged. The copy has no association with its KDL file and writes no KDL
file. Project files and pane folders remain shared; Duplicate does not copy a Git checkout or
create a worktree. A workspace restored from Deleted workspaces before its first Open can also
be duplicated from its retained contents, even if the original file is gone.

### Reordering workspaces

Select a row in **Workspaces**, press Tab and choose **Move up** or **Move down**.
You can also drag a workspace row onto another row in the list or full sidebar. Moving upward
places it before that row; moving downward places it after that row. The destination is
highlighted while dragging. A click opens on release; dragging starts no programs and does
not switch workspaces. Release outside workspace rows or press Esc to cancel.
The row stays selected and the workspace, tab and pane on each attached terminal stay focused.
The sidebar, Workspaces and `kraal workspaces` share the order. It is saved automatically,
including while the server is stopped and after restart. Opening or closing a workspace keeps
its place. Moving an unimported file starts no programs and changes no KDL file.
The first row has no Move up action, and the last has no Move down action.

While searching, **Clear search to reorder** replaces the move actions. Choose it to clear the
search and keep the selected row, then move or drag in the full list. Dragging a filtered
list says **Clear search to reorder** and keeps the order. If another client changes the
order during a drag, kraal asks you to drag again against the current list.
If saving fails, kraal keeps the previous order and offers **Retry** and **Cancel**.
If a timed-out write's rollback also fails, the dialog says the saved order may differ and asks
you to fix the save error and retry. Cancel returns to the list with the same row selected.

### One server for what a session saves

A session's server holds the session where it saves it (`~/.local/state/kraal`), and listens in
the runtime directory of the shell that started it (`$XDG_RUNTIME_DIR/kraal`, or
`/tmp/kraal-<uid>/kraal` without one). A kraal that looks in another runtime directory (a cron job
or a `sudo -u` shell without `$XDG_RUNTIME_DIR`, beside a desktop terminal that has it) does not
find that server. It does not start a second one on the same files, nor forget them: after a
few seconds spent looking for a server of its own, it stops and says where the session runs.

```
$ kraal
kraal: session main is not started here: kraal 4242 runs it with its socket in "/run/user/1000/kraal", not in "/tmp/kraal-1000/kraal" where this kraal looks: reach it with XDG_RUNTIME_DIR="/run/user/1000"
```

Set `XDG_RUNTIME_DIR` as it says to work with that session, or name another one (`kraal -s NAME`).
When `$XDG_RUNTIME_DIR` is set and kraal cannot keep its sockets in it (the path is too long for
one, or another user could replace it), kraal says so when it starts a server, in that server's
log, and in `kraal config check`.

The same holds between machines that share a home directory, as far as their file system locks a
file for all of them: the second machine is told the session runs on the first. A network file
system mounted with local locks only keeps each machine's kraal from its own, not from the
others': name the session differently on each machine there (`kraal -s NAME`), or keep kraal's
state on a local disk (`XDG_STATE_HOME`). Where the state directory cannot lock a file at all,
kraal starts no session and forgets none, and says so: it would have no way to keep a second
server from the same files.

## Editing a pane

Open **Edit pane** with `ctrl+b P e`, or find it in the command palette. It edits the focused
terminal pane's **Name**, **Folder** and **Command** without a KDL file. Previews, file managers
and sign-in panes offer their name only. An empty Name restores the pane's automatic name.

Folder initially shows the pane's current local directory. It accepts an absolute path,
`~`, or a path relative to that displayed directory. For an SSH pane this is the local launch
directory, not a remote directory. Command initially shows the saved launch, not a command
guessed from shell history. A changed Command uses `/bin/sh` syntax, including quoting and
pipes; an empty Command starts your configured shell. Unchanged commands retain their original
arguments and launch settings.

Enter or **Apply** submits the form; Tab and Shift+Tab move between fields and buttons. Fields
and buttons also accept clicks. In the Panes list, **Edit pane** (`ctrl+e`) edits the selected pane.
Esc or **Cancel** leaves the pane unchanged and returns to that list with its search and selection,
or to the panes when opened directly. An invalid folder
or command syntax keeps all the entered values with a correction to make. Long errors wrap and
scroll with PageUp/PageDown or the mouse wheel at both 80x24 and 124x36.

Changing only Name does not restart anything. A command or folder change on a stopped pane
keeps it stopped, showing **Ready to run**; Enter runs it explicitly. For a running pane,
**Apply command?** lists what will stop, including background jobs, and shows the replacement
command and folder. **Cancel** is selected first, so Enter returns to the entered fields.
**Stop and apply** saves the requested launch before stopping the old programs, waits for their
exit and final output, then starts the replacement in the same pane with its scrollback.
Another client's changes cannot be overwritten by an old form; changed activity needs a fresh
confirmation.

For a managed agent, changing only Folder uses the existing conversation resume checks. An
unverifiable conversation or ambiguous command is refused before stopping. Changing Command
explicitly creates a plain command without the managed account or conversation; the form says
so before applying. Account changes and pane edits cannot run concurrently in the same pane.

If saving fails, nothing stops and the form keeps its values to retry. If the replacement cannot
start, the pane keeps the requested launch and its output: correct it in Edit pane, or fix the
reported problem and press Enter to retry. An interruption during the stop restores the edited
command waiting for Enter. The changes are part of the automatically kept workspace and never
write its source KDL file. A later session restart keeps ordinary commands stopped as usual.

## Persistence

Sessions keep running when you close the terminal, lose the connection or detach (`ctrl+b d`):
`kraal` attaches again. After a restart of the server, a quit or a reboot, the processes are gone,
but kraal rebuilds the saved layout under the rules below. Closed workspaces remain closed.
A workspace that cannot be brought back (its shell gone, say) stays saved as it was. Fix the
reported cause and restart to retry. Other workspaces still restore; panes created for a
workspace whose restoration fails are removed.

Automatic saves run in the background as the layout changes, with a periodic check every
30 seconds. Scrollback is saved separately when enabled (`save_scrollback`, on by default).
An abrupt crash can lose changes or output that have not reached disk. Save failures appear
in **Messages** (`ctrl+b V j`) and are retried; automatic keeping is not a backup of project files.
Close, Delete and Quit wait for a durable save before stopping programs.

### What resumes

These rules apply when reopening a closed workspace or restoring an open one after a server
restart. Detaching leaves the original processes running and needs no restoration.

| Saved pane | On reopening or restart |
|---|---|
| Local shell | A new shell starts in its saved directory. Commands previously typed into it are not replayed. A recognized Claude or Codex conversation in that shell may receive a resume command. |
| Interactive SSH command, without a remote command | Reconnects. Remote jobs and terminal state are not restored by kraal. |
| Claude or Codex | A known conversation can resume with supported launch arguments. The agent must still have its saved history and sign-in. See [agent resume limits](agents.md#resume-limits). |
| Other command, including an SSH remote command | Waits for Enter. Check the displayed command before running it. |
| Pane already stopped or suspended | Stays stopped, waiting for Enter. |
| Preview | Reopens its file if available. |
| Laptop file panel | Reopens at the session machine's home directory. Laptop paths and listings are not saved. |

Restored scrollback is terminal output, not a running program or an agent transcript. A turn
interrupted by stopping an agent is not continued in memory. Login panes are never restored.

`ctrl+b q` (or `ctrl+b W q`; `Q` in the classic preset) quits: it lists the programs that would
stop, as a restart does, and asks, Cancel selected. Then the server saves the session and stops
with every program in it, as `kraal kill` does, and each attached terminal ends with
`[kraal: stopped; kraal starts it again with its layout]`. A session that cannot be saved is not
stopped: it keeps running, and the quit says why (`kraal kill` stops it all the same).

### Closing a workspace

Closing a workspace (`ctrl+b W x`, or `ctrl+x` on an open workspace in the picker) keeps its current layout
and stops its programs. The row stays **Closed**, including when it was the last open workspace.
Enter in the picker, clicking its sidebar row, or `kraal open <name>` reopens that saved layout.
Tabs, splits, floating panes, pane directories, commands, known conversation references and
available saved scrollback are kept. Reopening follows the [resume rules](#what-resumes): new shells,
interactive SSH reconnections, supported Claude or Codex resumes, and waiting commands.
The imported KDL file is not rewritten or reread by Close or Open; editing, removing or breaking that file does not replace
the saved layout. Export also works while the workspace is closed, without starting its programs.

Close lists the running programs, background jobs, agents and SSH connections before stopping them.
The question is titled **Close &lt;name&gt;?**. Programs are named as they appear in the terminal;
a tab name is added only to distinguish repeated program descriptions. It says **The layout will
be kept.** and **Unsaved work in these programs may be lost.**
**Cancel** is selected first; Enter cancels, and **Stop and close** confirms.
Other workspaces keep running, and clients showing the closed workspace move
to another open workspace, or to Workspaces if none remain. Shells waiting at their prompt need no
extra confirmation; unknown activity still asks. If another client changes
the programs while the question is open, the updated inventory needs confirmation. Before signalling any
program, kraal writes the layout, scrollback and close intent durably. If that preparation fails,
the dialog says **Nothing was stopped** and offers **Retry**. If programs cannot all be stopped,
the workspace stays open and Retry continues the close. If saving the final state fails after
stopping, the dialog says **Programs stopped. Couldn't save the final state.** The prepared
layout remains recoverable; Retry saves the final state. After a crash during Close, the next
server finishes the recorded close without reopening those programs. Recovery checks the machine,
boot and process start times before signalling surviving programs.

Closing an individual pane or tab removes it from the layout. When the last pane ends, including
`exit` in the last shell, the workspace stays empty, open and saved. **+ Add pane** adds a shell with
Enter or a click. The empty view shows **No panes.** and the selected **+ Add pane** action.
The session and attached terminals stay open, and an empty workspace stays
empty after a restart. Closing the last open workspace shows Workspaces. Use Quit (`ctrl+b q`)
or `kraal kill` to stop the session explicitly. A command pane that shows its exit code remains
in the layout until it is closed.

### Deleting and restoring a workspace

In **Workspaces** (`ctrl+b w`), select a workspace, press Tab and choose **Delete**.
The same action is available from a sidebar row's actions. There is no direct Delete shortcut.
**Cancel** is selected first. For a running workspace, one **Stop and delete** confirmation
lists its programs and background jobs and explains that their unsaved work cannot be restored.
Project files, Git branches and external KDL files are kept.

After the deletion is saved, the workspace leaves the main list. Click **Undo** in the toast,
or press **Ctrl+Z** while the deletion result is in Workspaces. Later, including after a restart,
open **Deleted workspaces** from Workspaces and press Enter or click a row to **Restore** it.
Deleted workspaces have no automatic expiry. Restore keeps the identity, layout, source contents
and available scrollback, and puts the workspace back at its previous position where possible.
It is **Closed**: restoring starts no programs. Enter on the restored row opens it explicitly.
If its name has been reused, **Restore workspace** proposes a free name such as `api-restored`;
the newer workspace and the deleted contents are both kept.

Deleting a workspace that came from a scanned KDL file also excludes that source from this
session's scans and from `kraal workspaces`, including while the server is stopped. Other sessions
keep their own lists. Deleting a file that has not been opened retains its contents without
starting its programs. Restore uses that retained copy even if the file has since changed or
disappeared; it does not write the external file.

Delete saves the layout, scrollback and intent before stopping programs, using the same process
identity checks as Close. A failed preparation stops nothing and offers **Retry**. If cancellation
of a timed-out write cannot be confirmed, the dialog says so and asks to fix the save error and
retry. A failure after stopping keeps the prepared layout and offers **Retry Delete** in Workspaces.
After a crash, the next server finishes the recorded deletion without relaunching those programs.
A failed Restore keeps the deleted copy and the entered name for retry. Deleting the last workspace
leaves Workspaces open; attaching after restart does not create a replacement workspace.

### Restarting a session

`kraal restart` (or `ctrl+b W r`) restarts a session's server on the kraal installed now, after an
update for instance: the server saves the session and stops, and a new one starts and brings it
back as above. First it lists the programs that would stop (a shell at its prompt runs nothing; an
agent in the middle of a turn loses that turn; jobs left in the background stop) and asks, yes by
default when nothing would stop; `kraal restart --yes` does not ask. Without a terminal to ask on
(a script), it restarts when nothing would stop and fails otherwise. A session that cannot be
saved (a full disk) is not restarted: it keeps running, and the restart says why. Attached
terminals wait for the new server and come back to the workspace they showed. Run from a pane of
the session, the command lists the same programs as from anywhere else, the jobs in the
background of that pane included (but not itself, nor a script that runs it), and it stays in
that pane until the restart closes it.

Each pane also shows again what it showed, scrollback and colours included, down to a dim
`kraal: restored from <time>` line; what runs in it now writes below that line. Copy mode and
search reach the restored text. A full-screen program (an editor, `htop`) is not saved: its pane
comes back with the text from before it started. kraal saves this text when the server stops, and
every 10 seconds or so while a pane prints, so a crash loses little. A pane whose text could not be
written (a full disk) is saved again later, printing or not, and `kraal kill` says when it still
could not be.

Account login panes are excluded: neither the pane nor its scrollback is saved or restored.
Their account entries stay, marked **sign-in required** until sign-in is verified.

This text is written to disk, readable by you alone, in `~/.local/state/kraal/scrollback-v2/<session>/`,
and it can hold secrets (a token printed by a command, a password echoed by mistake):
`save_scrollback = false` in the config stops saving it and deletes what was saved (kraal deletes
only the files it wrote there, and nothing through a link put in place of that directory).
`kraal forget` deletes the current saved state, scrollback, attachments and saved paste buffers
of a stopped session. Original files from before 0.4 stay available for
[recovery](compatibility.md#saved-files-and-sessions). Paste buffers are
not saved unless `[buffers] persist = true` ([Configuration](configuration.md)).

### Workspace identities

A saved workspace has an identity independent of its name, position and source path.
`kraal workspaces --json` includes it as `id` on open and closed workspaces; file-only entries omit it.
Restarting keeps that identity. The pane and tab numbers used within a running server remain
local to that server.

Sessions use format 2 in `sessions-v2/<session>.json` with scrollback in
`scrollback-v2/<session>/`. Saved sessions from 0.4.x keep their workspace identities,
layout, source links and account references across restarts. Binding account identities
in retained recipes during ordinary workspace operations remains.

Kraal 0.5 and later leave older `sessions/<session>.json` and `scrollback/<session>/` files untouched.
Without a current session file, start, attach and restore refuse old state with
`run kraal 0.4 once first`. A newer state format is refused with `use a newer kraal`.
If reading or validating state fails, kraal leaves it in place and refuses to start the session.
Fix the reported cause and retry. See [Compatibility](compatibility.md#saved-files-and-sessions).

`kraal forget` removes current state, scrollback, attachments and saved paste buffers. If old
state remains, the next start refuses it with the same next step. Account changes do not rewrite saved sessions
or external workspace files. See [Accounts](agents.md#accounts).

A workspace whose panes cannot be restored keeps its identity, layout, source and failure reason.
Fix that cause and restart to retry; opening its source does not replace the saved workspace.

## Workspace files

Workspace layouts can be imported from any KDL file. Files in `~/.config/kraal/workspaces/*.kdl`
and the directories `workspace_dirs` names also appear in Workspaces. The picker shows an
error for an invalid file; `kraal workspaces --json` omits that file. The full sidebar and
compact strip show valid files from the last source scan. Drawing them does not rescan or
reload those files. Automatic scans read KDL files in the background only when their inputs
may have changed, a scan was requested again, or a previous read failed. The periodic check
compares metadata and directory listings, including sources linked from outside the configured
directories. See [Configuration](configuration.md) for the scan timing.
A source associated with a workspace already listed does not add another row.

Two to import and change:
[first.kdl](../examples/workspaces/first.kdl) (tabs, a split, a live preview, a command pane) and
[two-agents.kdl](../examples/workspaces/two-agents.kdl) (two agents, a stack of helper panes).
One with servers and an agent:

```kdl
layout {
    cwd "~/Dev/api"
    tab name="servers" focus=true {
        pane split_direction="vertical" {
            pane name="db" command="ssh" { args "admin@db.lan"; }
            pane name="web" command="ssh" { args "-p" "2222" "admin@web.lan"; }
        }
    }
    tab name="work" {
        pane split_direction="vertical" {
            pane agent="claude" size="60%"          // Claude Code, on the account the rules pick
            pane preview="README.md"                // a live Markdown preview
        }
    }
}
```

`layout` holds tabs, or panes directly when one tab is enough. What they take:

| Command | |
|---|---|
| `cwd "~/Dev/api"` in `layout`, `cwd="docs"` on a tab or a pane | where panes start; a relative one is taken from the one around it |
| `name="db"` on a tab or a pane | its name; a pane without one is named after what runs in it |
| `focus=true` on a tab or a pane | the tab shown, the pane focused, when the workspace opens |
| `split_direction="vertical"` on a tab or a pane with panes in it | they go side by side; `"horizontal"` (the default), one above the other |
| `size="60%"` or `size=40` on a pane | its share of the split it is in, in percent or in cells |
| `command="ssh"` with `{ args "-p" "2222" "host"; }` | the program the pane runs instead of a shell, and its arguments |
| `start_suspended=true` · `close_on_exit=true` | the command waits for Enter before it runs · the pane closes when it exits |
| `agent="claude"` (or `"claude:pro"`), with `args` if needed | that agent as the pane's program, on the account the rules pick (or the one named) |
| `preview="README.md"` | a preview of that file instead of a terminal |
| `stacked=true` on a pane with panes in it | they are a stack (below) |
| `floating_panes { pane … }` in a tab | panes floating over the others |

What kraal does not know in a file is left out, and `kraal config check` says which line.

An agent pane is a command pane: kraal runs `claude` or `codex` itself, with the account's
environment and no shell under it. When the agent exits, the pane stays with its exit code:
Enter starts it again, using a known conversation when supported ([resume limits](agents.md#resume-limits)),
`x` closes the pane, and `close_on_exit=true` closes it by itself. The name of another agent, which kraal keeps no accounts
for, is typed into a shell: that shell is what is left when the agent exits.

A saved agent account keeps an internal `account_id` beside its readable `agent="claude:name"`.
Renaming the account keeps that binding; removing and recreating its name does not replace it.
External definitions using only `agent="claude:name"` resolve by name when opened and are not
rewritten before account changes. Renaming that account leaves the definition unresolved;
reusing its old name makes the definition select the replacement account.
To choose a different account by editing the file, set `agent` and remove its old
`account_id`. A bare `agent="claude"` continues to choose by the account rules each time.

`stacked=true` makes a pane's children a stack: they share its place, one open and
the others folded to the top line of their frame ([Stacks and layouts](keys.md#stacks-and-layouts)).
The focused child is the one open, else the one marked `expanded=true`, else the first:

```kdl
layout {
    pane split_direction="vertical" {
        pane agent="claude" size="60%"
        pane stacked=true {
            pane name="tests" command="cargo" expanded=true { args "watch" "-x" "test"; }
            pane name="logs" command="tail" { args "-f" "app.log"; }
            pane name="shell"
        }
    }
}
```

From the command line:

| Command | |
|---|---|
| `kraal workspaces` | its open and closed workspaces with retained tab and pane counts, plus files not imported (`--json` for scripts); saved workspaces remain listed when the server is stopped |
| `kraal open shop` | ensures `shop` is open, reopening its saved layout or loading its file the first time; does not attach or change an attached terminal's displayed workspace (any name: `kraal open files` too) |
| `kraal import ./shop.kdl` | previews the file, then asks to add it; commands and agents wait for Enter |
| `kraal import ./shop.kdl --name shop-copy --yes` | accepts the preview with an explicit name, without prompting |
| `kraal export ./shop.kdl --workspace shop` | exports an open or closed workspace, without conversations or private account names |
| `kraal export ./shop.kdl --workspace shop --replace` | confirms replacement of the reviewed file, keeping a backup |

### Importing a layout

Use **From file...** or `kraal import` for a preview and commands that wait for Enter.
The older named-file paths (`kraal shop`, `kraal attach shop`, `kraal open shop`, or opening an
unimported file's row) still honor the file's launch settings on first open: commands can start
immediately unless marked `start_suspended=true`. Once retained, Open uses the personal layout.
`kraal open` does not change the workspace shown in attached terminals; choose it in Workspaces,
or use `kraal attach shop` from outside a pane.

Import and export output abbreviates paths under your home directory with `~`. Success
messages quote workspace names, for example `Workspace "imported" added.`

In **Add workspace**, choose **From file...**. Enter **File**, then Tab or **Preview** to see
the tab and pane counts and any unsupported KDL warnings. **Name** starts with the file's name
and remains editable. **Add** opens the workspace only after that preview. Parsing and previewing
run no programs. Shells may start on Add; commands and agents wait for Enter, even if the file
asked to start them immediately. The original file stays unchanged, with no copy required in
kraal's configuration. Later openings use the retained personal layout and normal resume rules.

An invalid file keeps the path and name in the form. Fix the file and **Retry**. A file that
changes after preview must be previewed again. A file already associated with a workspace offers
**Open** for that workspace, and never creates a duplicate. A taken name must be changed.
Missing explicit account references require a choice of an available account. Use the arrow keys
on the account field to choose, then **Next** if there are more references. If none is available,
add one in **Accounts**, then retry.
The CLI offers the same account choice in a terminal. `--yes` cannot choose a missing account
for you. A failed durable save rolls back the addition and leaves the form ready to retry.

The legacy batch command `kraal import-zellij --from ~/layouts` still copies valid layouts into
kraal's workspace directory, preserving their exact bytes, comments and unsupported nodes.
It uses the same parser and checked file writer. `kraal import-zellij --from ~/layouts --force`
allows replacement with a `.kdl.bak` backup; files owned by a retained workspace in this session
are skipped. It copies recipes without opening workspaces. Use **From file...** or `kraal import`
for the preview and waiting-command workflow.

Batch imports use the running server, or check saved workspace ownership while holding the
stopped session. If the session is busy without a reachable server, the import waits briefly
for it to become available. If it stays busy, that file is skipped unchanged with a message
to retry the import.

### Exporting a layout

**Export layout** is in a saved workspace's Tab actions, also while closed. The historical
`ctrl+b W s` shortcut opens the same form. **File** proposes an unused copy path. Change it to
export elsewhere, including to the source file explicitly. Export preserves the workspace's
name, identity and position; it does not add a second workspace to this session's list.
**Layout exported** reports the full path written. Its single **Close** button returns to the
previous screen. Errors keep the editable path with **Retry**.

The exported KDL describes tabs, splits, stacks, floating panes, folders and startup commands.
It contains no scrollback, conversations or running process state. Split sizes are rounded to
percentages and floating-pane geometry is not preserved. A tab with only floating panes gains
a shell underneath them. Ordinary commands typed into a shell are not turned into launch recipes.
Recognized agents are an exception: exporting while `claude` is detected running in a plain
shell produces `agent="claude"`. This keeps the agent kind, not the shell invocation's options,
environment or conversation. On first open through a named-file path, that pane can start
Claude automatically under the recipient's account rules. Use **From file...** or `kraal import`
to make it wait for Enter instead.
Folder paths and command arguments remain as configured; the recipient needs those folders
and programs. Managed agents use portable references such as `agent="claude"`; the recipient's account rules apply. Comments and unsupported KDL
cannot survive serialization, so every existing destination needs explicit replacement.
The source is never overwritten automatically.

A retained workspace keeps its source association across Close and restart: the sidebar and
`kraal workspaces` list it once, and `kraal open` with the name of its file opens that same
workspace without switching an attached terminal's view.
Renaming the workspace (`ctrl+b W e`, or **Rename** in the list) changes only its saved name.
It works while open or closed, without starting programs or renaming any file. Names may contain
`/` or start with `.` even when there is a source file. A file renamed on disk while its workspace
is open stays linked; one deleted can be written again by an explicit save.

### Saving without replacing someone else's layout

A destination linked to another workspace in this session is refused, including another path
or symlink to the same file. Choose another name. Workspaces whose panes could not be restored
also keep their file association.

Every existing destination, including an unchanged source, asks
**Replace file?**, with **Cancel** selected. A linked file edited outside kraal, including by
another session, asks **File changed** instead. Cancel returns to the file path you entered, where
you can choose a path for a copy. Confirming **Replace file** keeps the replaced file as
`.kdl.bak` beside the resolved target, including when saving through a symlink. Even an invalid
KDL file needs this explicit confirmation. If the file changes again while the question is open,
replacement is refused: retry to review the new version. Errors keep the form and its input open.
Enter retries with the same path; Esc cancels. Errors wrap below the input at both 80-column and
wider terminal sizes. If a diagnostic is taller than the dialog, `pageup`/`pagedown` or the mouse
wheel scrolls it while the input and footer stay visible. Moving the cursor keeps the error;
editing the value clears it.

`kraal export <file> --replace` confirms replacement from the CLI. There is no permanent
overwrite permission. Exporting a copy leaves the workspace's source association unchanged.

The comparison uses the loaded or last saved contents, not just a timestamp, and survives a
session restart. A session saved by an older version has no recorded contents, so its first
replacement needs confirmation. Directory scans and renames do not accept external edits on
your behalf. An explicit export can recreate a missing source; a dangling symlink is refused.

Saves from current kraal sessions use the same lock for a destination, including symlink
aliases. A replacement first moves the existing file aside, checks those exact contents, and
publishes the complete new file only if the destination is still absent. An editor that creates
the destination meanwhile wins. The destination is briefly absent during replacement. Writes
through an editor's already open descriptor remain in the backup. On failure, kraal restores
the old file if the destination is free; otherwise it keeps both versions and reports the
recovery file to inspect before retrying. After a crash during replacement, the old file is in
`.kdl.bak` or a sibling named `.<file>.<pid>.<number>.recover`; move it back if the destination is
missing. Recovery files are never removed by temporary-file cleanup.

### Reviewing changes to a source file

An external edit never replaces your saved layout or stops its programs. The Workspaces list
and sidebar show **File changed**, **File error**, or **Source file missing** on the saved
workspace. **Open** still uses your saved layout. A missing source is never recreated by an
automatic save.

**File changed** compares the file's contents with the source contents recorded when kraal
imported, loaded, or exported it. Editing tabs,
panes, or workspace names never triggers it. The recorded contents survive a restart.

Choose **Review** from the notification or the workspace's Actions. **Load from file** in Actions
opens the same review, even when the source is unchanged. It is available only for a workspace
with a source. Review shows the file name, saved and file tab and pane counts, and import warnings.
**Cancel** is selected first. **Keep layout** keeps your personal layout and acknowledges only the
file version reviewed. A later edit is reported again. It never grants permission to overwrite
that file on export.

**Load from file** keeps the previous local layout for Undo and saves the replacement. If programs
or jobs need to stop, **Stop and load** lists them and requires confirmation. A file edited again
or a changed set of programs requires another review before anything stops. New commands,
including agents, wait for Enter. An open workspace opens the loaded layout; a closed workspace
stays closed. Your workspace keeps its name, identity, source association, and list position.

The **Undo** notification and **Undo load** in Actions restore the previous layout as **Closed**.
Open it when ready. Undo does not restart stopped processes or recover their unsaved work.
If programs or jobs need to stop, Undo asks for confirmation. This also applies while a new
shell's activity cannot yet be checked. Cancel keeps the current layout and leaves Undo available.
One previous layout is retained per workspace, including across restart. Another successful load
replaces that Undo with the layout it replaced. Undo never edits the source file.

An invalid or missing source opens **File error** with the parser or read diagnosis and
**Open saved**. **Details** shows the full path and wrapped error, with scrolling when needed.
Fix the source and choose **Retry** there to read it again. Invalid files not yet imported open
the existing **Add workspace** form, keeping the file and name for Retry. No workspace is added
until the file is valid and the import succeeds. If a load refers to an unavailable account,
correct its reference in the file or add the account in Accounts, then review again.

If saving fails before programs stop, they remain running. If a later step fails, the previous
layout stays saved and the message gives the next action. An unfinished Close must be completed
before retrying Load. If the replacement is saved but cannot open, retry **Open** or use
**Undo load** from Actions.

### Workspace commands

`kraal workspaces` (also `kraal ws`) lists open and closed workspaces and unimported files.
`kraal workspaces --json` includes saved workspace IDs. The commands below accept a saved
workspace's name or ID. IDs disambiguate saved workspaces with the same name.

| Command | Result |
|---|---|
| `kraal workspaces create api` | Creates and saves a workspace with a shell in the calling directory |
| `kraal workspaces rename api backend` | Renames the saved workspace, keeping its source file unchanged |
| `kraal workspaces close backend` | Stops its programs and keeps the layout as Closed |
| `kraal workspaces close backend --yes` | Accepts stopping the programs shown in the preview |
| `kraal workspaces delete backend` | Confirms recoverable deletion, keeping project and source files |
| `kraal workspaces delete backend --yes` | Accepts deletion and stopping the programs shown in the preview |
| `kraal workspaces --deleted` | Lists deleted workspaces with their IDs, even with the server stopped |
| `kraal workspaces --deleted --json` | Lists the same recoverable workspaces as JSON |
| `kraal workspaces restore backend` | Restores the deleted workspace as Closed, without starting programs |
| `kraal workspaces restore backend --name backend-restored` | Restores under another name when the old name is taken |

Mutating commands start the session server if needed, with its usual session restoration.
Create and Rename do not change the workspace shown by attached terminals. Close and Delete
use the same durable operations as the list. Close lists running programs and background jobs
before asking; idle shells need no extra confirmation. Delete always asks. Without a terminal,
commands that need confirmation stop with instructions to use `--yes`. If the workspace or its
programs change after the preview, nothing is stopped: run the command again to review them.

Delete also accepts an unimported file's workspace name and remembers its removal without
removing the file. Rename requires importing that file first with `kraal open NAME`.
If several deleted workspaces share a name, use the ID from `kraal workspaces --deleted`.
Restore never replaces a workspace that now uses the old name; use `--name` to choose another.
After Restore, `kraal open NAME` opens it. `kraal close` and `kraal rename NAME` still act on panes.
