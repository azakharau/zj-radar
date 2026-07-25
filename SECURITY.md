# Security policy

Please report vulnerabilities privately via
[GitHub Security Advisories](https://github.com/marktoda/zj-radar/security/advisories/new)
rather than a public issue. You should hear back within a few days.

The main supply-chain surface is distribution: the `curl | sh` installer and its
per-artifact `.sha256` checksum sidecars, published alongside the aggregator
wasm (`zj_radar_agents.wasm`) on each tagged release — see
[`docs/install.md`](docs/install.md) for verifying that checksum by hand.
Reports about weaknesses in that path are especially welcome. There is no bug
bounty.

## Pipe trust model

The `zj_radar.status.v1` pipe has a local-session trust boundary: any process
inside the Zellij session (or another plugin, via `MessagePlugin`) can forge
payloads. The aggregator plugin treats them as untrusted display data —
payloads over 64 KB are dropped whole and every text field is sanitized and
truncated at parse time (`zj-radar-core`'s `parse`). The plugin's own Zellij
permissions are `ReadApplicationState` + `MessageAndLaunchOtherPlugins` only —
it runs no commands and mutates no application state, so a forged payload's
worst case is a misleading entry in the `{pipe_agents}` widget. What that
cannot prevent: a local writer can always paint misleading status. That is
inherent to the boundary, not a vulnerability.
