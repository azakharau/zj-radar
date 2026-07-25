# zj-radar-claude

A Claude Code **plugin** that broadcasts agent status (working / waiting / done)
to [zj-radar](../../)'s `{pipe_agents}` zjstatus widget — with **no
`settings.json` editing**. Installing the plugin auto-registers the hooks;
uninstalling removes them cleanly.

This plugin only sends status. Install the aggregator first with the
[main install guide](../../docs/install.md), then add this producer.

## Install

One-time (from the zj-radar marketplace repo):

```
/plugin marketplace add marktoda/zj-radar
/plugin install zj-radar-claude@zj-radar
```

Or scriptable / local dev (no marketplace):

```
claude plugin install zj-radar-claude@zj-radar      # after adding the marketplace
claude --plugin-dir /path/to/zj-radar/plugins/zj-radar-claude   # session-only
```

## What it does

Registers these hooks (all calling the bundled `scripts/notify.sh`):

| Hook | Status |
|------|--------|
| `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `SubagentStop` | `running` |
| `Notification` (`permission_prompt` / `elicitation_dialog` matchers) | `pending` |
| `Stop` | `done` (cleared by the aggregator's `done_ttl_secs`, or the next broadcast) |
| `SessionStart` (`matcher: clear` only) | `idle` (resets the entry on `/clear`) |
| `SessionEnd` | `idle` (clears the entry when the Claude session exits) |

Each fires a `zellij pipe --name zj_radar.status.v1` broadcast. It is a **no-op
outside Zellij**, so it's safe to leave enabled everywhere.

The bundled `notify.sh` requires `jq` and `git` on PATH (to parse the payload and
derive repo/branch). If the native [`zj-radar`](../../docs/producers.md#codex-and-the-native-cli)
CLI is installed, the script automatically prefers it (`exec zj-radar notify
claude`), which needs neither `jq` nor `bash` — the `jq`+`bash` path is only the
fallback when the binary isn't on PATH.

## Uninstall

```
/plugin uninstall zj-radar-claude@zj-radar
```

Hooks are removed automatically — nothing to clean up by hand.
