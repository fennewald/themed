# themed

One piece of shared state — the current UI theme — kept in sync across a small
fleet of personal machines on a private Tailscale tailnet.

`themed` is a last-write-wins register plus opportunistic one-hop gossip. Theme
changes come from interactive use: at any moment the operator is at one keyboard
on one machine, so the machine where the change happens **pushes** it to the
others. No consensus, no heartbeat, no polling, no election. Idle cost is a
daemon sitting in `accept()` — no timers, no periodic log lines.

## Using it

```console
$ themed set '{"mode":"light"}'   # tell the local daemon; it fans out
$ themed get
{"mode":"light"}
```

`set` exits non-zero only if the local daemon is unreachable or the payload is
not a single JSON value. Peers that are down are logged and skipped; they catch
up when they next start.

## Running the daemon

Invoking `themed` with no subcommand runs the daemon:

```
themed \
  --self          <hostname> \
  --listen        <tailscale-ipv4>:47100 \
  --state-file    $XDG_STATE_HOME/themed/state.json \
  --socket        $XDG_RUNTIME_DIR/themed.sock \
  --reconcile-cmd '<shell command>' \
  --peer host-a.example.ts.net:47100 \
  --peer host-b.example.ts.net:47100
```

| Flag | Default | Notes |
|---|---|---|
| `--self` | system hostname | Only feeds the version tiebreaker. |
| `--listen` | `$(tailscale ip -4):47100` | Discovery retries with backoff until tailscaled answers. |
| `--state-file` | `$XDG_STATE_HOME/themed/state.json`, else `~/.local/state/themed/state.json` | Rewritten in place on every adopted change, so watchers keep their inode. |
| `--socket` | `$XDG_RUNTIME_DIR/themed.sock`, else `$XDG_STATE_HOME/themed/themed.sock` | macOS has no `XDG_RUNTIME_DIR`; the state dir is a path the CLI derives identically. |
| `--reconcile-cmd` | none | Run via `sh -c` with the blob on stdin. |
| `--peer` | none | Repeatable. Zero peers is valid and harmless. |
| `-v/--verbose` | off | Per-message tracing. `RUST_LOG` also works. |

Meant to run as an unprivileged per-user service (`systemd --user`, launchd
LaunchAgent) with restart-on-failure. Binding retries if the Tailscale interface
is not up yet, so starting before tailscaled is fine.

## The blob is opaque

`themed` never interprets the theme payload. It stores it, ships it, hands it to
the reconcile hook on stdin, and compares records only by `version`. Today the
blob is `{"mode":"dark"}`; tomorrow it can grow accent colors or per-app
overrides with **zero** changes here — all meaning lives in the hook.

The hook runs on every *change*: a local `set`, an adopted `announce`, and once
at startup. A record that carries the same blob as the one already held only
refreshes the version — no hook, no fan-out. Because the daemon cannot know what
the system had applied before it started, the startup run is unconditional; the
hook should be idempotent.

## Protocol

Plain TCP on port **47100**, bound to the Tailscale address. One JSON object per
line, UTF-8. Connections are one-shot: connect, send one message, read the reply
if there is one, close.

| From → To | Message | Reply |
|---|---|---|
| setter → peers | `{"t":"announce","version":<u64>,"blob":<json>}` | none |
| starter → peers | `{"t":"query"}` | `{"t":"state","version":<u64>,"blob":<json>}` |

`announce`: if `version > local.version`, adopt and persist it; if the blob also
differs, run the hook and re-announce once to every peer. That single re-
broadcast is what covers partial connectivity (A cannot reach C, but B can); it
self-terminates because a stale re-announce is ignored by every recipient.

The control socket speaks the same framing: `{"t":"set","blob":<json>}` →
`{"t":"ok"}` or `{"t":"err","msg":…}`, and `{"t":"get"}` → `{"t":"state",…}`.
Each socket accepts only its own commands.

`version` is an opaque ordering token — nanoseconds since the Unix epoch with a
hash of `--self` in the low byte, forced past the previous value so a local set
always wins locally. Never interpret it beyond `>` / `<=`.

Malformed lines and unknown message types are logged at debug and dropped. The
daemon never crashes on input.

## Trust model

**The tailnet is the security boundary.** No authentication, no TLS. Any host
that can reach the listener may set the theme. Bind only to the Tailscale
address.

## Nix

```nix
{
  inputs.themed.url = "github:fennewald/themed";
  # then either:
  #   nixpkgs.overlays = [ inputs.themed.overlays.default ];  →  pkgs.themed
  #   home.packages = [ inputs.themed.packages.${system}.default ];
}
```

### Home-Manager

`homeManagerModules.default` defines the per-user service —
`systemd --user` on Linux, a launchd LaunchAgent on macOS (nix-darwin or
standalone) — and builds `themed` for the host's system, so no overlay is
needed.

```nix
{
  imports = [ inputs.themed.homeManagerModules.default ];

  services.themed = {
    enable = true;
    peers = [
      "fezzik.coelacanth-byzantine.ts.net:47100"
      "vizzini.coelacanth-byzantine.ts.net:47100"
    ];
    reconcileCmd = "nu --no-config-file -c 'use theme.nu; theme reconcile'";
  };
}
```

`--listen` is omitted by default, so `tailscale` is placed on the service PATH
(override with `services.themed.tailscalePackage`) and the daemon discovers its
address itself. See `nix/hm-module.nix` for every option (`selfName`, `listen`,
`stateFile`, `socket`, `logLevel`, `extraArgs`, …). Import
`homeManagerModules.themed` instead if you want the package to come from
`pkgs.themed` via `overlays.default`.

## Development

```console
$ cargo test        # unit tests + end-to-end tests over real sockets
$ cargo clippy --all-targets
$ nix build
```
