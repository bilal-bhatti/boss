//! A small async-friendly wrapper around `rumqttc`.
//!
//! `rumqttc`'s blocking client owns its own networking; we drive its
//! `Connection` on a dedicated thread and forward incoming publishes onto an
//! `async-channel`, so the async Matter side can consume them on any executor
//! without us pulling in a second async runtime.

use std::time::Duration;

use async_channel::Receiver;
use rumqttc::{Client, Event, MqttOptions, Packet, QoS};

use crate::config::Config;
use crate::error::{Error, Result};

/// An incoming retained/live publish from the broker.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub topic: String,
    pub payload: Vec<u8>,
    pub retain: bool,
}

/// A cheap-to-clone handle for subscribing and publishing.
#[derive(Clone)]
pub struct MqttClient {
    inner: Client,
}

impl MqttClient {
    /// Subscribe to a topic filter at QoS 0.
    pub fn subscribe(&self, topic: &str) -> Result<()> {
        self.inner
            .subscribe(topic, QoS::AtMostOnce)
            .map_err(|e| Error::mqtt(format!("subscribe `{topic}`: {e}")))
    }

    /// Publish a (non-retained) payload at QoS 0.
    pub fn publish(&self, topic: &str, payload: &str) -> Result<()> {
        self.inner
            .publish(topic, QoS::AtMostOnce, false, payload.as_bytes())
            .map_err(|e| Error::mqtt(format!("publish `{topic}`: {e}")))
    }
}

/// Connect to the broker and start the receive loop.
///
/// Returns a client handle for subscribing/publishing, a receiver yielding every
/// incoming publish, and a `connected` receiver that fires once per successful
/// (re)connection (the broker's `ConnAck`). Subscriptions are *session* state
/// and a clean session starts empty, so every `ConnAck` — first connect and each
/// reconnect after a broker restart or network blip — must be followed by a full
/// re-subscribe, or boss goes silent (receives nothing) while still "connected".
///
/// The receive loop runs until the connection handle is dropped; `rumqttc`
/// reconnects on its own across transient broker outages.
pub fn connect(cfg: &Config, client_id: &str) -> Result<MqttConn> {
    let mut opts = MqttOptions::new(client_id, &cfg.mqtt_host, cfg.mqtt_port);
    opts.set_keep_alive(Duration::from_secs(30));
    if let Some(user) = &cfg.mqtt_username {
        opts.set_credentials(user, cfg.mqtt_password.clone().unwrap_or_default());
    }

    let (client, mut connection) = Client::new(opts, 16);
    let (tx, rx) = async_channel::bounded::<Incoming>(256);
    // Coalesced (bounded 1): consumers re-subscribe everything per signal, so one
    // pending notification already covers any bursts.
    let (connected_tx, connected_rx) = async_channel::bounded::<()>(1);

    std::thread::Builder::new()
        .name("mqtt-eventloop".to_owned())
        .spawn(move || {
            for event in connection.iter() {
                match event {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        log::info!("mqtt connected");
                        let _ = connected_tx.try_send(());
                    }
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        let msg = Incoming {
                            topic: p.topic,
                            payload: p.payload.to_vec(),
                            retain: p.retain,
                        };
                        // Blocks (back-pressure) until the async side drains, and
                        // returns `Err` only once the receiver is gone — our cue to stop.
                        if tx.send_blocking(msg).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("mqtt connection error (will retry): {e}");
                        // `rumqttc` retries immediately; without a pause a refused
                        // broker spins this loop hot. Throttle to ~1 attempt/sec.
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
            log::info!("mqtt event loop stopped");
        })
        .map_err(|e| Error::mqtt(format!("spawn mqtt thread: {e}")))?;

    Ok(MqttConn {
        client: MqttClient { inner: client },
        incoming: rx,
        connected: connected_rx,
    })
}

/// The pieces returned by [`connect`].
pub struct MqttConn {
    pub client: MqttClient,
    pub incoming: Receiver<Incoming>,
    /// Fires once per successful (re)connection; drive a full re-subscribe on it.
    pub connected: Receiver<()>,
}
