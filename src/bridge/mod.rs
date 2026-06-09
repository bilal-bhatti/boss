//! The Matter bridge: a fixed-capacity pool of device slots exposed as one
//! Matter data-model handler.
//!
//! See AGENTS.md ("Bridge core architecture") for why this is a fixed pool with
//! interior mutability rather than a growing `Vec` of handlers.

pub mod bridged;
pub mod node;
pub mod switch;

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use async_channel::{Receiver, Sender};
use futures::future::try_join_all;
use rand::RngCore;

use rs_matter::dm::clusters::app::on_off::{
    self, ClusterAsyncHandler as _, NoLevelControl, OnOffHandler, OnOffHooks,
};
use rs_matter::dm::clusters::decl::bridged_device_basic_information::ClusterHandler as _;
use rs_matter::dm::clusters::decl::{
    bridged_device_basic_information, descriptor, groups as groups_decl,
};
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _, DescHandler};
use rs_matter::dm::clusters::groups::{ClusterHandler as _, GroupsHandler};
use rs_matter::dm::{
    Async, AsyncHandler, AttrId, ClusterId, Dataver, EndptId, HandlerContext, InvokeContext,
    InvokeReply, MatchContext, Matcher, Metadata, Node, ReadContext, ReadReply, WriteContext,
};
use rs_matter::error::{Error, ErrorCode};

use crate::bridge::bridged::BridgedHandler;
use crate::bridge::node::{device_endpoint, AGGREGATOR_ENDPOINT, MAX_DEVICES};
use crate::bridge::switch::{Activation, MqttOnOff};
use crate::discovery::SwitchConfig;
use crate::mqtt::MqttClient;

// Cluster ids served on a bridged switch endpoint.
const DESC_ID: ClusterId = desc::DescHandler::CLUSTER.id;
const GROUPS_ID: ClusterId = GroupsHandler::CLUSTER.id;
const BRIDGED_ID: ClusterId = BridgedHandler::CLUSTER.id;
const ONOFF_ID: ClusterId = <MqttOnOff as OnOffHooks>::CLUSTER.id;

// The aggregator (endpoint 1) descriptor's PartsList enumerates the bridged
// endpoints; notifying a change to it makes controllers pick up new devices
// without waiting for their own re-read.
const PARTS_LIST_ATTR: AttrId = descriptor::AttributeId::PartsList as AttrId;

/// A read-only snapshot of one bridged device, rendered by the status page.
pub struct DeviceView {
    pub endpoint: EndptId,
    pub name: String,
    pub unique_id: String,
    pub reachable: bool,
    pub on: bool,
}

/// One device slot. The struct shape is fixed for the bridge's lifetime; every
/// runtime change goes through interior mutability (the handlers' own cells,
/// the shared [`Activation`], and `active`).
struct Slot {
    desc: DescHandler<'static>,
    groups: GroupsHandler,
    bridged: BridgedHandler,
    on_off: OnOffHandler<'static, MqttOnOff, NoLevelControl>,
    /// Shared command-topic binding (also held by `on_off`'s hooks).
    activation: std::sync::Arc<Activation>,
    /// Feeds device-reported state into `on_off`'s `run` loop.
    state_tx: Sender<bool>,
    active: AtomicBool,
}

