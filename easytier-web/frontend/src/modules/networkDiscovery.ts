import { inject, provide, ref, type InjectionKey, type Ref } from 'vue'
import { Api as LibApi, ConfigStores, LocalConfigs, NetworkTypes, Utils } from 'easytier-frontend-lib'
import type ApiClient from './api'
import type { CentralNetworkSummary } from './api'
import { networkNameIdentity } from './networkTopology'

/**
 * A network is whatever the devices run: every device configuration with the
 * same network name is one network, identified like the topology identifies
 * running peers. The console stores no network state of its own here.
 */
export interface NetworkMember {
  key: string
  device: Utils.DeviceInfo
  instanceId: string
  running: boolean
  /** Configuration file permission flags reported by the node. */
  permission: number
}

export interface DiscoveredNetwork {
  identity: string
  name: string
  members: NetworkMember[]
  runningCount: number
  /** A central network publishes configurations under this name. */
  central?: CentralNetworkSummary
}

/** Network configurations found on one device. */
export interface DeviceNetworkScan {
  device: Utils.DeviceInfo
  running: string[]
  disabled: string[]
  metas: Record<string, LibApi.NetworkMeta>
}

function instanceIdString(id: Utils.UUID | string): string {
  return typeof id === 'string' ? id : Utils.UuidToStr(id)
}

export async function scanDeviceNetworks(api: ApiClient, device: Utils.DeviceInfo): Promise<DeviceNetworkScan> {
  const remote = api.get_remote_client(device.machine_id)
  const ids = await remote.list_network_instance_ids()
  const running = (ids.running_inst_ids ?? []).map(instanceIdString)
  const disabled = (ids.disabled_inst_ids ?? []).map(instanceIdString).filter(id => !running.includes(id))
  const all = [...running, ...disabled]
  const metas = all.length ? (await remote.get_network_metas(all)).metas ?? {} : {}
  return { device, running, disabled, metas }
}

export function groupNetworks(scans: DeviceNetworkScan[], central: CentralNetworkSummary[] = []): DiscoveredNetwork[] {
  const networks = new Map<string, DiscoveredNetwork>()
  const network = (name: string) => {
    let entry = networks.get(name)
    if (!entry) {
      entry = { identity: networkNameIdentity(name), name, members: [], runningCount: 0 }
      networks.set(name, entry)
    }
    return entry
  }
  for (const scan of scans) {
    const running = new Set(scan.running)
    for (const instanceId of [...scan.running, ...scan.disabled]) {
      const name = scan.metas[instanceId]?.network_name?.trim()
      // A configuration without a name has no network to belong to.
      if (!name) continue
      const entry = network(name)
      const isRunning = running.has(instanceId)
      entry.members.push({
        key: `${scan.device.machine_id}/${instanceId}`,
        device: scan.device,
        instanceId,
        running: isRunning,
        permission: scan.metas[instanceId]?.config_permission ?? 0,
      })
      if (isRunning) entry.runningCount++
    }
  }
  // A central network stays visible while none of its members are online.
  for (const summary of central) {
    if (summary.network_name) network(summary.network_name).central = summary
  }
  const deviceName = (member: NetworkMember) => member.device.alias || member.device.hostname || member.device.machine_id
  for (const entry of networks.values()) {
    entry.members.sort((a, b) => deviceName(a).localeCompare(deviceName(b)) || a.instanceId.localeCompare(b.instanceId))
  }
  return [...networks.values()].sort((a, b) => a.name.localeCompare(b.name))
}

/**
 * Fields that describe one device rather than the network. A batch edit
 * that changes them would give every selected member the same value.
 */
export const PER_DEVICE_FIELDS: readonly string[] = [
  'hostname', 'virtual_ipv4', 'dhcp', 'network_length', 'proxy_cidrs', 'dev_name', 'bind_device',
  'credential_file', 'managed_credentials', 'vpn_portal_config', 'port_forwards', 'mapped_listeners',
  'socket_mark', 'ipv6_public_addr_prefix', 'dhcp_ipv4_cidr', 'dhcp_ipv6_cidr',
]

/** Fields whose values differ between the given configurations. */
export function variedFields(configs: NetworkTypes.NetworkConfig[]): string[] {
  const [first, ...others] = configs
  if (!first) return []
  const fields = new Set<string>()
  for (const other of others) {
    for (const field of LocalConfigs.changedConfigFields(first, other)) fields.add(field)
  }
  // The structured peer list mirrors the initial peer URLs.
  if (fields.has('peer_urls')) fields.delete('peers')
  return [...fields].sort()
}

/**
 * Copy the edited fields from the batch template into one member's
 * configuration; every other field keeps the member's own value.
 */
