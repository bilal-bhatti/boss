//! The single error type for the bridge.
//!
//! Everything fallible returns `Result<_, Error>`. We never panic on bad
//! external input (malformed MQTT payloads, broker hiccups); we surface it.

use std::borrow::Cow;

/// All the ways an operation in `boss` can fail.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A discovery payload could not be decoded into a known device.
    #[error("invalid discovery payload on topic `{topic}`: {source}")]
    Discovery {
        topic: String,
        #[source]
        source: serde_json::Error,
    },

    /// A discovery topic did not match the expected
    /// `homeassistant/<component>/.../config` shape.
    #[error("malformed discovery topic `{0}`")]
    DiscoveryTopic(String),

    /// No registered device type handles this discovery component.
    #[error("no device type registered for component `{0}`")]
    UnknownComponent(String),

    /// The MQTT client failed (connect, subscribe, publish, …).
    #[error("mqtt error: {0}")]
    Mqtt(Cow<'static, str>),

    /// Bad command-line / environment configuration.
    #[error("configuration error: {0}")]
    Config(Cow<'static, str>),
}

impl Error {
    pub fn config(msg: impl Into<Cow<'static, str>>) -> Self {
        Error::Config(msg.into())
    }

    pub fn mqtt(msg: impl Into<Cow<'static, str>>) -> Self {
        Error::Mqtt(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
