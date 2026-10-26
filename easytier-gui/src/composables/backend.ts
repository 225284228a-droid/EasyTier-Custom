import { invoke } from '@tauri-apps/api/core'
import { Api, NetworkTypes, LocalConfigs, Utils } from 'easytier-frontend-lib'

type NetworkConfig = NetworkTypes.NetworkConfig
type ValidateConfigResponse = Api.ValidateConfigResponse
type ListNetworkInstanceIdResponse = Api.ListNetworkInstanceIdResponse
interface ServiceOptions {
  config_dir: string
  rpc_portal: string
  file_log_level: string
  file_log_dir: string
  config_server?: string
}

export type ServiceStatus = "Running" | "Stopped" | "NotInstalled"

export async function parseNetworkConfig(cfg: NetworkConfig, capabilities?: readonly string[]) {
  return invoke<string>('parse_network_config', { cfg: NetworkTypes.toBackendNetworkConfig(cfg, capabilities) })
}

export async function generateNetworkConfig(tomlConfig: string) {
  const config = await invoke<NetworkConfig>('generate_network_config', { tomlConfig })
  return NetworkTypes.normalizeNetworkConfig(config)
}

export async function collectNetworkInfo(instanceId: string) {
  return await invoke<Api.CollectNetworkInfoResponse>('collect_network_info', { instanceId })
}

export async function getVpnPortalInfo(instanceId: string) {
  const info = await invoke<NetworkTypes.VpnPortalInfo | undefined>('get_vpn_portal_info', { instanceId })
  return info ? NetworkTypes.normalizeVpnPortalInfo(info) : undefined
}

export async function getVpnStatus(instanceId: string) {
  const status = await invoke<NetworkTypes.GetVpnStatusResponse | undefined>('get_vpn_status', { instanceId })
  return status ? NetworkTypes.normalizeVpnStatus(status) : undefined
}

export async function addVpnPortalClient(instanceId: string, client: { name: string, virtual_ip: string, groups: string[] }) {
  return invoke('patch_vpn_portal_clients', { instanceId, action: 'add', name: client.name, virtualIp: client.virtual_ip, groups: client.groups })
}

export async function removeVpnPortalClient(instanceId: string, name: string) {
  return invoke('patch_vpn_portal_clients', { instanceId, action: 'remove', name })
}

export async function clearVpnPortalClients(instanceId: string) {
  return invoke('patch_vpn_portal_clients', { instanceId, action: 'clear' })
}

export async function setLoggingLevel(level: string) {
  return await invoke('set_logging_level', { level })
}

export interface MobileTunConfig {
  instance_ids: string[]
  addresses: string[]
  routes: string[]
  dns?: string | null
  rejected: { instance_id: string, error: string }[]
}

export async function setTunFd(fd: number, instanceIds?: string[], generation?: string) {
  return invoke<MobileTunConfig>('set_tun_fd', { fd, instanceIds, generation })
}

export async function getMobileTunConfig(instanceIds: string[]) {
  return invoke<MobileTunConfig>('get_mobile_tun_config', { instanceIds })
}

export async function getEasytierVersion() {
  return await invoke<string>('easytier_version')
}

export async function listNetworkInstanceIds() {
  return await invoke<ListNetworkInstanceIdResponse>('list_network_instance_ids')
}

export async function deleteNetworkInstance(instanceId: string, expectedRevision: string) {
  return await invoke('remove_network_instance', { instanceId, expectedRevision })
}

export async function updateNetworkConfigState(instanceId: string, disabled: boolean, expectedRevision: string) {
  return await invoke('update_network_config_state', { instanceId, disabled, expectedRevision })
}

export async function validateConfig(cfg: NetworkConfig, capabilities?: readonly string[]) {
  return await invoke<ValidateConfigResponse>('validate_config', { config: NetworkTypes.toBackendNetworkConfig(cfg, capabilities) })
}

export async function observeLocalConfigs() {
  return invoke<Omit<LocalConfigs.LocalConfigSnapshot, 'online' | 'capabilities'>>('observe_local_configs')
}

export async function patchLocalConfig(request: LocalConfigs.LocalConfigPatchRequest) {
  return invoke<LocalConfigs.LocalConfigPatchResult>('patch_local_config', { request: { ...request, inst_id: Utils.StrToUuid(request.inst_id) } })
}

export interface MobileNetworkProfile {
  no_tun: boolean
  dhcp: boolean
  dhcpv6: boolean
  virtual_ipv4?: string | null
}

/** VPN-relevant settings of a local network instance, including one about to start. */
export async function getMobileNetworkProfile(instanceId: string) {
  return invoke<MobileNetworkProfile>('mobile_network_profile', { instanceId })
}

export async function getNetworkMetas(instanceIds: string[]) {
  return await invoke<Api.GetNetworkMetasResponse>('get_network_metas', { instanceIds })
}

export async function initService(opts?: ServiceOptions) {
  return await invoke('init_service', { opts })
}

export async function setServiceStatus(enable: boolean, rpcPortal?: string) {
  return await invoke('set_service_status', { enable, rpcPortal })
}

export interface RetiredService {
  name: string
  /** "disabled" (reversible), "uninstalled" (irreversible), or "failed" */
  action: 'disabled' | 'uninstalled' | 'failed' | string
  error?: string
}

export async function retireConflictingServices(configDir: string, rpcPortal?: string, allowUninstall = false) {
  return await invoke<RetiredService[]>('retire_conflicting_services', { configDir, rpcPortal, allowUninstall })
}

export async function getServiceStatus() {
  return await invoke<ServiceStatus>('get_service_status')
}

export async function resolveSharedConfigDir(configDir?: string) {
  return await invoke<string>('resolve_shared_config_dir', { configDir })
}

export async function initRpcConnection(isNormalMode: boolean, url?: string, configDir?: string) {
  return await invoke('init_rpc_connection', { isNormalMode, url, configDir })
}

export async function isClientRunning() {
  return await invoke<boolean>('is_client_running')
}

export async function initWebClient(url?: string) {
  return await invoke('init_web_client', { url })
}

export async function isWebClientConnected() {
  return await invoke<boolean>('is_web_client_connected')
}
