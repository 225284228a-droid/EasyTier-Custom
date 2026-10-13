const COMMANDS: &[&str] = &[
    "ping",
    "prepare_vpn",
    "start_vpn",
    "reconfigure_vpn",
    "complete_vpn_reconfigure",
    "stop_vpn",
    "get_vpn_status",
    "get_underlying_dns_servers",
    "consume_vpn_tile_action",
    "registerListener",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS)
        .android_path("android")
        .ios_path("ios")
        .build();
}
