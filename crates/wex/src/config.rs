use core::net::SocketAddr;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub const DEFAULT_NATS_ADDRESS: &str = "nats://localhost:4222";

fn default_nats_address() -> Box<str> {
    DEFAULT_NATS_ADDRESS.into()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct HttpTrigger {
    pub address: SocketAddr,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Nats {
    /// NATS address to use
    #[serde(default = "default_nats_address")]
    pub address: Box<str>,
    // TODO: TLS etc.
}

impl Default for Nats {
    fn default() -> Self {
        Self {
            address: default_nats_address(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct NatsTrigger {
    pub subject: Box<str>,
    /// NATS queue group to use
    #[serde(default)]
    pub group: Box<str>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct WrpcNatsTrigger {
    /// Prefix to listen for export invocations on
    #[serde(default)]
    pub prefix: Box<str>,
    /// NATS queue group to use
    #[serde(default)]
    pub group: Box<str>,
    pub instance: Box<str>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct WrpcTrigger {
    #[serde(default)]
    pub nats: Box<[WrpcNatsTrigger]>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Trigger {
    #[serde(default)]
    pub http: Box<[HttpTrigger]>,
    #[serde(default)]
    pub nats: Box<[NatsTrigger]>,
    #[serde(default)]
    pub wrpc: WrpcTrigger,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct KeyvalueBucket {
    pub target: Box<str>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Keyvalue {
    #[serde(default)]
    pub buckets: HashMap<Box<str>, KeyvalueBucket>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct MessagingClient {
    pub target: Box<str>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Messaging {
    #[serde(default)]
    pub clients: HashMap<Box<str>, MessagingClient>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Plugin {
    pub protocol: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub trigger: Trigger,
    #[serde(default)]
    pub keyvalue: Keyvalue,
    #[serde(default)]
    pub messaging: Messaging,
    #[serde(default)]
    pub plugin: HashMap<Box<str>, Plugin>,
    #[serde(default)]
    pub nats: Nats,
}
