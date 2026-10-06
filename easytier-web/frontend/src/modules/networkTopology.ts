import { Utils } from 'easytier-frontend-lib'
import type { NetworkTypes } from 'easytier-frontend-lib'
import type { TrafficObservation, TrafficTracker } from './topologyTraffic'
import { TOPOLOGY_CACHE_TTL_MS } from './topologyTraffic'

/**
 * A topology node is a peer in a running EasyTier network. A machine can
 * host more than one network, so the console machine id is deliberately not
 * used as the identity of a topology node.
 */
export interface TopologyNode {
  id: string
  label: string
  networkIdentity: string
  peerId: number
  country?: string
  latitude?: number
  longitude?: number
  /** Location reported for the node itself, rather than the console session. */
  nodeLocation?: Utils.Location
  /** Public address reported by EasyTier's running node info. */
  publicIp?: string
  managed: boolean
  machineId?: string
  /** This node only has a cached observation in the current refresh. */
  stale?: boolean
}

export interface TopologyLink {
  source: string
  target: string
  protocols: string[]
  /** Neither endpoint freshly observed this connection in the current refresh. */
  stale?: boolean
  /** Estimated bits per second from source to target. */
  txBps?: number
  /** Estimated bits per second from target to source. */
  rxBps?: number
}

export interface NetworkSnapshot {
  device: Utils.DeviceInfo
  instanceId: string
  detail: NetworkTypes.NetworkInstanceRunningInfo
  stale?: boolean
  /** Monotonic time when this machine's successful RPC completed. */
  collectedAt?: number
}

export interface Topology {
  nodes: TopologyNode[]
  links: TopologyLink[]
  /** Unique running network identities represented by the collected snapshots. */
  networkIdentities: string[]
}

function validPeerId(value: unknown): value is number {
  return typeof value === 'number' && Number.isInteger(value) && value > 0 && value <= 0xffffffff
}

function cleanText(value: unknown): string | undefined {
  if (typeof value !== 'string')
    return undefined
  const text = value.trim()
  return text.length ? text : undefined
}

function networkNameFrom(snapshot: NetworkSnapshot): string | undefined {
  const runtimeName = cleanText(snapshot.detail.network_name)
  if (runtimeName)
    return runtimeName
  const names = new Set<string>()
  const addPeers = (peers: NetworkTypes.PeerInfo[] | undefined) => {
    for (const peer of peers ?? []) {
      for (const conn of peer.conns ?? []) {
        if (conn.is_closed)
          continue
        const name = cleanText(conn.network_name)
        if (name)
          names.add(name)
      }
    }
  }
  addPeers(snapshot.detail.peers)
  addPeers(snapshot.detail.peer_route_pairs
    ?.map(pair => pair.peer)
    .filter((peer): peer is NetworkTypes.PeerInfo => !!peer))
  return [...names].sort()[0]
}

/**
 * Get the network identity without looking at a web-console URL or an
 * underlay address. The peer connection's network name is the strongest
 * runtime signal; an instance id is only a fallback for older peers that do
 * not include the name in connection info.
 */
export function networkIdentity(snapshot: NetworkSnapshot): string {
  const name = networkNameFrom(snapshot)
  if (name)
    return `name:${name}`
  const instanceId = cleanText(snapshot.instanceId)
  if (instanceId)
    return `instance:${instanceId}`
  return `machine:${snapshot.device.machine_id}`
}

function networkIdentityWithInheritedName(
  snapshot: NetworkSnapshot,
  namesByInstance: Map<string, Set<string>>,
): string {
  const ownName = networkNameFrom(snapshot)
  if (ownName)
    return `name:${ownName}`
  const instanceId = cleanText(snapshot.instanceId)
  const inheritedNames = instanceId ? namesByInstance.get(instanceId) : undefined
  const inheritedName = inheritedNames?.size === 1 ? [...inheritedNames][0] : undefined
  if (inheritedName)
    return `name:${inheritedName}`
  return networkIdentity(snapshot)
}

