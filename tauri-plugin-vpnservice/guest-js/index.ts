import { invoke } from '@tauri-apps/api/core'

export async function ping(value: string): Promise<string | null> {
  return await invoke<{ value?: string }>('plugin:vpnservice|ping', {
    payload: {
      value,
    },
  }).then((r) => (r.value ? r.value : null));
}

export interface InvokeResponse {
  errorMsg?: string;
  granted?: boolean;
}

export interface StartVpnRequest {
  ipv4Addrs?: string[];
  routes?: string[];
  dnsServers?: string[];
  disallowedApplications?: string[];
  mtu?: number;
  instanceIds?: string[];
  generation?: string;
}

export interface VpnStatusResponse {
  running: boolean;
  ipv4Addrs?: string[];
  routes?: string[];
  dnsServers?: string[];
  generation?: string;
  generationHighWater?: string;
}

export interface UnderlyingDnsServersResponse {
  /** DNS literals for the VPN Builder, including an IPv6 zone when present. */
  dnsServers: string[];
  /** Normalized address literals used by Core route classification. */
  systemDnsServers: string[];
  errorMsg?: string;
}

export interface CompleteVpnReconfigureResponse {
  accepted: boolean;
}

export type VpnTileAction = 'start' | 'stop';

export interface VpnTileActionResponse {
  action?: VpnTileAction;
}

export async function prepare_vpn(): Promise<InvokeResponse | null> {
  return await invoke<InvokeResponse>('plugin:vpnservice|prepare_vpn', {})
}

export async function start_vpn(request: StartVpnRequest): Promise<InvokeResponse | null> {
  return await invoke<InvokeResponse>('plugin:vpnservice|start_vpn', {
    ...request,
  })
}

export async function reconfigure_vpn(request: StartVpnRequest): Promise<InvokeResponse | null> {
  return await invoke<InvokeResponse>('plugin:vpnservice|reconfigure_vpn', {
    ...request,
  })
}

export async function complete_vpn_reconfigure(
  generation: string,
  installed: boolean,
  errorMsg?: string,
): Promise<CompleteVpnReconfigureResponse> {
  return await invoke<CompleteVpnReconfigureResponse>('plugin:vpnservice|complete_vpn_reconfigure', {
    generation,
    installed,
    errorMsg,
  })
}

export async function stop_vpn(): Promise<InvokeResponse | null> {
  return await invoke<InvokeResponse>('plugin:vpnservice|stop_vpn', {})
}

export async function get_vpn_status(): Promise<VpnStatusResponse | null> {
  return await invoke<VpnStatusResponse>('plugin:vpnservice|get_vpn_status', {})
}

export async function get_underlying_dns_servers(): Promise<UnderlyingDnsServersResponse> {
  return await invoke<UnderlyingDnsServersResponse>('plugin:vpnservice|get_underlying_dns_servers', {})
}

export async function consume_vpn_tile_action(): Promise<VpnTileActionResponse> {
  return await invoke<VpnTileActionResponse>('plugin:vpnservice|consume_vpn_tile_action', {})
}
