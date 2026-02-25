use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInterfaceConfig {
    pub name: String,
    pub ipv4: String,
    #[serde(default = "default_mask")]
    pub mask: u8,
    #[serde(default)]
    pub gateway: Option<String>,
}

pub fn default_mask() -> u8 {
    24
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    pub dest: String,
    pub mask: u8,
    pub via: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    #[serde(default = "default_buffer_size")]
    pub buffer_size: usize,
    pub interfaces: Vec<NetworkInterfaceConfig>,
    pub routes: Vec<RouteConfig>,
}

pub fn default_buffer_size() -> usize {
    1024 * 1024 // 1MB
}
