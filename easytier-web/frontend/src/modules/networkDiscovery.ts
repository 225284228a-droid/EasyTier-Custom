import { inject, provide, ref, type InjectionKey, type Ref } from 'vue'
import { Api as LibApi, ConfigStores, Utils, type NetworkTypes } from 'easytier-frontend-lib'
import type ApiClient from './api'
import type { CentralNetworkSummary, MeshMember, MeshNetworkView } from './api'
import { networkNameIdentity } from './networkTopology'

/**
 * A network is whatever the devices run: every device configuration with the
 * same network name is one network, identified like the topology identifies
 * running peers. The console only remembers what it last saw on each device,
 * so offline members stay visible.
 */
export interface NetworkMember {
  key: string
  device: Utils.DeviceInfo
  instanceId: string
  running: boolean
  /** Configuration file permission flags reported by the node. */
  permission: number
  /** Its configuration was read now; an offline member is only remembered. */
  online: boolean
  /** A missed core-setting change waits for it to connect. */
  pending?: boolean
  /** A temporary member: a central add-on authenticated by a credential. */
  credential?: boolean
  /** Batch edits from this console leave its advanced settings and access control alone. */
  locked?: boolean
}

export interface DiscoveredNetwork {
  identity: string
  name: string
  /**
   * What people call it: the display name its configurations show (a central
   * network's display name first), else the network name.
   */
  label: string
  members: NetworkMember[]
  runningCount: number
  /** The central record behind its temporary devices. */
  central?: CentralNetworkSummary
  /** What the console keeps for it: core settings, preset, joining on their own. */
  record?: MeshNetworkView
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

/** A peer of a network that is not connected to this console. */
export interface NetworkClient {
  key: string
  peerId: number
  hostname: string
  /** Virtual IPv4 with its prefix. */
  address?: string
}

/**
 * What the running members of a network report, by member key, read for
 * all their devices in one request.
 */
export async function memberDetails(api: ApiClient, members: NetworkMember[]): Promise<Map<string, NetworkTypes.NetworkInstanceRunningInfo>> {
  const running = members.filter(member => member.online && member.running)
  const devices = [...new Set(running.map(member => member.device.machine_id))].sort()
  const details = new Map<string, NetworkTypes.NetworkInstanceRunningInfo>()
  if (!devices.length)
    return details
  const machines = await api.collect_topology_network_info(devices)
  for (const [machineId, { info }] of Object.entries(machines)) {
    for (const [instanceId, detail] of Object.entries(info?.info?.map ?? {}))
      details.set(`${machineId}/${instanceId}`, detail)
  }
  return details
}

/**
 * The peers in the members' route tables that are neither members nor
 * public servers: clients that joined the network without this console. A
 * route table lists every peer of the network, not only the direct ones.
 */
export function networkClients(details: Map<string, NetworkTypes.NetworkInstanceRunningInfo>, members: NetworkMember[]): NetworkClient[] {
  const memberInstances = new Set(members.map(member => member.instanceId))
  const memberPeers = new Set<number>()
  const memberRoutes: NetworkTypes.Route[] = []
  for (const member of members) {
    const detail = details.get(member.key)
    if (!detail)
      continue
    if (detail.my_node_info?.peer_id)
      memberPeers.add(detail.my_node_info.peer_id)
    memberRoutes.push(...detail.routes ?? [])
  }
  const clients = new Map<string, NetworkClient>()
  for (const route of memberRoutes) {
    if (!route.peer_id || memberPeers.has(route.peer_id) || memberInstances.has(route.inst_id)
      || route.feature_flag?.is_public_server)
      continue
    const key = route.inst_id || `peer:${route.peer_id}`
    if (clients.has(key))
      continue
    clients.set(key, {
      key,
      peerId: route.peer_id,
      hostname: route.hostname?.trim() || `Peer ${route.peer_id}`,
      address: route.ipv4_addr?.address ? Utils.ipv4InetToString(route.ipv4_addr) : undefined,
    })
  }
  return [...clients.values()].sort((left, right) => left.hostname.localeCompare(right.hostname))
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

export function groupNetworks(
  scans: DeviceNetworkScan[],
  central: CentralNetworkSummary[] = [],
  remembered: MeshMember[] = [],
  devices: Utils.DeviceInfo[] = [],
  records: MeshNetworkView[] = [],
): DiscoveredNetwork[] {
  const networks = new Map<string, DiscoveredNetwork>()
  const network = (name: string) => {
    let entry = networks.get(name)
    if (!entry) {
      entry = { identity: networkNameIdentity(name), name, label: name, members: [], runningCount: 0 }
      networks.set(name, entry)
    }
    return entry
  }
  const displayNames = new Map<string, string[]>()
  for (const scan of scans) {
    const running = new Set(scan.running)
    for (const instanceId of [...scan.running, ...scan.disabled]) {
      const name = scan.metas[instanceId]?.network_name?.trim()
      // A configuration without a name has no network to belong to.
      if (!name) continue
      const entry = network(name)
      const isRunning = running.has(instanceId)
      const shown = LibApi.configDisplayName(scan.metas[instanceId], instanceId)
      if (shown !== name) displayNames.set(name, [...(displayNames.get(name) ?? []), shown])
      entry.members.push({
        key: `${scan.device.machine_id}/${instanceId}`,
        device: scan.device,
        instanceId,
        running: isRunning,
        permission: scan.metas[instanceId]?.config_permission ?? 0,
        online: true,
      })
      if (isRunning) entry.runningCount++
    }
  }
  // Remembered instances add what a scan cannot show: offline members,
  // a pending change, and which members are temporary.
  const scanned = new Map([...networks.values()].flatMap(entry => entry.members.map(member => [member.key, member] as const)))
  const scannedDevices = new Set(scans.map(scan => scan.device.machine_id))
  for (const member of remembered) {
    const key = `${member.device_id}/${member.instance_id}`
    const known = scanned.get(key)
    if (known) {
      known.pending = member.pending || undefined
      known.credential = member.credential || undefined
      known.locked = member.locked || undefined
      continue
    }
    // A scanned device no longer has this instance.
    if (scannedDevices.has(member.device_id) || !member.network_name) continue
    if (member.display_name) displayNames.set(member.network_name, [...(displayNames.get(member.network_name) ?? []), member.display_name])
    const device = devices.find(device => device.machine_id === member.device_id)
      ?? { machine_id: member.device_id, hostname: member.device_id, running_network_count: 0 } as Utils.DeviceInfo
    network(member.network_name).members.push({
      key,
      device,
      instanceId: member.instance_id,
      running: false,
      permission: 0,
      online: false,
      pending: member.pending || undefined,
      credential: member.credential || undefined,
      locked: member.locked || undefined,
    })
  }
  // The name most members show labels the network.
  for (const [name, shown] of displayNames) {
    const counts = new Map<string, number>()
    for (const value of shown) counts.set(value, (counts.get(value) ?? 0) + 1)
    network(name).label = [...counts].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))[0][0]
  }
  // A central network stays visible while none of its members are online.
  for (const summary of central) {
    if (!summary.network_name) continue
    const entry = network(summary.network_name)
    entry.central = summary
    entry.label = summary.display_name || summary.network_name
  }
  // So does a network the console keeps a record of, before its first member.
  for (const record of records) {
    if (!record.network_name) continue
    const entry = network(record.network_name)
    entry.record = record
    if (record.core.display_name) entry.label = record.core.display_name
  }
  const deviceName = (member: NetworkMember) => member.device.alias || member.device.hostname || member.device.machine_id
  for (const entry of networks.values()) {
    entry.members.sort((a, b) => deviceName(a).localeCompare(deviceName(b)) || a.instanceId.localeCompare(b.instanceId))
  }
  return [...networks.values()].sort((a, b) => a.name.localeCompare(b.name))
}

