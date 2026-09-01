# `themed` — design & implementation brief

> This file is the handoff for the agent implementing `themed` in `~/proj/themed`.
> It was written from inside the `gvc-fleet` repo (`~/latakoo/fleet`), which is
> the sole consumer. Copy it into the project as `DESIGN.md` (or similar) — plan
> mode blocked writing outside the plan file directly.

---

## 1. What this is

`themed` is a tiny cross-platform daemon that keeps **one piece of shared state —
the current UI theme — synchronized across a small fleet of personal machines**,
replacing an etcd cluster that was doing the same job with absurd overhead
(raft consensus, heartbeat traffic, per-host disk, quorum-loss write failures,
`etcdctl member add` ceremony, ~400 lines of CLI wrapper — all to move one
string).

Design philosophy: theme changes are driven by **interactive use**. At any
moment the operator is at one keyboard on one machine. So: an already-listening
socket on every host, and the machine where the change happens **pushes** it to
the others. No consensus, no heartbeat, no polling, no election. A last-write-
wins register plus opportunistic one-hop gossip is entirely sufficient.

Hard requirements:
- Zero idle cost beyond sitting in `accept()` — no timers, no keepalive pings.
- A machine that was powered off **catches up on next start**.
- Changing the theme while some peers are unreachable still works (applies
  locally, propagates to whoever is reachable, converges later).
- The daemon is **schema-agnostic about the theme payload** (see §4).
- Runs as an unprivileged per-user service: `systemd --user` on Linux, `launchd`
  LaunchAgent on macOS.

Non-goals: authentication/encryption on the wire (the transport is a private
Tailscale tailnet — see §6), persistence beyond a single cached value, more than
one logical key, history, or any HTTP surface.

---

## 2. The fleet it runs on (context from `gvc-fleet`)

`gvc-fleet` is a single Nix flake configuring every machine the operator (Carson)
runs. Relevant hosts:

| Host        | OS / builder                         | Arch           | Notes                                  |
|-------------|--------------------------------------|----------------|----------------------------------------|
| `fezzik`    | NixOS + home-manager                 | x86_64-linux   | always-on dev host, Austin office      |
| `vizzini`   | NixOS + home-manager                 | x86_64-linux   | always-on, Austin office               |
| `fire-swamp`| nix-darwin + home-manager            | aarch64-darwin | Carson's MacBook (M1)                  |
| `xanadu`    | standalone home-manager (Debian)     | x86_64-linux   | Carson's home PC                       |
| `max`       | standalone home-manager (Ubuntu)     | aarch64-linux  | Jetson Orin AGX, Carson's home         |
| `mt-vernon` | standalone home-manager (Debian)     | x86_64-linux   | headless subnet router; theming n/a    |

- **All hosts are on one Tailscale tailnet**, `coelacanth-byzantine.ts.net`, with
  MagicDNS — every host is reachable at `<host>.coelacanth-byzantine.ts.net`.
- **Peer registry**: `hosts/registry.nix` in the fleet repo is the single source
  of truth for fleet membership (currently `fezzik`, `vizzini`, `fire-swamp`,
  `xanadu`, `max` — not `mt-vernon`). The fleet's Nix code will generate
  `themed`'s `--peer` list from `attrNames registry` × the tailnet domain,
  excluding the local host. A host absent from the registry simply runs with no
  peers, which must be harmless.
- **Nix builds `themed` natively per host** for its own system (x86_64-linux,
  aarch64-linux, aarch64-darwin). No cross-compilation is required of you, but
  **keep the dependency tree tiny** — ideally `std` only, `serde_json` at most.
  `Cargo.toml` is already `edition = "2024"`, name `themed`, version `0.1.0`.

### How the fleet will package & wire it (external contract you're coding to)

This repo now ships the wiring as `homeManagerModules.default` (see
`nix/hm-module.nix`), so the fleet repo only has to:
1. Add the flake input and `imports = [ inputs.themed.homeManagerModules.default ]`
   on every host. The module builds `themed` for the host's system itself, so no
   overlay is required (`homeManagerModules.themed` + `overlays.default` is the
   alternative if `pkgs.themed` is wanted globally). **`Cargo.lock` is committed.**