impl Slot {
    fn new(index: usize, client: MqttClient, rand: &mut impl RngCore) -> Self {
        let (state_tx, state_rx) = async_channel::bounded(8);
        let activation = Activation::new(client);
        let hooks = MqttOnOff::new(activation.clone(), state_rx);

        Slot {
            desc: DescHandler::new(Dataver::new_rand(&mut *rand)),
            groups: GroupsHandler::new(Dataver::new_rand(&mut *rand)),
            bridged: BridgedHandler::new(Dataver::new_rand(&mut *rand)),
            on_off: OnOffHandler::new_standalone(
                Dataver::new_rand(&mut *rand),
                device_endpoint(index),
                hooks,
            ),
            activation,
            state_tx,
            active: AtomicBool::new(false),
        }
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

/// Routing tables from MQTT topics to slot indices.
#[derive(Default)]
struct Routes {
    by_state_topic: HashMap<String, usize>,
    by_avty_topic: HashMap<String, usize>,
    by_unique_id: HashMap<String, usize>,
}

/// The bridge: a fixed pool of device slots plus MQTT routing.
pub struct Bridge {
    slots: Vec<Slot>,
    client: MqttClient,
    routes: Mutex<Routes>,
    /// Signalled when a device is added, so `run` can push a PartsList report.
    /// Bounded at 1 — coalesced bursts still result in one re-read.
    added_tx: Sender<()>,
    added_rx: Receiver<()>,
}

impl Bridge {
    pub fn new(client: MqttClient, rand: &mut impl RngCore) -> Self {
        let mut slots = Vec::with_capacity(MAX_DEVICES);
        for i in 0..MAX_DEVICES {
            slots.push(Slot::new(i, client.clone(), rand));
        }
        let (added_tx, added_rx) = async_channel::bounded(1);
        Bridge {
            slots,
            client,
            routes: Mutex::new(Routes::default()),
            added_tx,
            added_rx,
        }
    }

    /// Map an endpoint id to a device slot index, if it is one.
    fn slot_for(&self, endpoint: Option<EndptId>) -> Option<usize> {
        let ep = endpoint?;
        let index = ep.checked_sub(node::FIRST_DEVICE_ENDPOINT)? as usize;
        (index < self.slots.len()).then_some(index)
    }

    fn routes(&self) -> std::sync::MutexGuard<'_, Routes> {
        self.routes.lock().expect("routes lock poisoned")
    }

    /// Bind a discovered switch to a free slot and subscribe to its topics.
    /// Idempotent per `unique_id`: re-announcements are ignored.
    pub fn add_switch(&self, cfg: &SwitchConfig) -> crate::error::Result<()> {
        let mut routes = self.routes();
        if routes.by_unique_id.contains_key(&cfg.unique_id) {
            log::debug!("switch `{}` already bridged; ignoring", cfg.name);
            return Ok(());
        }

        let Some(index) = (0..self.slots.len()).find(|i| !self.slots[*i].is_active()) else {
            return Err(crate::error::Error::config(format!(
                "device pool full ({MAX_DEVICES}); cannot bridge `{}`",
                cfg.name
            )));
        };

        let slot = &self.slots[index];
        slot.activation.bind(cfg.command_topic.clone());
        slot.bridged
            .activate(cfg.unique_id.clone(), cfg.name.clone());
        slot.active.store(true, Ordering::Release);

        routes.by_state_topic.insert(cfg.state_topic.clone(), index);
        routes.by_unique_id.insert(cfg.unique_id.clone(), index);
        self.client.subscribe(&cfg.state_topic)?;
        if let Some(avty) = &cfg.availability_topic {
            routes.by_avty_topic.insert(avty.clone(), index);
            self.client.subscribe(avty)?;
        }
        drop(routes);

        // Ask `run` to push an aggregator PartsList report so controllers pick
        // up the new endpoint immediately. Bounded(1): if one is already
        // pending, a single re-read still reflects all adds since.
        let _ = self.added_tx.try_send(());

        log::info!(
            "bridged switch `{}` -> endpoint {}",
            cfg.name,
            device_endpoint(index)
        );
        Ok(())
    }

    /// A read-only view of every currently bridged device, for the status page.
    pub fn device_views(&self) -> Vec<DeviceView> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_active())
            .map(|(i, s)| {
                let (unique_id, name, reachable) = s.bridged.snapshot();
                DeviceView {
                    endpoint: device_endpoint(i),
                    name,
                    unique_id,
                    reachable,
                    on: s.on_off.on_off(),
                }
            })
            .collect()
    }

    /// Route a non-discovery MQTT message to the right slot: a state update or
    /// an availability change.
    pub async fn deliver(&self, topic: &str, payload: &[u8]) {
        let (state_slot, avty_slot) = {
            let routes = self.routes();
            (
                routes.by_state_topic.get(topic).copied(),
                routes.by_avty_topic.get(topic).copied(),
            )
        };

        if let Some(i) = state_slot {
            let on = payload == b"ON";
            // Bounded channel; if the consumer is keeping up this never blocks.
            if self.slots[i].state_tx.send(on).await.is_err() {
                log::warn!("slot {i} state channel closed");
            }
        } else if let Some(i) = avty_slot {
            let online = payload == b"online";
            self.slots[i].bridged.set_reachable(online);
        }
    }
}

impl Metadata for Bridge {
    fn access<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Node<'_>) -> R,
    {
        let active = (0..self.slots.len()).filter(|i| self.slots[*i].is_active());
        let mut buf = Vec::new();
        node::write_endpoints(active, &mut buf);
        node::with_node(&buf, f)
    }
}

impl AsyncHandler for Bridge {
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        let (Some(index), cluster) = (self.slot_for(ctx.endpt()), ctx.cluster()) else {
            return Err(ErrorCode::EndpointNotFound.into());
        };
        let slot = &self.slots[index];
        if !slot.is_active() {
            return Err(ErrorCode::EndpointNotFound.into());
        }