export function mergeConfigFields(
  target: NetworkTypes.NetworkConfig,
  source: NetworkTypes.NetworkConfig,
  fields: readonly string[],
): NetworkTypes.NetworkConfig {
  const merged = NetworkTypes.toBackendNetworkConfig(target) as unknown as Record<string, unknown>
  const values = NetworkTypes.toBackendNetworkConfig(source) as unknown as Record<string, unknown>
  const copied = new Set(fields)
  // Both describe the initial peers; normalizing refills one from the other.
  if (copied.has('peer_urls') || copied.has('peers')) {
    copied.add('peer_urls')
    copied.add('peers')
  }
  for (const field of copied) {
    if (field === 'secure_mode') {
      // Only the switch is shared; each node keeps its own key pair.
      const own = (merged.secure_mode ?? {}) as Record<string, unknown>
      const enabled = (values.secure_mode as { enabled?: boolean } | undefined)?.enabled ?? false
      merged.secure_mode = enabled ? { ...own, enabled } : undefined
    } else if (values[field] === undefined) {
      // JSON omits default values, so an absent value is a reset.
      delete merged[field]
    } else {
      merged[field] = structuredClone(values[field])
    }
  }
  const config = NetworkTypes.normalizeNetworkConfig(merged as unknown as NetworkTypes.NetworkConfig)
  config.instance_id = target.instance_id
  return config
}

export interface NetworkDiscovery {
  networks: Ref<DiscoveredNetwork[] | undefined>
  refreshing: Ref<boolean>
  /** Devices whose configurations could not be read in the last refresh. */
  failedDevices: Ref<number>
  refresh(): Promise<void>
  start(): void
  stop(): void
}

const DISCOVERY_KEY: InjectionKey<NetworkDiscovery> = Symbol('networkDiscovery')

export function createNetworkDiscovery(api: ApiClient, centralEnabled: () => boolean, interval = 10_000): NetworkDiscovery {
  const networks = ref<DiscoveredNetwork[]>()
  const refreshing = ref(false)
  const failedDevices = ref(0)
  // The last successful scan stands in for a device that briefly fails.
  const scans = new Map<string, DeviceNetworkScan>()
  let scope = api.persistenceScope
  let pending: Promise<void> | undefined
  let timer: number | undefined
  let running = false

  const collect = async () => {
    if (scope !== api.persistenceScope) {
      scope = api.persistenceScope
      scans.clear()
      networks.value = undefined
    }
    const requestScope = scope
    refreshing.value = true
    try {
      const [machines, central] = await Promise.all([
        api.list_machines({ timeout: 8_000 }),
        centralEnabled() ? api.list_networks().catch(() => []) : Promise.resolve([]),
      ])
      if (requestScope !== api.persistenceScope) return
      const devices = machines.map(Utils.buildDeviceInfo).filter(device => device.machine_id)
      const online = devices.filter(device => device.online !== false)
      for (const machineId of scans.keys()) {
        if (!online.some(device => device.machine_id === machineId)) scans.delete(machineId)
      }
      const queue = [...online]
      let failed = 0
      await Promise.all(Array.from({ length: Math.min(4, queue.length) }, async () => {
        while (queue.length) {
          const device = queue.shift()!
          try {
            scans.set(device.machine_id, await scanDeviceNetworks(api, device))
          } catch (cause) {
            failed++
            console.warn(`Failed to read network configurations of ${device.hostname}`, cause)
          }
        }
      }))
      if (requestScope !== api.persistenceScope) return
      failedDevices.value = failed
      networks.value = groupNetworks([...scans.values()], central ?? [])
    } catch (cause) {
      console.warn('Failed to discover networks', cause)
      networks.value ??= []
    } finally {
      refreshing.value = false
    }
  }

  const schedule = () => {
    window.clearTimeout(timer)
    if (running) timer = window.setTimeout(() => void refresh(), interval)
  }

  const refresh = async () => {
    // Concurrent callers share the refresh in flight.
    pending ??= collect().finally(() => {
      pending = undefined
      schedule()
    })
    await pending
  }

  return {
    networks,
    refreshing,
    failedDevices,
    refresh,
    start() {
      running = true
      void refresh()
    },
    stop() {
      running = false
      window.clearTimeout(timer)
    },
  }
}

export function provideNetworkDiscovery(discovery: NetworkDiscovery) {
  provide(DISCOVERY_KEY, discovery)
}

/** The console's shared discovery, when the page is inside the console shell. */
export function injectNetworkDiscovery(): NetworkDiscovery | undefined {
  return inject(DISCOVERY_KEY, undefined)
}

export function useNetworkDiscovery(): NetworkDiscovery {
  const discovery = injectNetworkDiscovery()
  if (!discovery) throw new Error('Network discovery is not provided')
  return discovery
}

/** A readable reason for a failed request; node diagnostics may be plain text. */
export function failureMessage(reason: unknown): string {
  const data = (reason as { response?: { data?: unknown } } | undefined)?.response?.data
  if (typeof data === 'string' && data.trim()) return data
  const message = (data as { message?: unknown } | undefined)?.message
  if (typeof message === 'string' && message) return message
  return reason instanceof Error ? reason.message : String(reason)
}

export interface DeviceConfigStore {
  store: ConfigStores.ConfigStore
  capabilities: string[]
}

/**
 * A configuration store for one device, as device management opens it: the
 * remote client checks writes against the capabilities the listing reports.
 */
export async function deviceConfigStore(api: ApiClient, device: Utils.DeviceInfo): Promise<DeviceConfigStore> {
  const remote = api.get_remote_client(device.machine_id)
  const listed = await remote.list_network_instance_ids()
  const capabilities = listed.runtime_capabilities ?? []
  const revision = ConfigStores.supportsRevisionStore(remote, capabilities, listed.support_local_config_revision)
  return { store: ConfigStores.createConfigStore(remote, capabilities, revision), capabilities }
}
