# Production

A safety net for the panes where a mistake costs the most.

A pane is production when its server, Kubernetes context or namespace, Terraform
workspace or AWS profile has `prod`, `production` or `prd` as a word of its name (`web-prod-1`,
`prod-eks`, `db.prod.lan`, `prd01`; not `products-api`, nor `preprod`). For a server, its name,
its host and its groups in `servers.toml` are looked at, unless it says its stage itself: with
`env = "prod"` (`kraal servers add … --env prod`) it is production, and with an `env` that has
none of these words (`"staging"`) it never is. In an SSH pane, the contexts the shell on the
server reports count too
([Contexts](tools.md#contexts)). A context kraal cannot know (a kubeconfig it cannot read or has
not read yet, a name a server sends that kraal does not take) counts as production. Production
panes get a red frame and a PROD badge, their tab turns red, and kraal asks before:

- broadcast typing reaches a production pane (once per pane and production reason, until the
  broadcast stops; no stops it);
- a paste with a line break goes into a production program that would run it as it arrives (a
  shell in bracketed paste mode shows it first, so it is not asked), from your terminal or from
  the paste buffers;
- a tool starts on a production pane, unless the tool says `read_only = true` (htop, logs…);
- a file of a production server opens in your editor (`e` in the file manager, a path hint in
  its pane);
- a runbook runs on production targets (its plan says which).

Broadcast input waits while a target's context is being read. A **Yes** given during that
read waits too: it sends the input only if the panes still have the production status named
in the question. If that status changes or the read stops answering, nothing is sent and kraal
tells you why. Type or paste the input again to ask about the new context. Input kept during
a read stays with the panes it was typed for, even if you switch tabs before the read ends.

Scripts get no prompt: `kraal send`, `kraal keys` and `kraal buffer paste` refuse a production
pane, and `kraal edit` a production server's file, unless given `--production`. From an agent's
pane, `--production` asks you instead (see [Agents](#agents)).

In `[production]` of the configuration, `words` changes the words (`[]` turns the whole guard
off, frames included), and `confirm_sync = false` or `confirm_paste = false` turns off the first
or the second question. `[contexts]` chooses which of Kubernetes, Terraform and AWS count.

This guard is a safety net based on names, not a permission system. Outside agents' panes, it
does not stand in the way of what names its target itself: `kraal exec`, `kraal cp` and
`kraal rm`, the file manager's copies, moves and deletions, and `kraal runbook run` with `--yes`.

## Agents

When a coding agent runs one of these kraal commands on production, kraal asks you first, and
nothing is done until you choose **Allow**:

- `kraal exec`, `kraal cp` and `kraal rm` with a production server among their servers (a copy's
  sources count, not only its destination);
- `kraal ssh` to a production server, in the agent's own terminal, and `kraal ssh` with a
  command that opens panes on production servers (`--group` included);
- `kraal send`, `kraal keys` and `kraal buffer paste` with `--production`, and `kraal edit` with
  `--production`;
- `kraal runbook run` with production targets in its plan, `--yes` or not. A runbook on
  production whose parameters list their choices with a command (`choices_command`) is refused
  from an agent's pane or a pane of another session, plan and run alike: those commands would
  run before you are asked. Run that one yourself.

The question names the agent and its pane, quotes what runs (the command line, a runbook's whole
plan, the text or keys to type, the buffer to paste, the line each server runs) and lists the
production panes and servers. It opens on a client that shows the pane, else the one used last,
when that client shows no other question or form and no file transfer holds its typing;
questions come one at a time. Your client tells kraal once it has drawn the question on your
terminal, over `attach --host` too: keys that reach kraal before that, or in the 0.3 s after
(held during a transfer, sent together with the key before it, or typed while a slow connection
still carries the question), do not answer it. **Cancel**, selected first, refuses: the command fails with
`refused in kraal: nothing was done`. With no client attached, the command is refused at once.

What you allow is what runs: the text, keys or buffer as they were when you were asked. If what
it reaches changed meanwhile (a pane's server or production context, a server's address, port or
other connection settings such as its jump host, the servers of a group, a runbook's targets,
checked again when the run starts), nothing runs and the
command says what changed. A command that kraal cannot resolve, or whose
session it cannot ask (stopped, or running another [protocol](compatibility.md#clients-and-servers)),
fails without doing anything.

A command counts as the agent's when it runs in a pane where kraal sees an agent: found from the
process that asks and those it descends from, else from the pane named by its environment and by
that of the nearest process it runs under that has one (`KRAAL_SESSION`, `KRAAL_PANE`,
`KRAAL_SOCKET`), which must agree. Only that pane's session knows the agent, so:

- `kraal exec`, `cp`, `rm` and `ssh` in the agent's own terminal are asked about in the session of
  the pane they run in, whatever `-s` says; if that session cannot be asked, they do nothing;
- a command sent with `-s` to another session, which would act there on production, is refused
  there, from any pane of another session: run it from a pane of that session, or yourself.

A command with none of these variables, run under no process that has them, is outside kraal:
`kraal exec`, `cp`, `rm` and `ssh` act as before.

Each answer is a JSON line in `~/.local/state/kraal/production.log` (kraal's state directory),
written whole under a lock that sessions share: when, the session, the pane and its workspace,
the agent, the command, the production panes and servers and the decision (`allowed`, `refused`,
`refused: nobody attached`, `refused: changed while asked`). An approval that cannot be written
there runs nothing. Hooks and `kraal events` get `production.approved` or `production.refused`.

Anything else an agent runs is out of kraal's sight: `ssh`, `kubectl`, `aws` or `terraform` run
directly, `kraal run`, which starts any command in a new pane without looking at it, and a
command that both left the pane's processes (its parents gone) and has the `KRAAL_` variables
removed.
Keeping an agent off production for good takes keeping production's keys and credentials out of
its reach.
