import { describe, expect, it } from 'vitest'
import { buildTopology } from '../../frontend/src/modules/networkTopology'

const snapshot = (latency: unknown, stale = false, closed = false) => ({
  device: { machine_id: 'a', hostname: 'node-a' } as any,
  instanceId: 'mesh',
  stale,
  detail: {
    running: true,
    network_name: 'mesh',
    my_node_info: { peer_id: 1 },
    peers: [{
      peer_id: 2,
      conns: [{ conn_id: 'udp', is_closed: closed, stats: { latency_us: latency } }],
    }],
  } as any,
})

describe('runtime topology link latency', () => {
  it('converts measured microseconds into milliseconds', () => {
    const link = buildTopology([], [snapshot('15000')]).links[0]
    expect(link.latencyMs).toBe(15)
  })

  it.each([0, -1, '0', '-1', '', '1e3', 'NaN', '1.5', 0.1, null, undefined, Infinity, Number.MAX_SAFE_INTEGER + 1])(
    'does not invent a latency for an invalid or missing sample %s', value => {
      expect(buildTopology([], [snapshot(value)]).links[0].latencyMs).toBeUndefined()
    },
  )

  it('averages distinct fresh channels and ignores duplicate connection reports', () => {
    const current = snapshot(10_000)
    const peer = current.detail.peers[0]
    peer.conns.push({ conn_id: 'wss', stats: { latency_us: 30_000 } })
    peer.conns.push({ conn_id: 'udp', stats: { latency_us: 10_000 } })
    expect(buildTopology([], [current]).links[0].latencyMs).toBe(20)
  })

  it('uses fresh measurements without including stale reverse reports', () => {
    const old = snapshot(90_000, true)
    old.device.machine_id = 'b'
    old.detail.my_node_info.peer_id = 2
    old.detail.peers[0].peer_id = 1
    expect(buildTopology([], [snapshot(10_000), old]).links[0].latencyMs).toBe(10)
    expect(buildTopology([], [old]).links[0].latencyMs).toBeUndefined()
    expect(buildTopology([], [snapshot(10_000, false, true)]).links).toEqual([])
  })

  it('averages valid RTT reports from both endpoints', () => {
    const reverse = snapshot(30_000)
    reverse.device.machine_id = 'b'
    reverse.detail.my_node_info.peer_id = 2
    reverse.detail.peers[0].peer_id = 1
    expect(buildTopology([], [snapshot(10_000), reverse]).links[0].latencyMs).toBe(20)
  })
})
