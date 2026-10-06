import { describe, expect, it } from 'vitest'
import { buildTopology } from '../../frontend/src/modules/networkTopology'
import { locateNode } from '../../frontend/src/modules/globeGeography'

const device = (id: string, instances: string[]) => ({
  machine_id: id, hostname: id, running_network_instances: instances,
}) as any
const route = (peerId: number, instance: string, cost = 1) => ({
  peer_id: peerId, inst_id: instance, hostname: instance, cost,
})
const detail = (routes: any[], peers: any[]) => ({ routes, peers }) as any
const peer = (id: number, closed = false) => ({
  peer_id: id, conns: [{ is_closed: closed, tunnel: { tunnel_type: 'udp' } }],
})

describe('dashboard topology', () => {
  it('deduplicates reverse connections and resolves instance IDs across machines', () => {
    const a = device('a', ['instance-a'])
    const b = device('b', ['instance-b'])
    const topology = buildTopology([a, b], [
      { device: a, instanceId: 'instance-a', detail: detail([route(7, 'instance-b')], [peer(7)]) },
      { device: b, instanceId: 'instance-b', detail: detail([route(8, 'instance-a')], [peer(8)]) },
    ])
    expect(topology.nodes).toHaveLength(2)
    expect(topology.links).toHaveLength(1)
    expect(topology.links[0].protocols).toEqual(['udp'])
  })

  it('does not turn relay routes or closed connections into direct links', () => {
    const a = device('a', ['instance-a'])
    const topology = buildTopology([a], [{
      device: a, instanceId: 'instance-a',
      detail: detail([route(7, 'instance-b', 3), route(8, 'instance-c')], [peer(8, true)]),
    }])
    expect(topology.links).toHaveLength(0)
  })

  it('keeps unknown peer IDs scoped to separate networks', () => {
    const a = device('a', ['network-a', 'network-b'])
    const topology = buildTopology([a], [
      { device: a, instanceId: 'network-a', detail: detail([], [peer(7)]) },
      { device: a, instanceId: 'network-b', detail: detail([], [peer(7)]) },
    ])
    expect(topology.nodes).toHaveLength(3)
    expect(topology.links).toHaveLength(2)
  })

  it('disambiguates copied instance IDs using the remote peer identity', () => {
    const a = device('a', ['shared'])
    const b = device('b', ['shared'])
    const c = device('c', ['shared'])
    const topology = buildTopology([a, b, c], [
      { device: a, instanceId: 'shared', detail: {
        ...detail([route(2, 'shared')], [peer(2)]), my_node_info: { peer_id: 1 },
      } },
      { device: b, instanceId: 'shared', detail: {
        ...detail([], []), my_node_info: { peer_id: 2 },
      } },
      { device: c, instanceId: 'shared', detail: {
        ...detail([], []), my_node_info: { peer_id: 3 },
      } },
    ])
    expect(topology.links[0].target).toBe('machine:b')
  })

  it('uses valid GeoIP coordinates and explicitly approximate country locations', () => {
    const node = { id: 'a', label: 'a', managed: true }
    expect(locateNode({ ...node, latitude: 0, longitude: 0 })?.approximate).toBe(false)
    expect(locateNode({ ...node, country: '中国' })?.approximate).toBe(true)
    expect(locateNode({ ...node, latitude: 1000, longitude: NaN })).toBeUndefined()
    expect(locateNode(node)).toBeUndefined()
  })
})