function nodeId(network: string, peerId: number): string {
  return `network:${network}:peer:${peerId}`
}

function ipv4(value: unknown): string | undefined {
  if (!value || typeof value !== 'object')
    return undefined
  const addr = (value as { addr?: unknown }).addr
  if (typeof addr !== 'number' || !Number.isFinite(addr) || addr <= 0)
    return undefined
  return Utils.ipv4ToString(value as { addr: number })
}

function snapshotLocation(snapshot: NetworkSnapshot): Utils.Location | undefined {
  const location = snapshot.detail.node_location
  if (location) {
    return {
      country: location.country,
      city: location.city,
      region: location.region,
      latitude: location.latitude,
      longitude: location.longitude,
    }
  }
  // Never fall back to the console session location here. That address may
  // be a CDN/FRP endpoint and is unrelated to the running mesh node.
  return undefined
}

function snapshotNode(snapshot: NetworkSnapshot, network: string, peerId: number): TopologyNode {
  const info = snapshot.detail.my_node_info
  const location = snapshotLocation(snapshot)
  return {
    id: nodeId(network, peerId),
    label: cleanText(info?.hostname) ?? cleanText(snapshot.device.hostname) ?? `Peer ${peerId}`,
    networkIdentity: network,
    peerId,
    country: location?.country,
    latitude: location?.latitude,
    longitude: location?.longitude,
    nodeLocation: location,
    publicIp: snapshot.detail.node_location?.public_ip ?? ipv4(info?.ips?.public_ipv4),
    managed: true,
    machineId: snapshot.device.machine_id,
    stale: snapshot.stale || undefined,
  }
}

function routeForPeer(snapshot: NetworkSnapshot, peerId: number): NetworkTypes.Route | undefined {
  const route = snapshot.detail.routes?.find(item => item.peer_id === peerId)
  if (route)
    return route
  return snapshot.detail.peer_route_pairs?.find(pair => pair.route?.peer_id === peerId)?.route
}

function mergeNode(existing: TopologyNode | undefined, incoming: TopologyNode): TopologyNode {
  if (!existing)
    return incoming
  const preferIncoming = incoming.managed && (!existing.managed || (!!existing.stale && !incoming.stale))
    || (!existing.managed && !incoming.managed && !!existing.stale && !incoming.stale)
  const primary = preferIncoming ? incoming : existing
  const secondary = preferIncoming ? existing : incoming
  const stale = existing.managed === incoming.managed
    ? existing.stale && incoming.stale
    : primary.stale
  return {
    ...primary,
    // A node observed from a managed snapshot is authoritative over a route
    // placeholder. Keep the first non-empty runtime metadata otherwise.
    label: primary.label,
    country: primary.country ?? secondary.country,
    latitude: primary.latitude ?? secondary.latitude,
    longitude: primary.longitude ?? secondary.longitude,
    nodeLocation: primary.nodeLocation ?? secondary.nodeLocation,
    publicIp: primary.publicIp ?? secondary.publicIp,
    managed: existing.managed || incoming.managed,
    machineId: primary.machineId ?? secondary.machineId,
    stale: stale || undefined,
  }
}

