# Deploying `boss`

`boss` bridges Home Assistant MQTT-discovery devices (ESPHome switches) into a
Matter fabric so they appear in Apple Home / any Matter controller.

This document covers building, deploying to the Incus container, updating, and
troubleshooting. For design/architecture see [`AGENTS.md`](./AGENTS.md).

---

## 1. Topology

```
ESPHome switches ──MQTT──▶  broker (relay, 192.0.2.10)
                                  │
                                  ▼  homeassistant/switch/# (discovery)
                           boss container (192.0.2.20)  ──Matter/mDNS──▶  Apple Home hub
```

- Broker: `relay` Incus container, `192.0.2.10:1883`, plain TCP.
- Bridge: `boss` Incus container, Debian x86_64, bridged `eth0` on the LAN.
- Both share the LAN so the Matter node is L2-reachable by the Home hub.

---

## 2. Prerequisites (build host — macOS)

- Rust toolchain via `rustup`.
- The static-musl Linux target (one-time):
  ```
  rustup target add x86_64-unknown-linux-musl
  ```
- `incus` CLI configured against the host running the containers.

No C cross-toolchain is required and nothing needs installing inside the
container: the binary is fully static and links with rust's bundled `rust-lld`
(see [`.cargo/config.toml`](./.cargo/config.toml)). `rumqttc` is built without
TLS (plain TCP), which keeps the dependency graph pure-Rust.

---

## 3. Build

The container runs `avahi-daemon`, so build with the **`avahi`** feature: boss
publishes its Matter mDNS records via avahi over D-Bus rather than binding UDP
5353 itself (which would collide with avahi).

```
cargo build --release --target x86_64-unknown-linux-musl --features avahi
```

Output: `target/x86_64-unknown-linux-musl/release/boss` — a static `static-pie`
ELF, no runtime dependencies. (The default/no-feature build uses the built-in
raw-socket responder instead — only suitable where nothing else owns 5353. See
`AGENTS.md` for `astro-dnssd`, the macOS-dev backend.)

---

## 4. First deploy

Prerequisite — the container needs avahi + D-Bus running:
```
incus exec boss -- apt-get install -y avahi-daemon dbus
```

```
# binary
incus file push target/x86_64-unknown-linux-musl/release/boss \
    boss/usr/local/bin/boss --mode 0755 --create-dirs

# systemd unit
incus file push deploy/boss.service boss/etc/systemd/system/boss.service

# config — ONE-TIME ONLY. deploy/boss.env is a template with a placeholder
# broker IP; the real IP is environment-specific and lives only on the
# container. Push it once, then set the real values in place (below). Do NOT
# re-push it on updates or you'll clobber the real config.
incus file push deploy/boss.env boss/etc/default/boss
incus exec boss -- sed -i 's/192\.0\.2\.10/<real-broker-ip>/' /etc/default/boss

# enable + start (also starts on container boot)
incus exec boss -- systemctl daemon-reload
incus exec boss -- systemctl enable --now boss

# watch it come up
incus exec boss -- journalctl -u boss -f
```

A healthy start logs: MQTT subscribed → loaded fabrics (if already paired) →
`bridged switch ... -> endpoint N` for each device → `Running Matter transport`
→ avahi `Avahi API version: ...` and `Registering mDNS service: ...`. (On first
run, before pairing, it prints a Matter QR + `PairingCode` instead of fabrics.)

---

## 5. Update an existing deployment

The binary is in use while running, so stop first to avoid `text file busy`:

```
cargo build --release --target x86_64-unknown-linux-musl --features avahi
incus exec boss -- systemctl stop boss
incus file push target/x86_64-unknown-linux-musl/release/boss boss/usr/local/bin/boss --mode 0755
incus exec boss -- systemctl start boss
```

Commissioning state in `/var/lib/boss` survives updates, so the device stays
paired across binary upgrades. The config at `/etc/default/boss` is **not**
re-pushed (it holds the real, environment-specific broker IP) — leave it alone.
Push the unit too only if `deploy/boss.service` changed (then `daemon-reload`).

---

## 6. Configuration

Runtime config lives in an environment file, `/etc/default/boss`, which the unit
loads via `EnvironmentFile=`. The unit expands those vars into boss's flags in
`ExecStart`, so the binary stays plain flags-only — edit the env file, not the
unit. Run `boss --help` for every flag. The vars:

| Variable | Flag | Value | Why |
|----------|------|-------|-----|
| `BOSS_MQTT_HOST` | `--mqtt-host` | _(your broker IP)_ | `nss-mdns` isn't set up for resolution, so a `.local` broker name may not resolve — use an IP. |
| `BOSS_MQTT_PORT` | `--mqtt-port` | `1883` | Broker port. |
| `BOSS_STATE_DIR` | `--state-dir` | `/var/lib/boss` | Persisted Matter state (provided by systemd `StateDirectory=boss`). |
| `BOSS_HTTP_PORT` | `--http-port` | `80` | Embedded status page (all interfaces). Port 80 needs root, which the unit already is. |
| `RUST_LOG` | — | `info` | Log level (passed as process env, not a flag). |

`deploy/boss.env` in the repo is a **template** with a placeholder broker IP
(`192.0.2.10`, RFC5737 — deliberately non-routable so a forgotten edit fails
loudly rather than hitting a real host). The real IP is environment-specific and
lives only on the container; **edit `/etc/default/boss` in place there** — never
commit it or re-push the template over it. Every var referenced in `ExecStart`
must be present: an unset `${VAR}` expands to an empty argument. `EnvironmentFile=`
has no leading `-`, so a missing file fails the unit loudly.

The unit runs as **root** and `Wants`/`After` `avahi-daemon.service`. Root is
required because the avahi mDNS backend authenticates to the system D-Bus, which
a transient `DynamicUser` id cannot do (see Troubleshooting). boss binds no
privileged port directly — avahi owns 5353; boss uses Matter UDP 5540.

To change the config, edit the live file in place on the container, then restart:
```
incus exec boss -- vi /etc/default/boss      # or: sed -i 's/old/new/' ...
incus exec boss -- systemctl restart boss
```
Editing the unit (`deploy/boss.service`) instead is a normal push and needs
`systemctl daemon-reload` before `restart`.

---

## 7. Commissioning into Apple Home

1. Get the pairing code from the logs (`PairingCode: [....-....-...]`) or scan
   the printed QR.
2. Apple Home → Add Accessory → More options → enter the code.
3. It is an uncertified (test-attestation) device — accept the "uncertified
   accessory" warning.
4. The three switches appear under the bridge.

The pairing code is fixed (derived from the built-in test commissioning data);
it does not change across restarts.

---

## 8. Operations

```
incus exec boss -- systemctl status boss      # state
incus exec boss -- journalctl -u boss -f       # follow logs
incus exec boss -- systemctl restart boss      # restart
incus exec boss -- systemctl stop boss         # stop
```

To re-commission from scratch (new fabric/QR), clear the state dir:
```
incus exec boss -- systemctl stop boss
incus exec boss -- rm -rf /var/lib/boss/*
incus exec boss -- systemctl start boss
```

---

## 9. Troubleshooting

- **`mDNS responder unavailable (StdIoError)` with the avahi build** — boss
  couldn't reach avahi over the system D-Bus. Causes: avahi-daemon/dbus not
  running (`systemctl is-active avahi-daemon dbus`), or the service runs as a
  `DynamicUser` (a transient uid cannot authenticate to the system bus — run as
  root, as the unit does). Verify by running the binary manually as root:
  `incus exec boss -- bash -c 'set -a; . /etc/default/boss; exec /usr/local/bin/boss --mqtt-host $BOSS_MQTT_HOST --state-dir $BOSS_STATE_DIR'`
  — a healthy run logs `Avahi API version` + `Registering mDNS service`.
- **Apple Home spins on "connecting"** — the controller can't reach the Matter
  node. With the built-in mDNS backend on an IPv4-only host it can be an IPv6
  issue (Matter prefers IPv6). With the avahi backend this is handled by
  avahi/the host; if it persists, confirm avahi is advertising
  (`avahi-browse -atp | grep -i matter`).
- **No devices bridged** — verify broker reachability and that discovery is
  retained: `mosquitto_sub -h 192.0.2.10 -t 'homeassistant/switch/#' -v`.
- **`text file busy` on push** — stop the service before pushing the binary.
- **`Address already in use` on mDNS** (built-in backend only) — something owns
  UDP 5353 (avahi, or macOS `mDNSResponder`); use the `avahi` backend, or free
  5353.
- **Service keeps restarting** — mDNS is a hard requirement, so an mDNS failure
  is fatal and the process exits (systemd restarts it; the start rate-limiter is
  disabled via `StartLimitIntervalSec=0` so it retries until mDNS works). Check
  the logs for the underlying cause (usually avahi/D-Bus, see the first entry).

---

## 10. Known limitations

- `StartUpOnOff` is in-memory only (not persisted across restarts).
- Switches only; sensors/other device types are a future addition (one
  `BridgedDevice`-style impl + registration — see `AGENTS.md`).