2. Set `services.themed.enable = true` and `services.themed.peers` — the
   `attrNames registry` × tailnet-domain list, minus the local host — plus
   `reconcileCmd` (§5). The module also exposes `selfName`, `listen`, `stateFile`,
   `socket`, `logLevel`, `extraArgs`.
3. The module already emits `Restart=on-failure` (systemd) / `KeepAlive=true`
   (launchd), puts `tailscale` on the service PATH so `--listen` can be omitted
   (`tailscale ip -4` discovery, retried), and pins `--socket` to
   `/tmp/themed.sock` on Darwin (no `XDG_RUNTIME_DIR`). The daemon still retries
   the bind if the tailscale interface is not up yet.

---

## 3. Wire protocol (peer ↔ peer)

- **Transport**: plain TCP on a fixed port (pick one, e.g. `47100`; document it —
  the fleet side must match). Listener bound to the host's Tailscale IPv4.
- **Framing**: one JSON object per line (`\n`-delimited), UTF-8.
- **No auth, no TLS.** The tailnet is the security boundary (§6).
- **Connections are one-shot**: connect, send one message, read the reply if the
  message expects one, close. Do not hold long-lived peer connections.

Messages:

| From → To      | Message                                             | Reply                                                  |
|----------------|----------------------------------------------------|--------------------------------------------------------|
| setter → peers | `{"t":"announce","version":<u128>,"blob":<json>}`  | none                                                   |
| starter → peers| `{"t":"query"}`                                     | `{"t":"state","version":<u128>,"blob":<json>}`         |

