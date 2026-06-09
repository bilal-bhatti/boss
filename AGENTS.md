# boss

A HomeKit/Matter bridge between ESPHome devices that broadcast Home Assistant
MQTT discovery messages and a Matter controller (Apple Home, etc.).

This is a Rust re-implementation of an earlier Go prototype (`hcbridge`) that used
`github.com/brutella/hc`. The new implementation targets **Matter** via
[`rs-matter`](https://github.com/project-chip/rs-matter), pinned to a git revision
in `Cargo.toml` (a sibling checkout can be used for local development).

## What it does

- Subscribe to Home Assistant MQTT discovery topics (e.g. `homeassistant/switch/#`,
  `homeassistant/sensor/#`).
- Decode discovery payloads into device descriptions.
- Expose each device as a Matter accessory.
- Wire both directions:
  - Matter command → publish to the device's MQTT command topic.
  - MQTT state update → reflect into the Matter cluster attribute.

### Scope (first cut)

- **Switches only.** `rs-matter` ships the `on_off` cluster; there is no
  temperature-measurement cluster, so sensors are out of scope until/unless we
  hand-write one.
- **Dynamic topology.** Devices appear as discovery messages arrive (not a fixed
  startup set), via a runtime-built `Node` + a dispatching `AsyncHandler`.

### Observed message format

Read-only sniff: `mosquitto_sub -h <broker> -t 'homeassistant/#' -v` (do NOT publish).

ESPHome uses **abbreviated** Home Assistant discovery keys — the Go prototype's
full-name structs (`state_topic`, …) do NOT match this broker. Real shape:

- Discovery topic: `homeassistant/switch/<node>/<object>/config`, payload:
  - `name`, `stat_t` (state topic), `cmd_t` (command topic),
    `avty_t` (availability topic), `uniq_id`, and nested `dev`:
    `ids`, `name`, `sw`, `mdl`, `mf`, `cns` (`[[type, value]]`).
- State (`stat_t`) payload: `ON` / `OFF` (retained).
- Availability (`avty_t`) payload: `online` / `offline` → drives Matter
  `reachable` on `bridged_device_basic_information`.
- Command (`cmd_t`): publish `ON` / `OFF`.

Typical devices are ESPHome `esp01_1m` switches.

**Command testing:** when testing the Matter→device path against real hardware,
use a non-critical device, read its current state first, and restore it after.
A switch's command topic accepts `ON`/`OFF` and the device echoes the new state
on its state topic.

## Coding guidelines

- **Seek correct and complete solutions** — no half-measures, no TODO-shaped holes
  where real handling belongs.
- **No silent failure modes** — surface errors. Don't swallow, don't `unwrap()` away
  real fallibility, don't paper over with defaults. Use `Result` and propagate.
- **Prefer monotone code** — one obvious way through; minimise branching and special
  cases. Straight-line code over clever control flow.
- **Don't over-abstract** — no traits, generics, or layers until a second concrete
  caller demands them. Wire existing pieces together before inventing new ones.
- **Small files, organized well** — one clear responsibility per file/module.
- **Clean idiomatic Rust** — follow community idioms; `cargo fmt` + `cargo clippy` clean.
- **Simple and readable over clever and opaque** — optimise for the next reader.
- **Only very high-value tests** — test the things that are easy to get wrong and
  costly when they break (e.g. discovery payload parsing against real captured
  bytes, topic routing). No tests that merely restate the implementation, no
  coverage-chasing, no testing the compiler or third-party libs.

## Extensibility (device types)

New device types (sensors, etc.) will be added later. Adding one MUST be:
implement a single trait + register it — no edits scattered across the bridge.

- One trait, e.g. `BridgedDevice`, captures everything type-specific:
  - `component()` — the HA discovery component it handles (`"switch"`, `"sensor"`, …).
  - `from_discovery(payload)` — parse its discovery JSON into the typed device.
  - `endpoint(id)` — the Matter endpoint description (device type(s) + clusters).
  - `into_handler(mqtt)` — produce the (type-erased) per-device handler + its
    background MQTT task.
- A registry maps `component → factory`. Adding a type = `impl BridgedDevice` +
  one registration line. `Switch` is the first and (for now) only impl.
- This is the one deliberate abstraction. Keep it thin — resist adding further
  layers until a second concrete device type actually needs them.

## Bridge core architecture (decision record)

Dynamic device count, but implemented as a **fixed-capacity slot pool**, not a
growing `Vec` of handlers. Reason: `rs-matter` holds `&` references into the
handler for the lifetime of its run loop; structurally mutating a `Vec`
(push/realloc) while those borrows are live is not expressible in safe Rust.

- `MAX_DEVICES` fixed slots (e.g. 16). Endpoint 0 = root, 1 = aggregator,
  device endpoints = `2 .. 2 + MAX_DEVICES`.
- All handler objects live in fixed arrays inside the `Bridge` struct — the
  structure never changes. **Every runtime change goes through interior
  mutability** (a slot's MQTT command topic, unique id, reachability, on/off
  state). No `Box`, no realloc, no cross-mutation borrows.
- `Metadata::access` builds the `Node` fresh each call from the currently
  *active* slots (hot-swappable node is explicitly supported by rs-matter —
  see its `SwappableMetadata` test). Inactive slots are simply absent from the
  node, so Matter never routes operations to them.
- `AsyncHandler` dispatches read/write/invoke/bump_dataver by `ctx.endpt()`:
  endpoint 0/1 → root/aggregator handlers; `2..` → the slot at that index.
- `run()` drives the root handler plus all `MAX_DEVICES` slot run-futures with a
  fixed `select`/`join`. A slot's on/off `run` simply awaits its state channel,
  so an inactive slot parks harmlessly until activated.
- Activating a slot (on discovery): fill its interior fields, mark it active,
  and signal `run` to push an aggregator `PartsList` change
  (`notify_attr_changed`) so controllers re-read and see the new device at once.

## Running

On start it prints a Matter QR + pairing code and opens a commissioning window.
See `--help` for flags.

The mDNS backend is a **Cargo feature** (`src/mdns.rs`), so the same source runs
on both macOS (dev) and the Linux deploy target — pick one:

- **macOS dev**: `cargo run --features astro-dnssd -- --mqtt-host <broker>`
  Uses Bonjour (system `dns_sd`, no extra install) and is fully commissionable
  locally. Without this feature the built-in responder collides with macOS's
  system `mDNSResponder` on UDP 5353.
- **Linux deploy**: `cargo run --features avahi` (avahi over D-Bus), or the
  default built-in raw-socket responder when no system responder owns 5353.

mDNS is a hard requirement: `main` treats an mDNS failure as **fatal** — it logs
the cause and exits, so systemd restarts the service (the unit `Wants`/`After`
avahi and disables the start rate-limiter, so it retries until mDNS is back)
rather than running on as an undiscoverable zombie.

### Other notes / gotchas

- **Interface selection** (built-in responder only) picks the first interface
  with a link-local IPv6 + IPv4. On a dev Mac this can wrongly pick `bridge100`/VM
  bridges instead of the real LAN (`en0`). Not an issue with astro/avahi.
- Verified end-to-end against a live broker: switches bridge to Matter endpoints
  `2/3/4` from retained discovery; `astro-dnssd` advertises the node as
  Commissionable; the command path drives a real switch.

## Deploy (Incus container `boss`, Debian x86_64)

Cross-compiled on macOS to a static musl binary — **no toolchain in the
container, no system C cross-toolchain on the Mac**:

- `.cargo/config.toml` links the musl target with bundled `rust-lld`.
- `rumqttc` has `default-features = false` (plain TCP) to drop `ring`, keeping
  the dep graph pure-Rust.
- `rustup target add x86_64-unknown-linux-musl` (one-time).

Build + push:
```
cargo build --release --target x86_64-unknown-linux-musl
incus exec boss -- systemctl stop boss            # avoid "text file busy"
incus file push target/x86_64-unknown-linux-musl/release/boss boss/usr/local/bin/boss --mode 0755
incus file push deploy/boss.env boss/etc/default/boss   # config (EnvironmentFile)
incus file push deploy/boss.service boss/etc/systemd/system/boss.service
incus exec boss -- systemctl daemon-reload
incus exec boss -- systemctl enable --now boss
incus exec boss -- journalctl -u boss -f
```

- Deployed with the **`avahi`** feature: boss publishes Matter mDNS via
  `avahi-daemon` over D-Bus (so avahi owns 5353 and the container is
  `boss.local`-discoverable). The unit `Wants`/`After` `avahi-daemon.service`.
- Unit runs as **root** (not `DynamicUser`): the avahi backend authenticates to
  the system D-Bus, which a transient `DynamicUser` id cannot do. With avahi,
  boss binds no privileged port (avahi owns 5353; boss uses Matter UDP 5540), so
  no capabilities are needed — root is only about D-Bus access.
- `StateDirectory=boss` → `/var/lib/boss` (survives reboots). Switching off
  `DynamicUser` made systemd migrate state out of `/var/lib/private/boss`.
- Runtime config lives in `/etc/default/boss` (from `deploy/boss.env`), loaded by
  the unit's `EnvironmentFile=`; systemd expands the `BOSS_*` vars into boss's
  CLI flags in `ExecStart`, so the binary stays plain flags-only (no env-parsing).
- Broker addressed by **IP** (a `.local` broker name may not resolve without nss-mdns).
- The IPv4-only concern of the built-in backend does not apply here — avahi
  handles advertising. (Built-in backend stays available via default features
  for hosts without avahi.)

## Layout

- `../rs-matter` — Matter implementation (sibling, vendored/checked-out).
- `hcbridge` — the original Go prototype (HomeKit bridge via `brutella/hc`), for reference.
