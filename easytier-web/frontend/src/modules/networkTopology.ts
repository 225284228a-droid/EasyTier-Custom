import { Utils } from 'easytier-frontend-lib'
import type { NetworkTypes } from 'easytier-frontend-lib'

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
}

export interface TopologyLink {
  source: string
  target: string
  protocols: string[]
}

export interface NetworkSnapshot {
  device: Utils.DeviceInfo
  instanceId: string
  detail: NetworkTypes.NetworkInstanceRunningInfo
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
  return {
    ...existing,
    // A node observed from a managed snapshot is authoritative over a route
    // placeholder. Keep the first non-empty runtime metadata otherwise.
    label: existing.managed ? existing.label : incoming.label,
    country: existing.country ?? incoming.country,
    latitude: existing.latitude ?? incoming.latitude,
    longitude: existing.longitude ?? incoming.longitude,
    nodeLocation: existing.nodeLocation ?? incoming.nodeLocation,
    publicIp: existing.publicIp ?? incoming.publicIp,
    managed: existing.managed || incoming.managed,
    machineId: existing.machineId ?? incoming.machineId,
  }
}

export function buildTopology(devices: Utils.DeviceInfo[], snapshots: NetworkSnapshot[]): Topology {
  const nodes = new Map<string, TopologyNode>()
  const links = new Map<string, TopologyLink>()
  const networkIdentities = new Set<string>()
  const namesByInstance = new Map<string, Set<string>>()
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
      }
      nodes.set(target, mergeNode(nodes.get(target), placeholder))

      const key = [source, target].sort().join('|')
      const existing = links.get(key)
      const protocols = [...new Set([
        ...(existing?.protocols ?? []),
        ...openConns
          .map(conn => cleanText(conn.tunnel?.tunnel_type))
          .filter((value): value is string => !!value),
      ])].sort()
      links.set(key, { source, target, protocols })
    }
  }

  // `devices` is intentionally only used to keep this function's API stable:
  // topology nodes come from runtime peer identities, never from console
  // client URLs or their CDN/FRP addresses.
  void devices
  return {
    nodes: [...nodes.values()],
    links: [...links.values()],
    networkIdentities: [...networkIdentities].sort(),
  }
}