Semantics:
- **`announce`**: if `version > local.version`, adopt (`version`, `blob`),
  persist, run the reconcile hook, then **re-announce once** to every *other*
  peer. If `version <= local.version`, ignore. This one-hop re-broadcast is what
  covers partial connectivity (A can't reach C, but B can); it self-terminates
  because a stale re-announce is ignored by every recipient.
- **`query`**: reply with the current record. Used only at startup.
- Malformed line / unknown `t`: log at debug, drop the connection, never crash.

### `version`

An opaque monotonic **last-write-wins ordering token**, NOT a schema version.
Generate it on a local `set` as: `u128` nanoseconds since the Unix epoch, and if
you want a tiebreaker, fold in a hash of `--self`. Never interpret it beyond
`>` / `<=`. Clock skew across this fleet is negligible and a theme flip losing a
race by a few seconds is a non-event.

---

## 4. The `blob` is opaque

`themed` **never parses `blob`**. It stores it, ships it, hands it to the
reconcile hook on stdin, and compares only `version`. All meaning — today
`{"mode":"dark"}` vs `{"mode":"light"}`, tomorrow maybe per-app overrides,
accent colors, whatever — lives in the nushell reconcile logic on the fleet
side. This is deliberate: theme-schema changes then require **zero** changes to
`themed` and no versioning dance. Treat `blob` as `serde_json::value::RawValue`
or just an unparsed `String` you validated is a single JSON value.

---

## 5. Control socket (CLI ↔ local daemon)

- **Transport**: Unix domain stream socket at `--socket` (default suggestion
  `$XDG_RUNTIME_DIR/themed.sock`; on macOS there's no `XDG_RUNTIME_DIR` — accept
  whatever path the flag passes, the fleet will pick a cache dir).
- Same newline-JSON framing. One command per connection.

| Command (client → daemon)                    | Reply                                    | Effect |
|---------------------------------------------|------------------------------------------|--------|
| `{"t":"set","blob":<json>}`                  | `{"t":"ok"}` / `{"t":"err","msg":…}`     | `version = now_ns`; persist; run reconcile hook; `announce` to all peers (parallel, per-peer timeout ~2s, failures logged not fatal). |
| `{"t":"get"}`                                | `{"t":"state","version":…,"blob":<json>}`| read-only |

Ship a client mode in the same binary so the fleet's nushell wrapper can just
shell out:
- `themed set '<json blob>'` — exit 0 if the daemon accepted it (fan-out
  failures still exit 0; they're best-effort). Exit non-zero only if the local
  daemon is unreachable or rejected the payload.
- `themed get` — prints the current blob (compact JSON) to stdout.

The fleet-side nushell (`modules/home/nu/mod/theme.nu`, for your reference — you
don't edit it) will define:
- `theme set light|dark` → builds `{mode: "light"}` and calls `themed set …`.
- `theme reconcile` → **this is `--reconcile-cmd`'s target**. Reads the blob from
  stdin, extracts `.mode`, and if it differs from the last-applied value:
  rewrites `~/.local/state/theme/current` (a bare `light`/`dark` string that
  wezterm and every running nushell already watch) and swaps a Helix config
  symlink + `SIGUSR1`s running `hx`. The exact `--reconcile-cmd` string the
  service will pass looks like:
  ```
  nu --no-config-file -c "$env.NU_LIB_DIRS=['<nix-store-path>']; use theme.nu; theme reconcile"
  ```
  All you must guarantee: **spawn `--reconcile-cmd` via the shell, write the raw
  `blob` bytes to its stdin, wait for it, log non-zero exit.** Run it on every
  adopted change (from `set`, from `announce`, and once at startup if the
  persisted/queried value wasn't already the applied one).

---

## 6. Trust model

**Trust the tailnet.** Bind only to the Tailscale interface address. Any host on
the tailnet may send `announce`/`set`. This matches the posture the etcd setup
used ("access is restricted to the tailnet at the network layer"). No shared
secret, no `tailscale whois` check in v1. If you want to leave a hook for a
future PSK, fine, but don't build it now.

---

## 7. Daemon lifecycle

1. Parse flags. Resolve `--listen` (or discover tailscale IP, retrying with
   backoff until it's available — tailscaled may come up after the user session).
2. Load `--state-file` if present → `(version, blob)`. If absent, `version = 0`
   and `blob` is a small default (`{"mode":"dark"}` is a reasonable built-in
   default, but keep it a single constant that's easy to find).
3. Open the control socket (unlink stale path first).
4. `query` all peers concurrently, short timeout. Among {self, all replies}, take
   the max `version`. If the winner ≠ what's currently applied (track an
   `applied_version` / compare to loaded state), persist it and run the reconcile
   hook once.
5. Enter the accept loop: peer listener + control listener (threads or async —
   your call; `std` threads + a `Mutex<Record>` is completely adequate and keeps
   deps at zero).
6. On `SIGTERM`/`SIGINT`: flush state file, remove the socket, exit 0.

Persistence: rewrite `--state-file` in place on every adopted change. Create
parent dirs. (The brief originally said temp file + rename; that swaps the
inode, which breaks other tools watching the path — see §10.)

Logging: to stderr (journald / launchd log capture it). Default level = state
transitions + errors only. A `-v/--verbose` flag may add per-message tracing.
**No periodic log lines ever** — "too loud" is the whole reason this project
exists.

---

## 8. Suggested repo layout

```
~/proj/themed/
  Cargo.toml         # exists; add deps only if truly needed
  Cargo.lock         # COMMIT THIS (Nix build needs it)
  src/
    main.rs          # arg parse, dispatch daemon vs client subcommands
    daemon.rs        # lifecycle, accept loops, state + persistence
    proto.rs         # message enums, (de)serialization, framing
    peer.rs          # announce / query / re-broadcast, per-peer timeouts
    control.rs       # unix socket server + `set`/`get` client
    reconcile.rs     # spawn --reconcile-cmd, feed blob on stdin
  README.md
```

Keep it well under a few hundred lines. If you reach for tokio, stop and
reconsider — blocking `std::net` + a handful of threads is the right size here.

---

## 9. Tests / acceptance

- Unit: `version` ordering; `announce` with older/equal/newer; malformed line is
  ignored not fatal; state-file round-trips and keeps its inode.
- Integration (spin up 2–3 daemons on `127.0.0.1` with distinct ports + sockets,
  a fake `--reconcile-cmd` that appends stdin to a file):
  - `set` on A → B and C's reconcile files get the blob.
  - Start C *after* a `set` on A → C's startup `query` converges it.
  - Kill A's link to C (use a port C isn't actually listening on for A's peer
    entry, but B has the real one) → B's re-announce still delivers to C.
  - `set` with all peers pointing at dead ports → local reconcile still runs,
    exit 0, errors logged.
- `themed get` reflects the last accepted `set`.

## 10. Open choices — settled

These were decided during implementation; see `README.md` for the user-facing
version.

- **TCP port: `47100`.** Listener bound to the host's Tailscale IPv4.
- **`--listen <addr:port>`, with `tailscale ip -4` as the fallback.** The flag
  wins when present (the fleet passes it); when omitted the daemon shells out to
  `tailscale ip -4` and retries with backoff until tailscaled answers. Binding
  also retries, so starting before the tailnet is up is fine either way.
- **Control socket default: `$XDG_RUNTIME_DIR/themed.sock`**, falling back to
  `std::env::temp_dir()/themed.sock` on macOS. State file default:
  `$XDG_STATE_HOME/themed/state.json` (then `~/.local/state/...`).
- **Threads.** Blocking `std::net`, one thread per connection, a `Mutex<Record>`,
  and a single worker thread that runs reconcile hooks and peer fan-out in
  adoption order.
- **Default blob: `{"mode":"dark"}`** — `daemon::DEFAULT_BLOB`, used only when no
  state file exists yet.
- **`version` is `u64`, not `u128`.** serde cannot carry 128-bit integers through
  an internally-tagged enum (`#[serde(tag = "t")]`), which is how the wire
  messages are modelled. It is nanoseconds since the epoch with a hash of
  `--self` in the low byte, forced past the previous value on a local set. Still
  opaque, still ordered only by `>` / `<=`.
- **The blob is `serde_json::Value`, not `RawValue`.** Same reason — `RawValue`
  cannot be deserialized through a tagged enum. Nothing inspects it; `Value`
  equality is also exactly the comparison update deduplication needs.

### Deviations from the brief worth knowing

- **Update deduplication.** A record that is newer but carries the *same* blob is
  adopted and persisted (the version moves forward) but does **not** run the
  reconcile hook and does **not** re-announce. Only a real theme change gossips.
  A local `set` always announces, even when redundant, since a peer may hold a
  different blob at a lower version.
- **The state file is rewritten in place, not replaced.** A temp file plus
  rename gives a torn-write-free update but swaps the inode, so any tool holding
  an `inotify`/`kqueue` watch on the path silently stops seeing changes. The
  daemon is the file's only writer and every write happens under the state lock,
  so a single truncating write needs no locking; `flock` can be added if an
  outside writer ever appears.
- **Startup always reconciles once.** §7.4 suggested reconciling only if the
  winning record differs from what is applied, but the daemon cannot observe what
  the system had applied before it started. The hook is already required to be
  idempotent, so it runs unconditionally at startup.
- **Re-announce goes to all peers, including the sender.** Excluding the sender
  would mean matching a source IP against resolved peer names for no benefit: the
  sender ignores it, since the version is not greater than its own.
- **Dependencies.** `clap`, `serde`, `serde_json`, `log`, `env_logger`,
  `signal-hook`.

### `ExecStart` contract for `gvc-fleet`

The service is generated by `nix/hm-module.nix` (`homeManagerModules.default`),
not hand-written in the fleet repo. It builds an invocation of the form:

```
themed \
  [--self <hostname>] \
  [--listen <tailscale-ipv4>:47100] \
  [--socket <path>] \            # pinned to /tmp/themed.sock on Darwin
  [--state-file <path>] \
  [--reconcile-cmd '<nushell one-liner>'] \
  --peer <other-host>.coelacanth-byzantine.ts.net:47100 \
  ...
```

Every flag except `--peer` is optional; omitted ones fall back to the daemon's
own defaults (§10). `--listen` is dropped by default — the module puts
`tailscale` on the service `PATH` and the daemon runs `tailscale ip -4`. The CLI
wrapper calls `themed --socket <same path> set '<json>'` / `... get`.

### Flake

The repo is a flake: `packages.<system>.default`, `overlays.default` (adds
`pkgs.themed`), and a dev shell, for `x86_64-linux`, `aarch64-linux`,
`x86_64-darwin`, `aarch64-darwin`. `Cargo.lock` is committed.