export interface NetworkDiscovery {
  networks: Ref<DiscoveredNetwork[] | undefined>
  refreshing: Ref<boolean>
  /** Devices whose configurations could not be read in the last refresh. */
  failedDevices: Ref<number>
  /** The device or central network list could not be read in the last refresh. */
  listFailed: Ref<boolean>
  refresh(): Promise<void>
  start(): void
  stop(): void
}

const DISCOVERY_KEY: InjectionKey<NetworkDiscovery> = Symbol('networkDiscovery')

export function createNetworkDiscovery(api: ApiClient, centralEnabled: () => boolean, interval = 10_000): NetworkDiscovery {
  const networks = ref<DiscoveredNetwork[]>()
  const refreshing = ref(false)
  const failedDevices = ref(0)
  const listFailed = ref(false)
  // A failed read keeps showing what the console reported last.
  let lastCentral: CentralNetworkSummary[] = []
  let lastRemembered: MeshMember[] = []
  let lastRecords: MeshNetworkView[] = []
  // The last successful scan stands in for a device that briefly fails.
  const scans = new Map<string, DeviceNetworkScan>()
  let scope = api.persistenceScope
  let pending: Promise<void> | undefined
  let again: Promise<void> | undefined
  let timer: number | undefined
  let running = false

  const collect = async () => {
    if (scope !== api.persistenceScope) {
      scope = api.persistenceScope
      scans.clear()
      lastCentral = []
      lastRemembered = []
      lastRecords = []
      networks.value = undefined
    }
    const requestScope = scope
    refreshing.value = true
    try {
      let centralFailed = false
      const keep = <T>(last: T[]) => (cause: unknown) => {
        centralFailed = true
        console.warn('Failed to read central networks', cause)
        return last
      }
      const central = centralEnabled()
      const [machines, centralNetworks, remembered, records] = await Promise.all([
        api.list_machines({ timeout: 8_000 }),
        central ? api.list_networks().catch(keep(lastCentral)) : Promise.resolve([]),
        central ? api.list_mesh_members().catch(keep(lastRemembered)) : Promise.resolve([]),
        central ? api.list_mesh_networks().catch(keep(lastRecords)) : Promise.resolve([]),
      ])
      if (requestScope !== api.persistenceScope) return
      lastCentral = centralNetworks ?? []
      lastRemembered = remembered ?? []
      lastRecords = records ?? []
      listFailed.value = centralFailed
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
      networks.value = groupNetworks([...scans.values()], lastCentral, lastRemembered, devices, lastRecords)
    } catch (cause) {
      console.warn('Failed to discover networks', cause)
      listFailed.value = true
      networks.value ??= []
    } finally {
      refreshing.value = false
    }
  }

  const schedule = () => {
    window.clearTimeout(timer)
    if (running) timer = window.setTimeout(() => void refresh(), interval)
  }

  const refresh = async (): Promise<void> => {
    // A request during a refresh needs a later one: what it asks for (a
    // new mode, a write it just made) may not be in the running one.
    if (pending) {
      again ??= pending.then(() => {
        again = undefined
        return refresh()
      })
      return again
    }
    pending = collect().finally(() => {
      pending = undefined
      schedule()
    })
    await pending
  }

  return {
    networks,
    refreshing,
    failedDevices,
    listFailed,
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

/** Pauses before the second and third try of a device write. */
const RETRY_PAUSES = [1000, 3000]

/** Whether a failed device write may pass when tried again. */
function retryable(reason: unknown): boolean {
  if (reason instanceof ConfigStores.ConfigApplyError) return false
  const status = (reason as { response?: { status?: number } } | undefined)?.response?.status
  return status === undefined || status === 408 || status === 409 || status === 429 || status >= 500
}

/**
 * Run a device write up to three times over an unsteady connection. `write`
 * reads the device again on each try, so it must only send what is still
 * needed (setting a state, not creating something).
 */
export async function retryDeviceWrite<T>(write: () => Promise<T>, pauses = RETRY_PAUSES): Promise<T> {
  for (const pause of pauses) {
    try {
      return await write()
    } catch (reason) {
      if (!retryable(reason)) throw reason
      await new Promise(resolve => setTimeout(resolve, pause))
    }
  }
  return write()
}
