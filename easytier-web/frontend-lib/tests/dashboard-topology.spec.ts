import { describe, expect, it } from 'vitest'
import { buildTopology } from '../../frontend/src/modules/networkTopology'
import { locateNode } from '../../frontend/src/modules/globeGeography'

const device = (id: string, instances: string[]) => ({
  machine_id: id,
  hostname: id,
  running_network_instances: instances,
}) as any

const route = (peerId: number, instance: string, cost = 1) => ({
  peer_id: peerId,
  inst_id: instance,
  hostname: `peer-${peerId}`,
  cost,
})

const peer = (id: number, closed = false, networkName = 'mesh') => ({
  peer_id: id,
  conns: [{
    conn_id: `conn-${id}`,
    peer_id: id,
    my_peer_id: 1,
    is_client: true,
    is_closed: closed,
    network_name: networkName,
    tunnel: { tunnel_type: 'udp' },
  }],
})

const detail = (
  routes: any[],
  peers: any[],
  myPeerId: number,
  networkName?: string,
) => ({
  routes,
  peers,
  peer_route_pairs: [],
  my_node_info: { peer_id: myPeerId, hostname: `node-${myPeerId}` },
  running: true,
  ...(networkName ? { network_name: networkName } : {}),
}) as any

const snapshot = (
  currentDevice: any,
  instanceId: string,
  myPeerId: number,
  routes: any[],
  peers: any[],
  networkName = 'mesh',
) => ({
  device: currentDevice,
  instanceId,
  detail: detail(routes, peers, myPeerId, networkName),
})

