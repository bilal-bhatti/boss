//! Home Assistant MQTT discovery: topic shape and shared payload types.
//!
//! ESPHome publishes discovery using Home Assistant's *abbreviated* keys
//! (`stat_t`, `cmd_t`, …), retained, under:
//!
//! ```text
//! homeassistant/<component>/<node_id>/<object_id>/config
//! ```
//!
//! `<component>` (e.g. `switch`) selects which device type handles the
//! payload — see the `BridgedDevice` registry.

use serde::Deserialize;

use crate::error::{Error, Result};

/// The discovery prefix every HA discovery topic starts with.
pub const DEFAULT_PREFIX: &str = "homeassistant";

/// The parts of a parsed discovery `config` topic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryTopic<'a> {
    /// e.g. `switch`, `sensor` — selects the device type.
    pub component: &'a str,
    /// e.g. `examplelamp`.
    pub node_id: &'a str,
    /// e.g. `example_lamp_switch`.
    pub object_id: &'a str,
}

impl<'a> DiscoveryTopic<'a> {
    /// Parse `<prefix>/<component>/<node_id>/<object_id>/config`.
    ///
    /// Returns `Err` (never panics) when the topic doesn't match the shape or
    /// prefix. HA also allows a node-less `<prefix>/<component>/<object_id>/config`
    /// form; we accept both and leave `node_id` empty for the short form.
    pub fn parse(prefix: &str, topic: &'a str) -> Result<Self> {
        let rest = topic
            .strip_prefix(prefix)
            .and_then(|r| r.strip_prefix('/'))
            .and_then(|r| r.strip_suffix("/config"))
            .ok_or_else(|| Error::DiscoveryTopic(topic.to_owned()))?;

        let parts: Vec<&str> = rest.split('/').collect();
        match parts.as_slice() {
            [component, node_id, object_id] => Ok(DiscoveryTopic {
                component,
                node_id,
                object_id,
            }),
            [component, object_id] => Ok(DiscoveryTopic {
                component,
                node_id: "",
                object_id,
            }),
            _ => Err(Error::DiscoveryTopic(topic.to_owned())),
        }
    }
}

/// The `dev` object shared by every discovery payload: identifies the
/// physical device behind one or more entities.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DeviceInfo {
    /// `ids` — stable device identifier (e.g. the ESP MAC).
    #[serde(rename = "ids", default)]
    pub identifiers: String,
    #[serde(rename = "name", default)]
    pub name: String,
    /// `sw` — software/firmware version.
    #[serde(rename = "sw", default)]
    pub sw_version: String,
    /// `mdl` — model.
    #[serde(rename = "mdl", default)]
    pub model: String,
    /// `mf` — manufacturer.
    #[serde(rename = "mf", default)]
    pub manufacturer: String,
    /// `cns` — connections, each `[type, value]` (e.g. `["mac", "aabb.."]`).
    #[serde(rename = "cns", default)]
    pub connections: Vec<(String, String)>,
}

/// The discovery payload for a `switch` component.
///
/// Only the fields the bridge needs; serde ignores the rest. Uses HA's
/// abbreviated keys as seen on the wire from ESPHome.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SwitchConfig {
    pub name: String,
    /// `stat_t` — topic carrying `ON`/`OFF`.
    #[serde(rename = "stat_t")]
    pub state_topic: String,
    /// `cmd_t` — topic accepting `ON`/`OFF`.
    #[serde(rename = "cmd_t")]
    pub command_topic: String,
    /// `avty_t` — topic carrying `online`/`offline`.
    #[serde(rename = "avty_t", default)]
    pub availability_topic: Option<String>,
    /// `uniq_id` — stable unique entity id.
    #[serde(rename = "uniq_id")]
    pub unique_id: String,
    #[serde(rename = "dev")]
    pub device: DeviceInfo,
}

impl SwitchConfig {
    pub fn parse(topic: &str, payload: &[u8]) -> Result<Self> {
        serde_json::from_slice(payload).map_err(|source| Error::Discovery {
            topic: topic.to_owned(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A representative ESPHome `switch` discovery payload, using Home
    // Assistant's abbreviated keys (the shape this parser must handle).
    const EXAMPLE_SWITCH: &[u8] = br#"{"name":"Example Lamp Switch","stat_t":"examplelamp/switch/example_lamp_switch/state","cmd_t":"examplelamp/switch/example_lamp_switch/command","avty_t":"examplelamp/status","uniq_id":"ESPswitchexample_lamp_switch","dev":{"ids":"aabbccddeeff","name":"examplelamp","sw":"2025.5.0","mdl":"esp01_1m","mf":"Espressif","cns":[["mac","aabbccddeeff"]]}}"#;

    #[test]
    fn parses_switch_payload() {
        let c = SwitchConfig::parse("homeassistant/switch/x/y/config", EXAMPLE_SWITCH).unwrap();
        assert_eq!(c.name, "Example Lamp Switch");
        assert_eq!(
            c.command_topic,
            "examplelamp/switch/example_lamp_switch/command"
        );
        assert_eq!(c.availability_topic.as_deref(), Some("examplelamp/status"));
        assert_eq!(c.unique_id, "ESPswitchexample_lamp_switch");
        assert_eq!(c.device.identifiers, "aabbccddeeff");
        assert_eq!(c.device.manufacturer, "Espressif");
        assert_eq!(c.device.model, "esp01_1m");
        assert_eq!(
            c.device.connections,
            vec![("mac".to_owned(), "aabbccddeeff".to_owned())]
        );
    }

    #[test]
    fn rejects_garbage_payload() {
        assert!(SwitchConfig::parse("t", b"not json").is_err());
    }

    #[test]
    fn parses_three_part_discovery_topic() {
        let t = DiscoveryTopic::parse(
            DEFAULT_PREFIX,
            "homeassistant/switch/examplelamp/example_lamp_switch/config",
        )
        .unwrap();
        assert_eq!(t.component, "switch");
        assert_eq!(t.node_id, "examplelamp");
        assert_eq!(t.object_id, "example_lamp_switch");
    }

    #[test]
    fn parses_node_less_discovery_topic() {
        let t = DiscoveryTopic::parse(DEFAULT_PREFIX, "homeassistant/sensor/temp/config").unwrap();
        assert_eq!(t.component, "sensor");
        assert_eq!(t.node_id, "");
        assert_eq!(t.object_id, "temp");
    }

    #[test]
    fn rejects_non_config_and_wrong_prefix() {
        assert!(DiscoveryTopic::parse(DEFAULT_PREFIX, "homeassistant/switch/x/state").is_err());
        assert!(DiscoveryTopic::parse(DEFAULT_PREFIX, "other/switch/x/y/config").is_err());
    }
}
