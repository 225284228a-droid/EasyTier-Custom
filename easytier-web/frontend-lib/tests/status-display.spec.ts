import { describe, expect, it } from 'vitest'
import { estimatedBandwidth, latencyMs, lossRate } from '../src/modules/statusDisplay'
import { ipv4ToString, ipv6ToString } from '../src/modules/utils'

function peerRoutePair(conns: any[]) {
  return {
    route: {
      ipv4_addr: '10.0.0.2',
      hostname: 'peer',
      version: 'test',
    },
    peer: {
      conns,
    },
  } as any
}

function peerRoutePairWithDefaultConn(conns: any[], defaultConnId: string) {
  const [part1, part2, part3, part4] = defaultConnId
    .replaceAll('-', '')
    .match(/.{8}/g)!
    .map((part) => Number.parseInt(part, 16))

  return {
    ...peerRoutePair(conns),
    peer: {
      default_conn_id: {
        part1,
        part2,
        part3,
        part4,
      },
      conns,
    },
  } as any
}

describe('status display helpers', () => {
  it('does not render missing IP values as zero addresses', () => {
    expect(ipv4ToString(undefined)).toBe('')
    expect(ipv4ToString(null)).toBe('')
    expect(ipv4ToString({} as any)).toBe('0.0.0.0')
    expect(ipv4ToString({ addr: 0 })).toBe('0.0.0.0')

    expect(ipv6ToString(undefined)).toBe('')
    expect(ipv6ToString(null)).toBe('')
    expect(ipv6ToString({} as any)).toBe('::0')
    expect(ipv6ToString({ part1: 0, part2: 0, part3: 0, part4: 0 })).toBe('::0')
    expect(ipv6ToString({ part4: 1 } as any)).toBe('::1')
  })

  it('skips missing latency and loss values', () => {
    expect(latencyMs(peerRoutePair([
      { conn_id: 'missing', stats: {} },
      { conn_id: 'valid', stats: { latency_us: '2500' } },
      { conn_id: 'invalid', stats: { latency_us: 'unknown' } },
    ]))).toBe('3ms')
    expect(latencyMs(peerRoutePair([
      { conn_id: 'missing', stats: {} },
      { conn_id: 'invalid', stats: { latency_us: 'unknown' } },
    ]))).toBe('')

    expect(lossRate(peerRoutePair([
      { conn_id: 'missing' },
      { conn_id: 'valid', loss_rate: '0.25' },
      { conn_id: 'invalid', loss_rate: 'unknown' },
    ]))).toBe('25%')
    expect(lossRate(peerRoutePair([
      { conn_id: 'missing' },
      { conn_id: 'invalid', loss_rate: 'unknown' },
    ]))).toBe('')
  })

  it('prefers the default connection when its metric is valid', () => {
    const defaultConnId = '00000001-0002-0003-0004-000000000005'
    const conns = [
      { conn_id: 'fallback', stats: { latency_us: '1000' }, loss_rate: '0.01' },
      { conn_id: defaultConnId, stats: { latency_us: '9000' }, loss_rate: '0.5' },
    ]

    expect(latencyMs(peerRoutePairWithDefaultConn(conns, defaultConnId))).toBe('9ms')
    expect(lossRate(peerRoutePairWithDefaultConn(conns, defaultConnId))).toBe('50%')
  })

  it('formats window bandwidth estimates and prefers the default connection', () => {
    const defaultConnId = '00000001-0002-0003-0004-000000000005'
    const conns = [
      {
        conn_id: 'fallback',
        stats: { bandwidth_estimate_version: 1, estimated_tx_bps: 12_500, estimated_rx_bps: 2_000_000 },
      },
      {
        conn_id: defaultConnId,
        stats: { bandwidth_estimate_version: 1, estimated_tx_bps: 1_250_000, estimated_rx_bps: 8_000 },
      },
    ]

    expect(estimatedBandwidth(peerRoutePairWithDefaultConn(conns, defaultConnId))).toEqual({
      upload: '1.25 Mbit/s',
      download: '8.00 kbit/s',
    })
  })

  it('shows unavailable bandwidth when a peer has no estimate', () => {
    expect(estimatedBandwidth(peerRoutePair([]))).toEqual({
      upload: '--',
      download: '--',
    })
    expect(estimatedBandwidth(peerRoutePair([{
      stats: { estimated_tx_bps: 0, estimated_rx_bps: '0' },
    }]))).toEqual({ upload: '--', download: '--' })
  })

  it('ignores the retired traffic estimator and unknown algorithm versions', () => {
    for (const bandwidth_estimate_version of [undefined, 0, 2, 'invalid']) {
      expect(estimatedBandwidth(peerRoutePair([{
        stats: { bandwidth_estimate_version, estimated_tx_bps: 1_000_000_000, estimated_rx_bps: 80_000 },
      }]))).toEqual({ upload: '--', download: '--' })
    }
  })

  it('uses only live window estimates, without mirroring a missing direction', () => {
    expect(estimatedBandwidth(peerRoutePair([
      { is_closed: true, stats: { bandwidth_estimate_version: 1, estimated_tx_bps: 9_000_000, estimated_rx_bps: 9_000_000 } },
      { stats: { bandwidth_estimate_version: '1', estimated_tx_bps: 1_000_000 } },
    ]))).toEqual({ upload: '1.00 Mbit/s', download: '--' })
  })

  it('uses a live window transport when the default lacks one, then drops it when closed', () => {
    const defaultConnId = '00000001-0002-0003-0004-000000000005'
    const udp = { conn_id: defaultConnId, stats: { bandwidth_estimate_version: 1, estimated_tx_bps: 0, estimated_rx_bps: 0 } }
    const tcp = { conn_id: 'tcp', stats: { bandwidth_estimate_version: 1, estimated_tx_bps: 25_000_000, estimated_rx_bps: 10_000_000 } }
    expect(estimatedBandwidth(peerRoutePairWithDefaultConn([udp, tcp], defaultConnId))).toEqual({
      upload: '25.0 Mbit/s',
      download: '10.0 Mbit/s',
    })
    expect(estimatedBandwidth(peerRoutePairWithDefaultConn([udp, { ...tcp, is_closed: true }], defaultConnId))).toEqual({
      upload: '--',
      download: '--',
    })
  })
})
