import { describe, expect, it } from 'vitest'
import { buildTopology, type NetworkSnapshot } from '../../frontend/src/modules/networkTopology'
import { TrafficTracker } from '../../frontend/src/modules/topologyTraffic'
import { FlowEmitter, flowEmissionsPerSecond, flowTravelSeconds } from '../../frontend/src/modules/globeFlow'

function node(id: number, remote?: number): NetworkSnapshot {
  return {
    device: { machine_id: `device-${id}`, hostname: `device-${id}` } as any,
    instanceId: `instance-${id}`,
    detail: {
      running: true, network_name: 'mesh',
      my_node_info: { peer_id: id, hostname: `device-${id}` },
      node_location: { country: 'Test', latitude: 1, longitude: id % 2 ? 10 : 20 },
      peers: remote ? [{
        peer_id: remote,
        conns: [{
          conn_id: 'reused-channel-id', my_peer_id: id, peer_id: remote,
          stats: { tx_bytes: 0, rx_bytes: 0, latency_us: id === 1 ? 5_000 : 150_000 },
        }],
      }] : [],
      routes: [],
    } as any,
  }
}

describe('distinct device links on overlapping map paths', () => {
  it('keeps byte rates, RTT, and bidirectional emitters separate by device pair', () => {
    const snapshots = [node(1, 2), node(2), node(3, 4), node(4)]
    const tracker = new TrafficTracker()
    buildTopology([], snapshots, tracker, 1_000)
    Object.assign(snapshots[0].detail.peers[0].conns[0].stats!, { tx_bytes: 250_000, rx_bytes: 125_000 })
    Object.assign(snapshots[2].detail.peers[0].conns[0].stats!, { tx_bytes: 2_500_000, rx_bytes: 25_000 })
    const topology = buildTopology([], snapshots, tracker, 2_000)
    expect(topology.nodes).toHaveLength(4)
    expect(topology.links).toHaveLength(2)
    const fast = topology.links.find(link => link.source.endsWith(':peer:1'))!
    const slow = topology.links.find(link => link.source.endsWith(':peer:3'))!
    expect(fast).toMatchObject({ txBps: 2_000_000, rxBps: 1_000_000, latencyMs: 5 })
    expect(slow).toMatchObject({ txBps: 20_000_000, rxBps: 200_000, latencyMs: 150 })
    const configurations = topology.links.flatMap(link => [false, true].map(reverse => ({
      key: JSON.stringify([link.source, link.target, reverse]),
      frequency: flowEmissionsPerSecond(reverse ? link.rxBps : link.txBps),
      duration: flowTravelSeconds(link.latencyMs),
      emitter: new FlowEmitter(),
    })))
    expect(new Set(configurations.map(config => config.key)).size).toBe(4)
    configurations.forEach(config => {
      config.emitter.advance(10, config.frequency, config.duration)
      expect(config.emitter.emittedCount).toBe(Math.floor(config.frequency * 10))
    })
    expect(configurations[0].duration).toBe(0.1)
    expect(configurations[2].duration).toBe(3)
    expect(configurations[0].frequency).not.toBe(configurations[2].frequency)
    const fastProgress = [...configurations[0].emitter.progress]
    const fastEmissions = configurations[0].emitter.emittedCount
    configurations[2].emitter.advance(1, 0, configurations[2].duration)
    expect(configurations[2].emitter.progress).toEqual([])
    expect(configurations[0].emitter.progress).toEqual(fastProgress)
    expect(configurations[0].emitter.emittedCount).toBe(fastEmissions)
  })
})
