//! The Bridged Device Basic Information cluster for one bridged endpoint.
//!
//! Reports the device behind the endpoint (its unique id, label, reachability).
//! Unlike the rs-matter example, `unique_id` is implemented for real and
//! reachability is driven from the device's MQTT availability topic. All fields
//! are interior-mutable so a slot can be (re)activated at runtime.

use std::sync::Mutex;

use rs_matter::dm::clusters::decl::bridged_device_basic_information::{
    AttributeId, ClusterHandler, HandlerAdaptor, KeepActiveRequest,
};
use rs_matter::dm::{Cluster, Dataver, InvokeContext, ReadContext, WriteContext};
use rs_matter::error::Error;
use rs_matter::tlv::{TLVBuilderParent, Utf8Str, Utf8StrBuilder};
use rs_matter::with;

pub use rs_matter::dm::clusters::decl::bridged_device_basic_information::FULL_CLUSTER;

pub struct BridgedHandler {
    dataver: Dataver,
    info: Mutex<Info>,
}

#[derive(Default)]
struct Info {
    unique_id: String,
    node_label: String,
    reachable: bool,
}

impl BridgedHandler {
    pub fn new(dataver: Dataver) -> Self {
        Self {
            dataver,
            info: Mutex::new(Info::default()),
        }
    }

    /// Populate the device identity and mark it reachable. Called when a slot
    /// is bound to a freshly discovered device.
    pub fn activate(&self, unique_id: String, node_label: String) {
        let mut info = self.lock();
        info.unique_id = unique_id;
        info.node_label = node_label;
        info.reachable = true;
    }

    /// Update reachability from the device's MQTT availability topic.
    pub fn set_reachable(&self, reachable: bool) {
        self.lock().reachable = reachable;
        self.dataver.changed();
    }

    pub fn adapt(self) -> HandlerAdaptor<Self> {
        HandlerAdaptor(self)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Info> {
        self.info.lock().expect("bridged info lock poisoned")
    }
}

impl ClusterHandler for BridgedHandler {
    const CLUSTER: Cluster<'static> = FULL_CLUSTER
        .with_features(0)
        .with_attrs(with!(required; AttributeId::NodeLabel))
        .with_cmds(with!());

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    fn reachable(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        Ok(self.lock().reachable)
    }

    fn unique_id<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        builder.set(self.lock().unique_id.as_str())
    }

    fn node_label<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        builder.set(self.lock().node_label.as_str())
    }

    fn set_node_label(&self, ctx: impl WriteContext, value: Utf8Str<'_>) -> Result<(), Error> {
        self.lock().node_label = value.to_owned();
        ctx.notify_changed();
        Ok(())
    }

    fn handle_keep_active(
        &self,
        _ctx: impl InvokeContext,
        _request: KeepActiveRequest<'_>,
    ) -> Result<(), Error> {
        // These are mains-powered, always-on devices, not ICDs: nothing to keep
        // awake. Acknowledge rather than erroring so controllers don't retry.
        Ok(())
    }
}