describe('dashboard topology', () => {
  it('deduplicates reverse connections and uses runtime peer identities', () => {
    const a = device('a', ['instance-a'])
    const b = device('b', ['instance-b'])
    const topology = buildTopology([a, b], [
      snapshot(a, 'instance-a', 1, [route(2, 'wrong-instance')], [peer(2)]),
      snapshot(b, 'instance-b', 2, [route(1, 'wrong-instance')], [peer(1)]),
    ])
    expect(topology.nodes).toHaveLength(2)
    expect(topology.links).toHaveLength(1)
    expect(topology.links[0].protocols).toEqual(['udp'])
    expect(topology.networkIdentities).toEqual(['name:mesh'])
    expect(topology.nodes.find(node => node.peerId === 2)?.machineId).toBe('b')
  })

  it('does not turn relay routes or closed connections into direct links', () => {
    const a = device('a', ['instance-a'])
    const topology = buildTopology([a], [
      snapshot(a, 'instance-a', 1, [route(7, 'instance-b', 3)], [peer(7, true)]),
    ])
    expect(topology.nodes).toHaveLength(1)
    expect(topology.links).toHaveLength(0)
  })

  it('keeps unknown peer IDs scoped to separate runtime networks', () => {
    const a = device('a', ['network-a', 'network-b'])
    const topology = buildTopology([a], [
      snapshot(a, 'network-a', 1, [], [peer(7, false, 'alpha')], 'alpha'),
      snapshot(a, 'network-b', 2, [], [peer(7, false, 'beta')], 'beta'),
    ])
    expect(topology.nodes).toHaveLength(4)
    expect(topology.links).toHaveLength(2)
    expect(topology.networkIdentities).toEqual(['name:alpha', 'name:beta'])
  })

  it('does not create duplicate nodes when instance IDs are copied', () => {
    const a = device('a', ['shared'])
    const b = device('b', ['shared'])
    const c = device('c', ['shared'])
    const topology = buildTopology([a, b, c], [
      snapshot(a, 'shared', 1, [route(2, 'shared')], [peer(2)]),
      snapshot(b, 'shared', 2, [], []),
      snapshot(c, 'shared', 3, [], []),
    ])
    expect(topology.nodes).toHaveLength(3)
    expect(topology.links[0].target).toContain(':peer:2')
    expect(topology.nodes.find(node => node.peerId === 2)?.machineId).toBe('b')
  })

  it('deduplicates a remote peer seen through several local nodes', () => {
    const a = device('a', ['mesh'])
    const b = device('b', ['mesh'])
    const c = device('c', ['mesh'])
    const topology = buildTopology([a, b, c], [
      snapshot(a, 'mesh', 1, [], [peer(9)]),
      snapshot(b, 'mesh', 2, [], [peer(9)]),
      snapshot(c, 'mesh', 3, [], [peer(9)]),
    ])
    expect(topology.nodes.filter(node => node.peerId === 9)).toHaveLength(1)
    expect(topology.links).toHaveLength(3)
  })

  it('counts eight peers in one network once despite copied instances and proxy URLs', () => {
    const devices = Array.from({ length: 8 }, (_, index) => ({
      ...device(`machine-${index + 1}`, ['copied-instance']),
      public_ip: 'https://console.cdn.example/',
      location: { country: 'Unknown', latitude: 51, longitude: -1 },
    }))
    const snapshots = devices.map((currentDevice, index) => {
      const nextPeerId = (index + 1) % devices.length + 1
      const connection = peer(nextPeerId)
      connection.conns.push({
        ...connection.conns[0],
        conn_id: `frp-${nextPeerId}`,
        tunnel: { tunnel_type: 'wss' },
      })
      return snapshot(currentDevice, 'copied-instance', index + 1, [
        route(nextPeerId, `ghost-instance-${index}`),
      ], [connection])
    })
    const topology = buildTopology(devices, snapshots)

    expect(topology.nodes).toHaveLength(8)
    expect(topology.links).toHaveLength(8)
    expect(topology.networkIdentities).toEqual(['name:mesh'])
    expect(topology.nodes.every(node => node.managed)).toBe(true)
    expect(topology.nodes.every(node => node.publicIp === undefined)).toBe(true)
    expect(topology.nodes.every(node => node.latitude === undefined)).toBe(true)
    expect(topology.links.every(link => link.protocols.join(',') === 'udp,wss')).toBe(true)
  })

  it('gets location and public IP only from the running node snapshot', () => {
    const a = {
      ...device('a', ['mesh']),
      location: { country: 'Proxy', latitude: 10, longitude: 10 },
      public_ip: 'https://cdn.example/',
    }
    const current = snapshot(a, 'mesh', 1, [], [])
    current.detail.node_location = {
      public_ip: '1.1.1.1',
      country: 'Australia',
      latitude: -33.86,
      longitude: 151.21,
    }
    const topology = buildTopology([a], [current])
    expect(topology.nodes[0].country).toBe('Australia')
    expect(topology.nodes[0].latitude).toBe(-33.86)
    expect(topology.nodes[0].publicIp).toBe('1.1.1.1')
  })

  it('uses valid GeoIP coordinates and explicitly approximate country locations', () => {
    const node = {
      id: 'a',
      label: 'a',
      networkIdentity: 'name:mesh',
      peerId: 1,
      managed: true,
    }
    expect(locateNode({ ...node, latitude: 0, longitude: 0 })?.approximate).toBe(false)
    expect(locateNode({ ...node, country: '中国' })?.approximate).toBe(true)
    expect(locateNode({ ...node, country: 'US' })?.approximate).toBe(true)
    expect(locateNode({ ...node, latitude: 1000, longitude: NaN })).toBeUndefined()
    expect(locateNode(node)).toBeUndefined()
  })

  it('locates small countries and territories absent from the low-detail coastlines', () => {
    const node = {
      id: 'a',
      label: 'a',
      networkIdentity: 'name:mesh',
      peerId: 1,
      managed: true,
    }
    for (const [country, latitude, longitude] of [
      ['SG', 1.366587, 103.816925],
      ['Hong Kong', 22.448829, 114.097769],
      ['澳门', 22.129735, 113.556038],
    ] as const) {
      const location = locateNode({ ...node, country })
      expect(location?.approximate).toBe(true)
      expect(location?.latitude).toBeCloseTo(latitude, 5)
      expect(location?.longitude).toBeCloseTo(longitude, 5)
    }
  })
})