        match cluster {
            Some(DESC_ID) => {
                Async(descriptor::HandlerAdaptor(&slot.desc))
                    .read(ctx, reply)
                    .await
            }
            Some(GROUPS_ID) => {
                Async(groups_decl::HandlerAdaptor(&slot.groups))
                    .read(ctx, reply)
                    .await
            }
            Some(BRIDGED_ID) => {
                Async(bridged_device_basic_information::HandlerAdaptor(
                    &slot.bridged,
                ))
                .read(ctx, reply)
                .await
            }
            Some(ONOFF_ID) => {
                on_off::HandlerAsyncAdaptor(&slot.on_off)
                    .read(ctx, reply)
                    .await
            }
            _ => Err(ErrorCode::AttributeNotFound.into()),
        }
    }

    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        let (Some(index), cluster) = (self.slot_for(ctx.endpt()), ctx.cluster()) else {
            return Err(ErrorCode::EndpointNotFound.into());
        };
        let slot = &self.slots[index];
        if !slot.is_active() {
            return Err(ErrorCode::EndpointNotFound.into());
        }

        match cluster {
            Some(BRIDGED_ID) => {
                Async(bridged_device_basic_information::HandlerAdaptor(
                    &slot.bridged,
                ))
                .write(ctx)
                .await
            }
            Some(ONOFF_ID) => on_off::HandlerAsyncAdaptor(&slot.on_off).write(ctx).await,
            _ => Err(ErrorCode::AttributeNotFound.into()),
        }
    }

    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        let (Some(index), cluster) = (self.slot_for(ctx.endpt()), ctx.cluster()) else {
            return Err(ErrorCode::EndpointNotFound.into());
        };
        let slot = &self.slots[index];
        if !slot.is_active() {
            return Err(ErrorCode::EndpointNotFound.into());
        }

        match cluster {
            Some(GROUPS_ID) => {
                Async(groups_decl::HandlerAdaptor(&slot.groups))
                    .invoke(ctx, reply)
                    .await
            }
            Some(ONOFF_ID) => {
                on_off::HandlerAsyncAdaptor(&slot.on_off)
                    .invoke(ctx, reply)
                    .await
            }
            _ => Err(ErrorCode::CommandNotFound.into()),
        }
    }

    fn bump_dataver(&self, ctx: impl MatchContext) {
        let Some(index) = self.slot_for(ctx.endpt()) else {
            return;
        };
        let slot = &self.slots[index];
        match ctx.cluster() {
            Some(DESC_ID) => Async(descriptor::HandlerAdaptor(&slot.desc)).bump_dataver(ctx),
            Some(GROUPS_ID) => Async(groups_decl::HandlerAdaptor(&slot.groups)).bump_dataver(ctx),
            Some(BRIDGED_ID) => Async(bridged_device_basic_information::HandlerAdaptor(
                &slot.bridged,
            ))
            .bump_dataver(ctx),
            Some(ONOFF_ID) => on_off::HandlerAsyncAdaptor(&slot.on_off).bump_dataver(ctx),
            _ => {}
        }
    }

    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        // Drive every slot's on/off state machine + hooks concurrently. Inactive
        // slots simply park on their (empty) state channel.
        let run_slots = async {
            let futures: Vec<BoxedRun<'_>> = self
                .slots
                .iter()
                .map(|s| boxed_run(&s.on_off, &ctx))
                .collect();
            try_join_all(futures).await.map(|_| ())
        };

        futures::future::try_join(run_slots, self.notify_added(&ctx))
            .await
            .map(|_| ())
    }
}

impl Bridge {
    /// Push an aggregator PartsList report whenever a device is added, so
    /// controllers re-read and discover the new endpoint without delay.
    async fn notify_added(&self, ctx: &impl HandlerContext) -> Result<(), Error> {
        loop {
            if self.added_rx.recv().await.is_err() {
                // Sender dropped; nothing more will arrive, but we must not return.
                core::future::pending::<()>().await;
            }
            ctx.notify_attr_changed(AGGREGATOR_ENDPOINT, DESC_ID, PARTS_LIST_ATTR);
            log::debug!("pushed aggregator PartsList change");
        }
    }
}

type BoxedRun<'a> = Pin<Box<dyn core::future::Future<Output = Result<(), Error>> + 'a>>;

fn boxed_run<'a>(
    on_off: &'a OnOffHandler<'static, MqttOnOff, NoLevelControl>,
    ctx: &'a impl HandlerContext,
) -> BoxedRun<'a> {
    // Call the cluster handler's background task directly: wrapping it in a
    // temporary `HandlerAsyncAdaptor` would have the returned future borrow that
    // temporary.
    Box::pin(on_off.run(ctx))
}

/// Matches the root/aggregator endpoints (0 and 1) and global operations, so
/// the top-level chain routes those to the system handler and everything else
/// to the bridge's slot pool.
#[derive(Clone, Copy)]
pub struct RootEndpoints;

impl Matcher for RootEndpoints {
    fn matches(&self, ctx: impl MatchContext) -> bool {
        match ctx.endpt() {
            None => true,
            Some(ep) => ep <= AGGREGATOR_ENDPOINT,
        }
    }
}
