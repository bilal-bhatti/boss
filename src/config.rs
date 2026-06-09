//! Runtime configuration, parsed once from command-line arguments.
//!
//! Explicit flags over a packed URI: clearer to read, and each bad value fails
//! loudly with a specific message rather than silently defaulting.

use std::path::PathBuf;

use crate::discovery::DEFAULT_PREFIX;
use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub mqtt_host: String,
    pub mqtt_port: u16,
    pub mqtt_username: Option<String>,
    pub mqtt_password: Option<String>,
    /// Discovery topic prefix (`homeassistant` by default).
    pub discovery_prefix: String,
    /// Directory for persisted Matter state (commissioning, fabrics). Must
    /// survive reboots for the device to stay paired.
    pub state_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mqtt_host: "localhost".to_owned(),
            mqtt_port: 1883,
            mqtt_username: None,
            mqtt_password: None,
            discovery_prefix: DEFAULT_PREFIX.to_owned(),
            state_dir: std::env::temp_dir().join("rs-matter"),
        }
    }
}

impl Config {
    /// Parse from `std::env::args()`. Unknown flags and missing values are
    /// errors, not silently ignored.
    pub fn from_args() -> Result<Self> {
        Self::from_iter(std::env::args().skip(1))
    }

    fn from_iter(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut cfg = Config::default();
        let mut args = args.into_iter();

        while let Some(flag) = args.next() {
            let mut value = || {
                args.next()
                    .ok_or_else(|| Error::config(format!("missing value for `{flag}`")))
            };

            match flag.as_str() {
                "--mqtt-host" => cfg.mqtt_host = value()?,
                "--mqtt-port" => {
                    let v = value()?;
                    cfg.mqtt_port = v
                        .parse()
                        .map_err(|_| Error::config(format!("invalid --mqtt-port `{v}`")))?;
                }
                "--mqtt-user" => cfg.mqtt_username = Some(value()?),
                "--mqtt-pass" => cfg.mqtt_password = Some(value()?),
                "--discovery-prefix" => cfg.discovery_prefix = value()?,
                "--state-dir" => cfg.state_dir = PathBuf::from(value()?),
                "-h" | "--help" => return Err(Error::config(USAGE)),
                other => return Err(Error::config(format!("unknown flag `{other}`\n{USAGE}"))),
            }
        }

        Ok(cfg)
    }

    /// The wildcard discovery topic to subscribe to for a given component,
    /// e.g. `homeassistant/switch/#`.
    pub fn discovery_filter(&self, component: &str) -> String {
        format!("{}/{}/#", self.discovery_prefix, component)
    }
}

const USAGE: &str = "\
boss — a Matter bridge for Home Assistant MQTT-discovery devices

Usage: boss [options]
  --mqtt-host <host>        MQTT broker host (default: localhost)
  --mqtt-port <port>        MQTT broker port (default: 1883)
  --mqtt-user <user>        MQTT username (optional)
  --mqtt-pass <pass>        MQTT password (optional)
  --discovery-prefix <p>    HA discovery prefix (default: homeassistant)
  --state-dir <dir>         Matter state dir (default: <tmp>/rs-matter)
  -h, --help                Show this help";

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config> {
        Config::from_iter(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_when_no_args() {
        let c = parse(&[]).unwrap();
        assert_eq!(c.mqtt_host, "localhost");
        assert_eq!(c.mqtt_port, 1883);
        assert_eq!(c.discovery_filter("switch"), "homeassistant/switch/#");
    }

    #[test]
    fn parses_flags() {
        let c = parse(&["--mqtt-host", "broker.example", "--mqtt-port", "8883"]).unwrap();
        assert_eq!(c.mqtt_host, "broker.example");
        assert_eq!(c.mqtt_port, 8883);
    }

    #[test]
    fn errors_on_bad_port_and_unknown_flag() {
        assert!(parse(&["--mqtt-port", "nope"]).is_err());
        assert!(parse(&["--frobnicate"]).is_err());
        assert!(parse(&["--mqtt-host"]).is_err()); // missing value
    }
}
