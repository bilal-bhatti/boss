//! Matter node/endpoint construction for the bridge.
//!
//! Endpoint layout:
//! - `0` — root node (commissioning, diagnostics, …)
//! - `1` — aggregator (enumerates the bridged devices)
//! - `2 .. 2 + MAX_DEVICES` — one bridged device per slot
//!
//! Per-device endpoints differ only by id; their device types and cluster
//! metadata are shared `'static` arrays, so building the node for any active
//! set is just pushing a handful of cheap `Endpoint` values.

use rs_matter::dm::clusters::app::on_off::OnOffHooks;
use rs_matter::dm::clusters::decl::bridged_device_basic_information::ClusterHandler as _;
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _};
use rs_matter::dm::clusters::groups::{self, ClusterHandler as _};
use rs_matter::dm::devices::{DEV_TYPE_AGGREGATOR, DEV_TYPE_BRIDGED_NODE, DEV_TYPE_ON_OFF_LIGHT};
use rs_matter::dm::{Cluster, DeviceType, Endpoint, EndptId, Node};
use rs_matter::root_endpoint;

use crate::bridge::bridged::BridgedHandler;
use crate::bridge::switch::MqttOnOff;

/// Maximum number of bridged devices. Endpoints are statically dimensioned to
/// this, so it bounds memory but not the *discovery* rate.
pub const MAX_DEVICES: usize = 16;

/// Endpoint id of the aggregator that enumerates bridged devices.
pub const AGGREGATOR_ENDPOINT: EndptId = 1;

/// First endpoint id used for a bridged device.
pub const FIRST_DEVICE_ENDPOINT: EndptId = 2;

/// The endpoint id for the device in slot `index` (`0 .. MAX_DEVICES`).
pub const fn device_endpoint(index: usize) -> EndptId {
    FIRST_DEVICE_ENDPOINT + index as EndptId
}

const AGGREGATOR_DEV_TYPES: [DeviceType; 1] = [DEV_TYPE_AGGREGATOR];
const AGGREGATOR_CLUSTERS: [Cluster<'static>; 1] = [desc::DescHandler::CLUSTER];

const SWITCH_DEV_TYPES: [DeviceType; 2] = [DEV_TYPE_ON_OFF_LIGHT, DEV_TYPE_BRIDGED_NODE];
const SWITCH_CLUSTERS: [Cluster<'static>; 4] = [
    desc::DescHandler::CLUSTER,
    groups::GroupsHandler::CLUSTER,
    BridgedHandler::CLUSTER,
    <MqttOnOff as OnOffHooks>::CLUSTER,
];

/// The root (endpoint 0) metadata. Defined as a const so the `clusters!`
/// arrays the macro expands to are const-promoted to `'static`.
const ROOT_ENDPOINT: Endpoint<'static> = root_endpoint!(eth);

fn aggregator_endpoint() -> Endpoint<'static> {
    Endpoint::new(
        AGGREGATOR_ENDPOINT,
        &AGGREGATOR_DEV_TYPES,
        &AGGREGATOR_CLUSTERS,
    )
}

fn switch_endpoint(id: EndptId) -> Endpoint<'static> {
    Endpoint::new(id, &SWITCH_DEV_TYPES, &SWITCH_CLUSTERS)
}

/// Fill `buf` with the endpoints for the currently active device slots: the
/// root and aggregator endpoints followed by one endpoint per active slot.
///
/// `active_slots` are slot indices (`0 .. MAX_DEVICES`) currently in use.
pub fn write_endpoints(
    active_slots: impl IntoIterator<Item = usize>,
    buf: &mut Vec<Endpoint<'static>>,
) {
    buf.clear();
    buf.push(ROOT_ENDPOINT);
    buf.push(aggregator_endpoint());
    for slot in active_slots {
        buf.push(switch_endpoint(device_endpoint(slot)));
    }
}

/// Borrow `buf` as a `Node` for the duration of `f`.
pub fn with_node<R>(buf: &[Endpoint<'static>], f: impl FnOnce(&Node<'_>) -> R) -> R {
    f(&Node { endpoints: buf })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_follow_the_id_scheme() {
        let mut buf = Vec::new();
        write_endpoints([0, 1, 2], &mut buf);

        // root + aggregator + 3 devices.
        let ids: Vec<EndptId> = buf.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![0, AGGREGATOR_ENDPOINT, 2, 3, 4]);
    }

    #[test]
    fn empty_active_set_is_just_root_and_aggregator() {
        let mut buf = Vec::new();
        write_endpoints([], &mut buf);
        assert_eq!(buf.iter().map(|e| e.id).collect::<Vec<_>>(), vec![0, 1]);
    }
}
