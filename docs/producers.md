# Producers — sending agent status to the aggregator

The aggregator (`zj-radar-agents`) is just a display, folded through your
zjstatus bar's `{pipe_agents}` widget. A **producer** is whatever broadcasts
agent status to it. zj-radar ships producers for Claude Code and Codex, and
the wire format is a documented pipe payload so you can write your own.

Install [the aggregator](install.md) first, then add a producer below.

## Claude Code

Installing this plugin auto-registers the status hooks — **no `settings.json`
editing**, clean uninstall. One shell command drives Claude Code's own plugin
CLI (marketplace add + install):

```sh
zj-radar setup claude
```

Or do the same from inside Claude Code (these are `/plugin` slash commands,
not shell) — both routes land on the identical marketplace install:

```text
/plugin marketplace add marktoda/zj-radar
/plugin install zj-radar-claude@zj-radar
```

The first command registers this repo as a plugin marketplace named `zj-radar`;
the second installs the `zj-radar-claude` plugin *from* it — that's what the
`zj-radar-claude@zj-radar` (`plugin@marketplace`) syntax means.

Requires `jq` and `git` on `PATH` (used to parse the hook payload and derive
repo/branch). See [`plugins/zj-radar-claude/README.md`](../plugins/zj-radar-claude/README.md)
for details. It's a no-op outside Zellij, so it's safe to leave enabled
everywhere.

## Codex and the native CLI

A native binary that drops the `jq`/`bash` dependency and wires non-plugin agents.

```sh
# Release tarballs (published on tagged releases; named by Rust target triple):
#   zj-radar-x86_64-unknown-linux-musl.tar.gz
#   zj-radar-aarch64-unknown-linux-musl.tar.gz
#   zj-radar-aarch64-apple-darwin.tar.gz
# Nix:
nix build github:marktoda/zj-radar#zj-radar-cli   # -> result/bin/zj-radar
# Cargo (crates.io; add `--git https://github.com/marktoda/zj-radar` for HEAD):
cargo install zj-radar
```

- **`zj-radar notify <claude|codex>`** — broadcasts agent status. The Claude
  plugin's hook script automatically prefers it when it's on `PATH` (jq-free);
  otherwise the plugin falls back to its bundled `bash`+`jq` script.
- **`zj-radar setup [codex]`** — idempotently wires Codex's
  `~/.codex/hooks.json` to call `zj-radar notify codex`. This preserves any
  existing Codex `notify` program (e.g. a Computer Use notifier), because hooks
  are additive. Use `--dry-run` to preview, `--uninstall` to remove only
  zj-radar's hooks, and `--check` to diagnose the current setup. After installing
  or changing hooks, run `/hooks` inside Codex once to review and trust the
  command hook. (Claude needs no `setup` — use the plugin above.)
- **`zj-radar setup codex --legacy-notify`** — opt-in fallback for older Codex
  setups that only support the single `notify` program. It refuses to replace a
  foreign notifier unless `--force` is also passed.

`setup` has exactly these two targets, `claude` and `codex` — there is no
`zellij` target. Running the aggregator itself is not something `setup`
manages; see [`install.md`](install.md).

Codex hooks report turn start, tool use, permission requests, subagents, and
turn stop. zj-radar maps those to `running`, `pending`, and `done`.

## Any script: `zj-radar notify generic`

Anything that isn't an instrumented agent — deploy scripts, cron jobs,
homegrown loops — can put a row on the radar without touching the wire format:

```sh
zj-radar notify generic --status running --msg "deploying site" --task "nightly deploy" --source deploy
# … do the work …
zj-radar notify generic --status done --msg "deploy finished" --source deploy
```

- `--status` (required): `running` | `pending` | `done` | `error` | `idle`. An
  unknown token prints a hint and sends nothing — it never lenient-parses to
  `idle` and erases your row.
- `--msg`: the activity line. `running` with no msg gets a `working` baseline;
  `idle` always broadcasts blank.
- `--task`: the sticky task label. Sent on the wire, but not currently read
  by the shipped aggregator (see the note on `task`/`ack` below).
- `--source`: picks the kind mark — `test` ⚗ · `build` ⚙ · `deploy` ⇡ ·
  `server` ❯ · `command` $ — anything else (including the default `generic`)
  renders the neutral `⦿`.
- Repo/branch come from `git` in the calling directory; the pane id from
  `$ZELLIJ_PANE_ID`. Outside Zellij it's a silent no-op (safe under `set -e`).
  `--dry-run` prints the payload instead of broadcasting.

The same lifecycle rules as agents apply: latest broadcast wins, and an entry
is dropped once its pane has gone quiet for `ttl_secs` (default 15 minutes;
`done` uses the shorter `done_ttl_secs`, default 30s — see
[`configuration.md`](configuration.md)). There is no return-to-shell-prompt
auto-clear — the aggregator does not watch pane command activity, only the
status pipe — so send `done`/`error` promptly when your script finishes
rather than leaning on the TTL to eventually hide a stale `running`.

## Writing your own producer

Writing one in Rust? Depend on
[`zj-radar-core`](https://crates.io/crates/zj-radar-core)
([docs.rs](https://docs.rs/zj-radar-core)) — the same crate both the CLI and
the aggregator plugin use: build a typed `StatusPayload` and serialize it with
`to_wire`, round-trip-tested against this schema, so your payload can't drift
from what the aggregator accepts. Everything below applies either way; the
crate just handles the encoding for you.

The aggregator's only real interface is the versioned pipe payload. Broadcast
(by name, never `--plugin`) a `zj_radar.status.v1` message:

```json
{ "v": 1,
  "source": "claude",
  "pane": { "type": "terminal", "id": 12 },
  "status": "running",
  "repo": "pinky",
  "branch": "fix/x",
  "msg": "running tests…",
  "task": "fix the flaky auth test" }