export function buildTopology(
  devices: Utils.DeviceInfo[],
  snapshots: NetworkSnapshot[],
  trafficTracker?: TrafficTracker,
  sampleTime = Date.now(),
): Topology {
  const nodes = new Map<string, TopologyNode>()
  const links = new Map<string, TopologyLink>()
  const networkIdentities = new Set<string>()
  const namesByInstance = new Map<string, Set<string>>()
  const trafficObservations: TrafficObservation[] = []
  for (const snapshot of snapshots) {
    const instanceId = cleanText(snapshot.instanceId)
    const name = networkNameFrom(snapshot)
    if (!instanceId || !name)
      continue
    const names = namesByInstance.get(instanceId) ?? new Set<string>()
    names.add(name)
    namesByInstance.set(instanceId, names)
  }

  // Register every local runtime peer before reading links. This makes
  // peer-id matching deterministic even when instance ids were copied or a
  // node has several underlay addresses behind CDN/FRP.
  const snapshotIdentities = new Map<NetworkSnapshot, { network: string, peerId: number }>()
  for (const snapshot of snapshots) {
    const network = networkIdentityWithInheritedName(snapshot, namesByInstance)
    networkIdentities.add(network)
    const peerId = snapshot.detail.my_node_info?.peer_id
    if (!validPeerId(peerId))
      continue
    snapshotIdentities.set(snapshot, { network, peerId })
    const local = snapshotNode(snapshot, network, peerId)
    nodes.set(local.id, mergeNode(nodes.get(local.id), local))
  }

  for (const snapshot of snapshots) {
    const identity = snapshotIdentities.get(snapshot)
    if (!identity)
      continue
    const source = nodeId(identity.network, identity.peerId)

    // A route describes reachability through the mesh. Only an open peer
    // connection establishes a direct edge in this visualization.
    for (const peer of snapshot.detail.peers ?? []) {
      const openConns = (peer.conns ?? []).filter(conn => !conn.is_closed)
      if (!openConns.length)
        continue
      const peerId = openConns.find(conn => validPeerId(conn.peer_id))?.peer_id
        ?? (validPeerId(peer.peer_id) ? peer.peer_id : undefined)
      if (!validPeerId(peerId))
        continue

      const target = nodeId(identity.network, peerId)
      if (target === source)
        continue

      const route = routeForPeer(snapshot, peerId)
      const placeholder: TopologyNode = {
        id: target,
        label: cleanText(route?.hostname) ?? `Peer ${peerId}`,
        networkIdentity: identity.network,
        peerId,
        managed: false,
        stale: snapshot.stale || undefined,
      }
      nodes.set(target, mergeNode(nodes.get(target), placeholder))

      const [canonicalSource, canonicalTarget] = [source, target].sort()
      const key = JSON.stringify([canonicalSource, canonicalTarget])
      const existing = links.get(key)
      const protocols = [...new Set([
        ...(existing?.protocols ?? []),
        ...openConns
          .map(conn => cleanText(conn.tunnel?.tunnel_type))
          .filter((value): value is string => !!value),
      ])].sort()
      links.set(key, {
        source: canonicalSource,
        target: canonicalTarget,
        protocols,
        stale: (existing ? existing.stale && snapshot.stale : snapshot.stale) || undefined,
      })
      if (trafficTracker && !snapshot.stale) {
        for (const conn of openConns) {
          trafficObservations.push({
            source,
            target,
            connId: conn.conn_id,
            txBytes: conn.stats ? (conn.stats.tx_bytes ?? 0) : undefined,
            rxBytes: conn.stats ? (conn.stats.rx_bytes ?? 0) : undefined,
            sampleTime: snapshot.collectedAt,
          })
        }
      }
    }
  }

  // `devices` is intentionally only used to keep this function's API stable:
  // topology nodes come from runtime peer identities, never from console
  // client URLs or their CDN/FRP addresses.
  void devices
  const topology = {
    nodes: [...nodes.values()],
    links: [...links.values()],
    networkIdentities: [...networkIdentities].sort(),
  }
  trafficTracker?.update(topology.links, trafficObservations, sampleTime)
  return topology
}

interface CachedDevice {
  device: Utils.DeviceInfo
  seenAt: number
}

interface CachedSnapshots {
  snapshots: NetworkSnapshot[]
  updatedAt: number
}

/** Retains successful observations briefly while respecting confirmed instance stops. */
export class TopologySnapshotCache {
  private devices = new Map<string, CachedDevice>()
  private snapshots = new Map<string, CachedSnapshots>()

  clear() {
    this.devices.clear()
    this.snapshots.clear()
  }

