import type { NetworkTypes, Utils } from 'easytier-frontend-lib'

export interface TopologyNode {
  id: string
  label: string
  country?: string
  latitude?: number
  longitude?: number
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

export function buildTopology(devices: Utils.DeviceInfo[], snapshots: NetworkSnapshot[]) {
  const nodes = new Map<string, TopologyNode>()
  const links = new Map<string, TopologyLink>()
  const instanceToNodes = new Map<string, Set<string>>()
  const identityToNodes = new Map<string, Set<string>>()
  function addIdentity(map: Map<string, Set<string>>, key: string, nodeId: string) {
    const candidates = map.get(key) ?? new Set<string>()
    candidates.add(nodeId)
    map.set(key, candidates)
  }
  function uniqueNode(candidates?: Set<string>) {
    return candidates?.size === 1 ? [...candidates][0] : undefined
  }

  for (const device of devices) {
    const id = `machine:${device.machine_id}`
    nodes.set(id, {
      id,
      label: device.hostname || device.machine_id,
      country: device.location?.country,
      latitude: device.location?.latitude,
      longitude: device.location?.longitude,
      managed: true,
      machineId: device.machine_id,
    })
    for (const instanceId of device.running_network_instances ?? [])
      addIdentity(instanceToNodes, instanceId, id)
  }
  for (const snapshot of snapshots) {
    const peerId = snapshot.detail.my_node_info?.peer_id
    if (peerId !== undefined) {
      addIdentity(identityToNodes, `${snapshot.instanceId}:${peerId}`,
        `machine:${snapshot.device.machine_id}`)
    }
  }

  for (const snapshot of snapshots) {
    const source = `machine:${snapshot.device.machine_id}`
    const routes = new Map(snapshot.detail.routes?.map(route => [route.peer_id, route]) ?? [])
    for (const pair of snapshot.detail.peer_route_pairs ?? []) {
      if (pair.route)
        routes.set(pair.route.peer_id, pair.route)
    }

    // Only connection records establish edges. A relay route is not a direct link.
    for (const peer of snapshot.detail.peers ?? []) {
      const conns = peer.conns?.filter(conn => !conn.is_closed) ?? []
      if (!conns.length)
        continue
      const route = routes.get(peer.peer_id)
      const instanceId = route?.inst_id
      const target = instanceId
        ? (uniqueNode(identityToNodes.get(`${instanceId}:${peer.peer_id}`))
          ?? uniqueNode(instanceToNodes.get(instanceId))
          ?? `instance:${instanceId}:peer:${peer.peer_id}`)
        : `unmanaged:${snapshot.instanceId}:${peer.peer_id}`
      if (source === target)
        continue
      if (!nodes.has(target)) {
        nodes.set(target, {
          id: target,
          label: route?.hostname || `Peer ${peer.peer_id}`,
          managed: false,
        })
      }
      const key = [source, target].sort().join('|')
      const existing = links.get(key)
      const protocols = [...new Set([
        ...(existing?.protocols ?? []),
        ...conns.map(conn => conn.tunnel?.tunnel_type).filter((value): value is string => !!value),
      ])].sort()
      links.set(key, { source, target, protocols })
    }
  }

  return { nodes: [...nodes.values()], links: [...links.values()] }
}
