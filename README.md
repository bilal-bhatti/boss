# boss

A Matter bridge for Home Assistant MQTT-discovery devices (ESPHome switches).
It subscribes to Home Assistant discovery topics, and exposes each discovered
device as a Matter accessory so they appear in Apple Home / any Matter
controller. Both directions are wired: controller commands publish to the
device's MQTT command topic, and MQTT state updates reflect back into Matter.

Built on [`rs-matter`](https://github.com/project-chip/matter-rs).

- **Build & run**: see [`DEPLOYMENT.md`](./DEPLOYMENT.md).
- **Design & internals**: see [`AGENTS.md`](./AGENTS.md).

## Quick start

```sh
# dev (macOS), with Bonjour mDNS so it's commissionable locally:
cargo run --features astro-dnssd -- --mqtt-host <broker-host>

# build the deployable static Linux binary:
rustup target add x86_64-unknown-linux-musl   # one-time
cargo build --release --target x86_64-unknown-linux-musl
```

On start it prints a Matter QR + pairing code and opens a commissioning window.

## Status

Switches (On/Off) over a dynamic, fixed-capacity device pool. Sensors and other
device types are a planned extension (one trait impl + registration). Deployed
and commissioned into Apple Home from an Incus container — see `DEPLOYMENT.md`.
