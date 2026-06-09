//! `boss` — a Matter bridge for Home Assistant MQTT-discovery devices.
//!
//! Foundation modules (MQTT + discovery + config) live here and are free of any
//! Matter coupling. The Matter bridge core is layered on top in later steps.

pub mod bridge;
pub mod config;
pub mod discovery;
pub mod error;
pub mod mqtt;