  nextExpiry(): number | undefined {
    let next: number | undefined
    for (const [machineId, cached] of this.devices) {
      const deviceExpiry = cached.seenAt + TOPOLOGY_CACHE_TTL_MS
      next = next === undefined ? deviceExpiry : Math.min(next, deviceExpiry)
      const saved = this.snapshots.get(machineId)
      if (saved?.snapshots.length)
        next = Math.min(next, saved.updatedAt + TOPOLOGY_CACHE_TTL_MS)
    }
    return next
  }

  updateDevices(devices: Utils.DeviceInfo[], time: number) {
    for (const device of devices) {
      this.devices.set(device.machine_id, { device, seenAt: time })
      if (device.running_network_count === 0) {
        this.snapshots.delete(device.machine_id)
      } else if (device.running_network_instances) {
        const running = new Set(device.running_network_instances)
        const cached = this.snapshots.get(device.machine_id)
        if (cached)
          cached.snapshots = cached.snapshots.filter(snapshot => running.has(snapshot.instanceId))
      }
    }
  }

  updateSnapshots(device: Utils.DeviceInfo, snapshots: NetworkSnapshot[], time: number): boolean {
    const running = device.running_network_instances ? new Set(device.running_network_instances) : undefined
    const accepted = snapshots.filter(snapshot => snapshot.detail.running && (!running || running.has(snapshot.instanceId)))
    if (device.running_network_count > 0 && accepted.length === 0)
      return false
    this.snapshots.set(device.machine_id, {
      snapshots: accepted,
      updatedAt: time,
    })
    return true
  }

  read(time: number, freshMachines: ReadonlySet<string> = new Set()): {
    devices: Utils.DeviceInfo[]
    snapshots: NetworkSnapshot[]
  } {
    const snapshots: NetworkSnapshot[] = []
    for (const [machineId, cached] of this.devices) {
      if (time - cached.seenAt >= TOPOLOGY_CACHE_TTL_MS) {
        this.devices.delete(machineId)
        this.snapshots.delete(machineId)
        continue
      }
      const saved = this.snapshots.get(machineId)
      if (!saved)
        continue
      if (time - saved.updatedAt >= TOPOLOGY_CACHE_TTL_MS) {
        this.snapshots.delete(machineId)
        continue
      }
      snapshots.push(...saved.snapshots.map(snapshot => ({
        ...snapshot,
        device: cached.device,
        stale: !freshMachines.has(machineId) || undefined,
      })))
    }
    return { devices: [...this.devices.values()].map(cached => cached.device), snapshots }
  }
}

export function isTopologyAuthError(error: unknown): boolean {
  const status = (error as { response?: { status?: number } } | undefined)?.response?.status
  return status === 401 || status === 403
}

export async function collectTopologySnapshotsWithRetry(
  device: Utils.DeviceInfo,
  collect: () => Promise<Record<string, NetworkTypes.NetworkInstanceRunningInfo | undefined>>,
  isCurrent: () => boolean = () => true,
  wait: (milliseconds: number) => Promise<void> = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds)),
): Promise<NetworkSnapshot[]> {
  const delays = [200, 500]
  for (let attempt = 0; ; attempt++) {
    if (!isCurrent())
      throw new Error('Topology request was superseded')
    try {
      const infos = await collect()
      if (!isCurrent())
        throw new Error('Topology request was superseded')
      const running = device.running_network_instances ? new Set(device.running_network_instances) : undefined
      const snapshots = Object.entries(infos).flatMap(([instanceId, detail]) =>
        detail?.running && (!running || running.has(instanceId)) ? [{ device, instanceId, detail }] : [])
      if (device.running_network_count > 0 && !snapshots.length)
        throw new Error('Running network information is temporarily empty')
      return snapshots
    } catch (error) {
      if (!isCurrent() || isTopologyAuthError(error) || attempt >= delays.length)
        throw error
      await wait(delays[attempt])
    }
  }
}