```

- `status`: `running` · `pending` · `done` · `error` · `idle`. An **unknown or
  empty `status` folds to `idle`**, which the aggregator treats the same as
  `gone` — it *drops* the pane's entry entirely — so a typo'd status silently
  erases the entry you meant to update; validate before broadcasting.
- `pane.id`: strip any `terminal_` prefix from `$ZELLIJ_PANE_ID`.
- `source`: tokens are lowercase-exact — matching is case-sensitive, so
  `"claude"` classifies as the Claude agent while `"Claude"` falls back to the
  neutral kind.
- `repo`, `branch`, `msg`, `task`, `ack`: part of the durable wire contract —
  parsed, sanitized, and capped like every other field below — but the
  shipped `zj-radar-agents` aggregator does not currently track or render any
  of them: it keys its whole aggregate on `pane_id` + `status` + `source`
  only (see `crates/agents/src/lib.rs`). Send them anyway if you like (a
  future or custom consumer of the same pipe could use them), just don't
  expect today's `{pipe_agents}` widget to show them.
- Unknown fields (including a legacy `on_focus`) are ignored, so it's safe to
  send extras.

The aggregator applies the latest broadcast per pane (the pipe delivers in
order, so there is no sequence number) and drops a pane's entry once it has
gone quiet for `ttl_secs` — see [`configuration.md`](configuration.md). The
wire parser (`zj-radar-core`, shared by every consumer) also defends against
malformed input: it strips ANSI/control chars and Unicode bidi-control
characters, folds newlines to spaces, and silently ignores unknown fields, so
extra keys never break a producer. It also enforces field limits, so you
don't have to pre-truncate: `repo`/`branch` are cut to 40 chars, `msg`/`task`
to 60, `source` to 16 — and a payload over **64 KB** is dropped whole.
`pane.type` must be `"terminal"`; any other pane type is rejected.

Quick smoke test (a "fake agent" — broadcast straight from your shell):

```sh
zellij pipe --name zj_radar.status.v1 -- \
  '{"v":1,"source":"test","pane":{"type":"terminal","id":12},"status":"running","repo":"demo","branch":"main","msg":"hello"}'
```

**Bound your sends.** `zellij pipe` is not fire-and-forget: Zellij holds the
client process until *every* loaded plugin instance consumes the message
(CLI-pipe backpressure). There is exactly one recipient now — the headless
aggregator, loaded once per session — rather than the one-per-tab sidebar
this replaced, so the blast radius of a stuck recipient is far smaller than it
used to be. But the same discipline still applies: a producer that fires per
tool-call with no timeout can still leak a blocked process and Zellij-server
FDs per event if the recipient is ever unresponsive, until the server hits
EMFILE. Wrap the call in a timeout (the bundled producers use 5 s,
`ZJ_RADAR_PIPE_TIMEOUT` to override); killing the client past the deadline
loses nothing — the message is already queued server-side.

The timeout must survive **your own death**, too. Hook runners kill their
hooks, and a producer killed mid-send never runs its kill-on-deadline — the
blocked `zellij pipe` client re-parents to init and leaks forever (this
exact orphan class EMFILE-crashed a real session). Put the watchdog *inside*
the subtree you spawn, not only in your process: run the pipe under a shell
alongside a detached `sleep <deadline>; kill` pair, the way the bundled
producers do (`self_limiting_pipe_argv` in `zj-radar-core`'s `pipe` module,
mirrored by notify.sh's sleep+kill watchdog).
