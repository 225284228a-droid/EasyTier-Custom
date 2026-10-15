#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PingRequest {
    pub value: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PingResponse {
    pub value: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoidRequest {}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartVpnRequest {
    pub ipv4_addrs: Option<Vec<String>>,
    pub routes: Option<Vec<String>>,
    pub dns_servers: Option<Vec<String>>,
    pub disallowed_applications: Option<Vec<String>>,
    pub mtu: Option<u32>,
    pub instance_ids: Option<Vec<String>>,
    pub generation: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteVpnReconfigureRequest {
    pub generation: String,
    pub installed: bool,
    pub error_msg: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteVpnReconfigureResponse {
    pub accepted: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub error_msg: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VpnStatus {
    pub running: bool,
    pub ipv4_addrs: Option<Vec<String>>,
    pub routes: Option<Vec<String>>,
    pub dns_servers: Option<Vec<String>>,
    pub generation: Option<String>,
    pub generation_high_water: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VpnTileActionResponse {
    pub action: Option<String>,
}
