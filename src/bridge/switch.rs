//! The `switch` device type: an `OnOffHooks` implementation backed by MQTT.
//!
//! Two directions, kept strictly separate so they can't loop:
//! - **Matter → device**: the cluster calls [`MqttOnOff::set_on_off`] when a
//!   controller flips the switch; we publish `ON`/`OFF` to the command topic.
//! - **device → Matter**: state-topic updates arrive on `state_rx`; the `run`
//!   loop applies them *without* re-publishing and signals the cluster to
//!   re-read via `OutOfBandMessage::Update`.
//!
//! The command topic lives behind a shared [`Activation`] so a pooled slot can
//! be bound to a discovered device at runtime without reaching inside the
//! owned `OnOffHandler`.

use std::sync::{Arc, Mutex};

use async_channel::Receiver;

use rs_matter::dm::clusters::app::on_off::{
    EffectVariantEnum, OnOffHooks, OutOfBandMessage, StartUpOnOffEnum,
};
use rs_matter::dm::clusters::decl::on_off::{self, AttributeId, CommandId, Feature};
use rs_matter::dm::Cluster;
use rs_matter::error::Error;
use rs_matter::tlv::Nullable;
use rs_matter::with;

use crate::mqtt::MqttClient;

/// The runtime binding of a switch slot to a physical device: the MQTT command
/// topic to publish to. Shared (`Arc`) between the device's [`MqttOnOff`] hooks
/// and the bridge that activates the slot.
pub struct Activation {
    client: MqttClient,
    command_topic: Mutex<Option<String>>,
}

impl Activation {
    pub fn new(client: MqttClient) -> Arc<Self> {
        Arc::new(Self {
            client,
            command_topic: Mutex::new(None),
        })
    }

    /// Bind the slot to a device's command topic.
    pub fn bind(&self, command_topic: String) {
        *self.lock() = Some(command_topic);
    }

    fn publish(&self, on: bool) {
        let payload = if on { "ON" } else { "OFF" };
        match self.lock().as_deref() {
            Some(topic) => {
                if let Err(e) = self.client.publish(topic, payload) {
                    log::error!("failed to command `{topic}`: {e}");
                }
            }
            None => log::warn!("on/off command before slot activation; ignoring"),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.command_topic.lock().expect("activation lock poisoned")
    }
}

/// Device-specific on/off logic for one bridged switch.
pub struct MqttOnOff {
    activation: Arc<Activation>,
    /// Receives the device's reported state (`true` = on) from the bridge.
    state_rx: Receiver<bool>,
    state: Mutex<HooksState>,
}

#[derive(Default)]
struct HooksState {
    on: bool,
    start_up: Option<StartUpOnOffEnum>,
}

impl MqttOnOff {
    pub fn new(activation: Arc<Activation>, state_rx: Receiver<bool>) -> Self {
        Self {
            activation,
            state_rx,
            state: Mutex::new(HooksState::default()),
        }
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut HooksState) -> R) -> R {
        f(&mut self.state.lock().expect("on/off state lock poisoned"))
    }
}

impl OnOffHooks for MqttOnOff {
    // On/Off Light requires the LIGHTING feature, which gates the
    // GlobalSceneControl/OnTime/OffWaitTime/StartUpOnOff attributes and the
    // OffWithEffect/OnWithRecallGlobalScene/OnWithTimedOff commands. The library
    // `OnOffHandler` implements them all; we just opt in via the metadata.
    const CLUSTER: Cluster<'static> = on_off::FULL_CLUSTER
        .with_revision(6)
        .with_features(Feature::LIGHTING.bits())
        .with_attrs(with!(
            required;
            AttributeId::OnOff
                | AttributeId::GlobalSceneControl
                | AttributeId::OnTime
                | AttributeId::OffWaitTime
                | AttributeId::StartUpOnOff
        ))
        .with_cmds(with!(
            CommandId::Off
                | CommandId::On
                | CommandId::Toggle
                | CommandId::OffWithEffect
                | CommandId::OnWithRecallGlobalScene
                | CommandId::OnWithTimedOff
        ));

    fn on_off(&self) -> bool {
        self.with_state(|s| s.on)
    }

    fn set_on_off(&self, on: bool) {
        // Matter → device. Record locally, then command the physical device.
        self.with_state(|s| s.on = on);
        self.activation.publish(on);
    }

    fn start_up_on_off(&self) -> Nullable<StartUpOnOffEnum> {
        match self.with_state(|s| s.start_up) {
            Some(v) => Nullable::some(v),
            None => Nullable::none(),
        }
    }

    fn set_start_up_on_off(&self, value: Nullable<StartUpOnOffEnum>) -> Result<(), Error> {
        // NOTE: in-memory only; not yet persisted across reboots (a known gap,
        // not a silent one — see AGENTS.md).
        self.with_state(|s| s.start_up = value.into_option());
        Ok(())
    }

    async fn handle_off_with_effect(&self, _effect: EffectVariantEnum) {
        // A plain switch has no dimming/fade effect to perform.
    }

    async fn run<F: Fn(OutOfBandMessage)>(&self, notify: F) {
        // device → Matter. Apply reported state locally (no publish) and ask the
        // cluster to re-read it. Must never return.
        loop {
            match self.state_rx.recv().await {
                Ok(on) => {
                    self.with_state(|s| s.on = on);
                    notify(OutOfBandMessage::Update);
                }
                Err(_) => {
                    // Sender dropped: nothing more will arrive, but `run` must
                    // not return. Park forever.
                    core::future::pending::<()>().await;
                }
            }
        }
    }
}
