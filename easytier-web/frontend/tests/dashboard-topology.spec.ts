import { describe, expect, it, vi } from 'vitest'
import { buildTopology, collectTopologySnapshotsWithRetry, TopologySnapshotCache } from '../src/modules/networkTopology'
import { TrafficTracker } from '../src/modules/topologyTraffic'
import { locateNode } from '../src/modules/globeGeography'

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
  it('keeps traffic rates and RTT separate when device pairs reuse a connection ID and coordinates', () => {
    const snapshots = [1, 2, 3, 4].map(id => {
      const connection = id % 2 ? peer(id + 1) : undefined
      if (connection) {
        Object.assign(connection.conns[0], {
          conn_id: 'reused-channel-id',
          my_peer_id: id,
          stats: { tx_bytes: 0, rx_bytes: 0, latency_us: id === 1 ? 5_000 : 150_000 },
        })
      }
      const instanceId = `instance-${id}`
      const current = snapshot(device(`device-${id}`, [instanceId]), instanceId, id, [], connection ? [connection] : [])
      current.detail.node_location = { country: 'Test', latitude: 1, longitude: id % 2 ? 10 : 20 }
      return current
    })
    const tracker = new TrafficTracker()
    buildTopology([], snapshots, tracker, 1_000)
    Object.assign(snapshots[0].detail.peers[0].conns[0].stats, { tx_bytes: 250_000, rx_bytes: 125_000 })
    Object.assign(snapshots[2].detail.peers[0].conns[0].stats, { tx_bytes: 2_500_000, rx_bytes: 25_000 })
    const topology = buildTopology([], snapshots, tracker, 2_000)
    expect(topology.nodes).toHaveLength(4)
    expect(topology.links).toHaveLength(2)
    expect(topology.links.find(link => link.source.endsWith(':peer:1')))
      .toMatchObject({ txBps: 2_000_000, rxBps: 1_000_000, latencyMs: 5 })
    expect(topology.links.find(link => link.source.endsWith(':peer:3')))
      .toMatchObject({ txBps: 20_000_000, rxBps: 200_000, latencyMs: 150 })
  })

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

  it('keeps source and target stable when snapshot order changes and samples both directions', () => {
    const a = device('a', ['instance-a'])
    const b = device('b', ['instance-b'])
    const tracker = new TrafficTracker()
    const aPeer = peer(2)
    const bPeer = peer(1)
    const aSnapshot = snapshot(a, 'instance-a', 1, [], [aPeer])
    const bSnapshot = snapshot(b, 'instance-b', 2, [], [bPeer])
    Object.assign(aPeer.conns[0], { stats: { tx_bytes: 100, rx_bytes: 200 } })
    Object.assign(bPeer.conns[0], { stats: { tx_bytes: 200, rx_bytes: 100 } })
    const first = buildTopology([a, b], [aSnapshot, bSnapshot], tracker, 1_000)
    expect(first.links[0].txBps).toBeUndefined()
    Object.assign(aPeer.conns[0], { stats: { tx_bytes: 150, rx_bytes: 300 } })
    Object.assign(bPeer.conns[0], { stats: { tx_bytes: 300, rx_bytes: 150 } })
    const second = buildTopology([a, b], [bSnapshot, aSnapshot], tracker, 2_000)
    expect(second.links[0]).toMatchObject({
      source: first.links[0].source,
      target: first.links[0].target,
      txBps: 400,
      rxBps: 800,
    })
  })

  it('samples each machine at RPC completion instead of diluting rates with a slow batch', () => {
    const a = device('a', ['mesh'])
    const b = device('b', ['mesh'])
    const tracker = new TrafficTracker()
    const aPeer = peer(2)
    const bPeer = peer(4)
    const aSnapshot = { ...snapshot(a, 'mesh', 1, [], [aPeer]), collectedAt: 1_000 }
    const bSnapshot = { ...snapshot(b, 'mesh', 3, [], [bPeer]), collectedAt: 10_000 }
    Object.assign(aPeer.conns[0], { stats: { tx_bytes: 100, rx_bytes: 200 } })
    Object.assign(bPeer.conns[0], { stats: { tx_bytes: 200, rx_bytes: 400 } })
    buildTopology([a, b], [aSnapshot, bSnapshot], tracker, 15_000)
    aSnapshot.collectedAt = 2_000
    bSnapshot.collectedAt = 20_000
    Object.assign(aPeer.conns[0], { stats: { tx_bytes: 200, rx_bytes: 400 } })
    Object.assign(bPeer.conns[0], { stats: { tx_bytes: 500, rx_bytes: 900 } })
    const topology = buildTopology([a, b], [aSnapshot, bSnapshot], tracker, 30_000)
    expect(topology.links.find(link => link.source.endsWith(':peer:1'))).toMatchObject({
      txBps: 800,
      rxBps: 1600,
    })
    expect(topology.links.find(link => link.source.endsWith(':peer:3'))).toMatchObject({
      txBps: 240,
      rxBps: 400,
    })
  })

  it('aggregates open UDP and WSS counters but excludes closed channels', () => {
    const a = device('a', ['mesh'])
    const tracker = new TrafficTracker()
    const currentPeer = peer(2)
    currentPeer.conns.push({ ...currentPeer.conns[0], conn_id: 'wss', tunnel: { tunnel_type: 'wss' } })
    currentPeer.conns.push({ ...currentPeer.conns[0], conn_id: 'closed', is_closed: true })
    for (const conn of currentPeer.conns)
      Object.assign(conn, { stats: { tx_bytes: 100, rx_bytes: 200 } })
    const current = snapshot(a, 'mesh', 1, [], [currentPeer])
    buildTopology([a], [current], tracker, 1_000)
    for (const conn of currentPeer.conns)
      Object.assign(conn, { stats: { tx_bytes: 125, rx_bytes: 250 } })
    expect(buildTopology([a], [current], tracker, 2_000).links[0]).toMatchObject({
      protocols: ['udp', 'wss'],
      txBps: 400,
      rxBps: 800,
    })
  })

  it('treats omitted protobuf JSON counters in an existing stats object as zero', () => {
    const a = device('a', ['mesh'])
    const tracker = new TrafficTracker()
    const currentPeer = peer(2)
    Object.assign(currentPeer.conns[0], { stats: {} })
    const current = snapshot(a, 'mesh', 1, [], [currentPeer])
    const first = buildTopology([a], [current], tracker, 1_000)
    expect(first.links[0].txBps).toBeUndefined()
    expect(first.links[0].rxBps).toBeUndefined()
    expect(buildTopology([a], [current], tracker, 2_000).links[0]).toMatchObject({
      txBps: 0,
      rxBps: 0,
    })
  })

  it('does not convert cached snapshots into new zero-rate samples', () => {
    const a = device('a', ['mesh'])
    const tracker = new TrafficTracker()
    const currentPeer = peer(2)
    const current = snapshot(a, 'mesh', 1, [], [currentPeer])
    Object.assign(currentPeer.conns[0], { stats: { tx_bytes: 100, rx_bytes: 200 } })
    buildTopology([a], [current], tracker, 1_000)
    Object.assign(currentPeer.conns[0], { stats: { tx_bytes: 150, rx_bytes: 300 } })
    buildTopology([a], [current], tracker, 2_000)
    const cached = buildTopology([a], [{ ...current, stale: true }], tracker, 4_000)
    expect(cached.links[0]).toMatchObject({ stale: true, txBps: 400, rxBps: 800 })
    expect(cached.nodes.find(node => node.peerId === 1)?.stale).toBe(true)
    expect(buildTopology([a], [current], tracker, 5_000).links[0]).toMatchObject({
      txBps: 0,
      rxBps: 0,
    })
  })

  it('uses a fresh endpoint report even when the other endpoint is cached', () => {
    const a = device('a', ['mesh'])
    const b = device('b', ['mesh'])
    const tracker = new TrafficTracker()
    const aPeer = peer(2)
    const bPeer = peer(1)
    const aSnapshot = { ...snapshot(a, 'mesh', 1, [], [aPeer]), stale: true }
    const bSnapshot = snapshot(b, 'mesh', 2, [], [bPeer])
    Object.assign(aPeer.conns[0], { stats: { tx_bytes: 1_000, rx_bytes: 2_000 } })
    Object.assign(bPeer.conns[0], { stats: { tx_bytes: 100, rx_bytes: 200 } })
    buildTopology([a, b], [aSnapshot, bSnapshot], tracker, 1_000)
    Object.assign(bPeer.conns[0], { stats: { tx_bytes: 150, rx_bytes: 300 } })
    const topology = buildTopology([a, b], [aSnapshot, bSnapshot], tracker, 2_000)
    expect(topology.links[0]).toMatchObject({ txBps: 800, rxBps: 400 })
    expect(topology.links[0].stale).toBeUndefined()
    expect(topology.nodes.find(node => node.peerId === 1)?.stale).toBe(true)
    expect(topology.nodes.find(node => node.peerId === 2)?.stale).toBeUndefined()
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

describe('dashboard snapshot cache and retry', () => {
  const runningDevice = (id = 'a', instances = ['mesh']) => ({
    ...device(id, instances),
    running_network_count: instances.length,
  })

  it('reports the earliest device or snapshot expiry and advances only after expired data is read', () => {
    const cache = new TopologySnapshotCache()
    const a = runningDevice('a')
    const b = runningDevice('b')
    expect(cache.nextExpiry()).toBeUndefined()
    cache.updateDevices([a], 0)
    cache.updateSnapshots(a, [snapshot(a, 'mesh', 1, [], [])], 2_000)
    expect(cache.nextExpiry()).toBe(60_000)
    cache.updateDevices([a], 10_000)
    cache.updateDevices([b], 1_000)
    expect(cache.nextExpiry()).toBe(61_000)
    cache.read(61_000)
    expect(cache.nextExpiry()).toBe(62_000)
    cache.read(62_000)
    expect(cache.nextExpiry()).toBe(70_000)
    cache.read(70_000)
    expect(cache.nextExpiry()).toBeUndefined()
    cache.updateDevices([a], 80_000)
    cache.clear()
    expect(cache.nextExpiry()).toBeUndefined()
  })

  it('retains missing machines and failed snapshots as stale for no more than sixty seconds', () => {
    const cache = new TopologySnapshotCache()
    const a = runningDevice()
    const current = snapshot(a, 'mesh', 1, [], [peer(2)])
    cache.updateDevices([a], 0)
    cache.updateSnapshots(a, [current], 0)
    expect(cache.read(1_000, new Set(['a'])).snapshots[0].stale).toBeUndefined()
    cache.updateDevices([], 2_000)
    const stale = cache.read(59_999)
    expect(stale.devices).toHaveLength(1)
    expect(stale.snapshots[0].stale).toBe(true)
    expect(cache.read(60_000)).toEqual({ devices: [], snapshots: [] })
  })

  it('does not extend the snapshot lifetime merely because machine heartbeats succeed', () => {
    const cache = new TopologySnapshotCache()
    const a = runningDevice()
    cache.updateDevices([a], 0)
    cache.updateSnapshots(a, [snapshot(a, 'mesh', 1, [], [peer(2)])], 0)
    cache.updateDevices([a], 59_000)
    expect(cache.read(60_000).devices).toHaveLength(1)
    expect(cache.read(60_000).snapshots).toHaveLength(0)
  })

  it('keeps the previous snapshot when heartbeat says running but collection is empty', () => {
    const cache = new TopologySnapshotCache()
    const a = runningDevice()
    cache.updateDevices([a], 0)
    cache.updateSnapshots(a, [snapshot(a, 'mesh', 1, [], [peer(2)])], 0)
    expect(cache.updateSnapshots(a, [], 2_000)).toBe(false)
    expect(cache.read(2_000).snapshots).toHaveLength(1)
  })

  it('removes confirmed stopped instances and ignores returned instances absent from the heartbeat', () => {
    const cache = new TopologySnapshotCache()
    const a = runningDevice('a', ['alpha', 'beta'])
    cache.updateDevices([a], 0)
    cache.updateSnapshots(a, [
      snapshot(a, 'alpha', 1, [], []),
      snapshot(a, 'beta', 2, [], []),
    ], 0)
    const onlyBeta = runningDevice('a', ['beta'])
    cache.updateDevices([onlyBeta], 1_000)
    expect(cache.read(1_000).snapshots.map(item => item.instanceId)).toEqual(['beta'])
    expect(cache.updateSnapshots(onlyBeta, [snapshot(a, 'alpha', 1, [], [])], 2_000)).toBe(false)
    expect(cache.read(2_000).snapshots.map(item => item.instanceId)).toEqual(['beta'])
    cache.updateDevices([runningDevice('a', [])], 3_000)
    expect(cache.read(3_000).snapshots).toHaveLength(0)
  })

  it('keeps caches scoped by machine ID and clears them for a server switch', () => {
    const cache = new TopologySnapshotCache()
    const a = runningDevice('a')
    const b = runningDevice('b')
    cache.updateDevices([a, b], 0)
    cache.updateSnapshots(a, [snapshot(a, 'mesh', 1, [], [])], 0)
    cache.updateSnapshots(b, [snapshot(b, 'mesh', 2, [], [])], 0)
    expect(cache.read(1_000, new Set(['b'])).snapshots.map(item => item.stale)).toEqual([true, undefined])
    cache.clear()
    expect(cache.read(2_000)).toEqual({ devices: [], snapshots: [] })
  })

  it('retries transient collection failures twice with bounded backoff', async () => {
    const a = runningDevice()
    const collect = vi.fn()
      .mockRejectedValueOnce(new Error('temporary'))
      .mockRejectedValueOnce(new Error('temporary'))
      .mockResolvedValue({ mesh: detail([], [peer(2)], 1, 'mesh') })
    const wait = vi.fn().mockResolvedValue(undefined)
    expect(await collectTopologySnapshotsWithRetry(a, collect, () => true, wait)).toHaveLength(1)
    expect(collect).toHaveBeenCalledTimes(3)
    expect(wait.mock.calls.map(call => call[0])).toEqual([200, 500])
  })

  it('treats empty running information as transient and caps retries at three attempts', async () => {
    const collect = vi.fn().mockResolvedValue({})
    const wait = vi.fn().mockResolvedValue(undefined)
    await expect(collectTopologySnapshotsWithRetry(runningDevice(), collect, () => true, wait))
      .rejects.toThrow('temporarily empty')
    expect(collect).toHaveBeenCalledTimes(3)
    expect(wait.mock.calls.map(call => call[0])).toEqual([200, 500])
  })

  it.each([401, 403])('does not retry authorization status %s', async (status) => {
    const error = { response: { status } }
    const collect = vi.fn().mockRejectedValue(error)
    const wait = vi.fn().mockResolvedValue(undefined)
    await expect(collectTopologySnapshotsWithRetry(runningDevice(), collect, () => true, wait)).rejects.toBe(error)
    expect(collect).toHaveBeenCalledTimes(1)
    expect(wait).not.toHaveBeenCalled()
  })

  it('rejects old in-flight collection results after a server switch', async () => {
    let current = true
    let finish!: (value: any) => void
    const collect = vi.fn(() => new Promise<Record<string, any>>(resolve => { finish = resolve }))
    const wait = vi.fn().mockResolvedValue(undefined)
    const pending = collectTopologySnapshotsWithRetry(runningDevice(), collect, () => current, wait)
    current = false
    finish({ mesh: detail([], [], 1, 'mesh') })
    await expect(pending).rejects.toThrow('superseded')
    expect(wait).not.toHaveBeenCalled()
  })

  it('does not start another retry once a server switch occurred during backoff', async () => {
    let current = true
    const collect = vi.fn().mockRejectedValue(new Error('temporary'))
    const wait = vi.fn(async () => { current = false })
    await expect(collectTopologySnapshotsWithRetry(runningDevice(), collect, () => current, wait))
      .rejects.toThrow('superseded')
    expect(collect).toHaveBeenCalledTimes(1)
  })
})
