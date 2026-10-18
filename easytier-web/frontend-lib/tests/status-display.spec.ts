import { describe, expect, it } from 'vitest'
import { estimatedBandwidth, estimatedConnectionBandwidth, totalTraffic, latencyMs, lossRate, tunnelProtocols, udpNatTypeName } from '../src/modules/statusDisplay'
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

  it('skips missing latency and invalid loss values', () => {
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
      { conn_id: 'valid', loss_rate: '0.25' },
      { conn_id: 'invalid', loss_rate: 'unknown' },
    ]))).toBe('25%')
    expect(lossRate(peerRoutePair([
      { conn_id: 'invalid', loss_rate: 'unknown' },
    ]))).toBe('')
  })

  it('renders omitted protobuf loss as zero only for an existing connection', () => {
    expect(lossRate(peerRoutePair([{ conn_id: 'zero' }]))).toBe('0%')
    expect(lossRate(peerRoutePair([{ conn_id: 'zero', loss_rate: 0 }]))).toBe('0%')
    expect(lossRate(peerRoutePair([
      { conn_id: 'zero' },
      { conn_id: 'lossy', loss_rate: 0.7 },
    ]))).toBe('0%')
    expect(lossRate(peerRoutePair([]))).toBe('')
    expect(lossRate({ route: {} } as any)).toBe('')
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

  it('reuses valid directional estimates and traffic on central-node connection rows', () => {
    const conn = { is_closed: false, stats: { bandwidth_estimate_version: '1', estimated_tx_bps: '1000000', tx_bytes: '2048', rx_bytes: '1024' } } as any
    expect(estimatedConnectionBandwidth(conn)).toEqual({ upload: '1.00 Mbit/s', download: '--' })
    expect(totalTraffic(peerRoutePair([conn]))).toEqual({ upload: '2.0 KiB', download: '1.0 KiB' })
    expect(estimatedConnectionBandwidth({ ...conn, is_closed: true })).toEqual({ upload: '--', download: '--' })
    expect(totalTraffic(peerRoutePair([]))).toEqual({ upload: '--', download: '--' })
  })

  it('accepts a string default connection ID from a JSON bridge', () => {
    const info = peerRoutePair([{ conn_id: 'fallback', stats: { bandwidth_estimate_version: 1, estimated_tx_bps: 5_000_000 } },
      { conn_id: 'default', stats: { bandwidth_estimate_version: 1, estimated_tx_bps: 1_000_000 } }])
    info.peer.default_conn_id = 'default'
    expect(estimatedBandwidth(info).upload).toBe('1.00 Mbit/s')
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

  it('formats protocol groups using active connection IDs', () => {
    const info = peerRoutePair([
      { conn_id: 'active-a', protocol: 'http3', ipv6: true },
      { conn_id: 'active-b', protocol: 'http3', ipv6: true },
      { conn_id: 'standby', protocol: 'wss', ipv6: false },
    ])
    info.peer.parallel_connections = {
      target: 4,
      active_conn_ids: ['active-a', 'active-b'],
      protocol: 'http3',
      fallback_reason: 'capacity limit',
    }

    expect(tunnelProtocols(info, conn => `${conn.protocol}${conn.ipv6 ? '6' : ''}`)).toBe('http36[×2],wss')
  })

  it('keeps legacy protocol labels without multipliers when the backend omits pool state', () => {
    const info = peerRoutePair([
      { conn_id: 'old-a', protocol: 'http3' },
      { conn_id: 'old-b', protocol: 'http3' },
      { conn_id: 'old-c', protocol: 'wss' },
    ])

    expect(tunnelProtocols(info, conn => conn.protocol)).toBe('http3,wss')
  })

  it('uses the default active connection without adding windows or mixing directions across members', () => {
    const activeA = { conn_id: 'active-a', stats: {
      bandwidth_estimate_version: 1, estimated_tx_bps: 2_000_000, estimated_rx_bps: 3_000_000,
    } }
    const activeB = { conn_id: 'active-b', stats: {
      bandwidth_estimate_version: 1, estimated_tx_bps: 3_000_000, estimated_rx_bps: 4_000_000,
    } }
    const backup = { conn_id: 'backup', stats: {
      bandwidth_estimate_version: 1, estimated_tx_bps: 90_000_000, estimated_rx_bps: 80_000_000,
    } }
    const info = peerRoutePair([activeA, activeB, backup])
    info.peer.default_conn_id = 'active-a'
    info.peer.parallel_connections = { target: 4, active_conn_ids: ['active-a', 'active-b'], protocol: 'http3' }

    expect(estimatedBandwidth(info)).toEqual({ upload: '2.00 Mbit/s', download: '3.00 Mbit/s' })

    info.peer.conns[1].stats = { bandwidth_estimate_version: 1, estimated_tx_bps: 3_000_000 }
    expect(estimatedBandwidth(info)).toEqual({ upload: '2.00 Mbit/s', download: '3.00 Mbit/s' })

    info.peer.default_conn_id = 'active-b'
    expect(estimatedBandwidth(info)).toEqual({ upload: '3.00 Mbit/s', download: '--' })

    info.peer.default_conn_id = 'backup'
    expect(estimatedBandwidth(info)).toEqual({ upload: '2.00 Mbit/s', download: '3.00 Mbit/s' })

    info.peer.conns[0].is_closed = true
    expect(estimatedBandwidth(info)).toEqual({ upload: '3.00 Mbit/s', download: '--' })
    info.peer.conns[1].is_closed = true
    expect(estimatedBandwidth(info)).toEqual({ upload: '--', download: '--' })
  })

  it('does not multiply bandwidth when four connections share a gigabit path', () => {
    const conns = Array.from({ length: 4 }, (_, i) => ({ conn_id: `active-${i}`, stats: {
      bandwidth_estimate_version: 1, estimated_tx_bps: 500_000_000, estimated_rx_bps: 45_000_000,
    } }))
    const info = peerRoutePair(conns)
    const single = estimatedBandwidth(info)
    info.peer.parallel_connections = { target: 4, active_conn_ids: conns.map(conn => conn.conn_id) }
    expect(estimatedBandwidth(info)).toEqual(single)
    expect(single).toEqual({ upload: '500 Mbit/s', download: '45.0 Mbit/s' })
  })

  it('ignores closed, unmeasured and standby connections when displaying pool latency and loss', () => {
    const info = peerRoutePair([
      { conn_id: 'closed', is_closed: true, stats: { latency_us: 1000 }, loss_rate: 1 },
      { conn_id: 'unmeasured', stats: { latency_us: 0 } },
      { conn_id: 'standby', stats: { latency_us: 500 }, loss_rate: 0.9 },
      { conn_id: 'active-a', stats: { latency_us: 4000 }, loss_rate: 0.01 },
      { conn_id: 'active-b', stats: { latency_us: 8000 }, loss_rate: 0.02 },
    ])
    info.peer.default_conn_id = 'unmeasured'
    expect(latencyMs(info)).toBe('1ms')
    info.peer.parallel_connections = { active_conn_ids: ['closed', 'active-a', 'active-b'] }
    expect(latencyMs(info)).toBe('4ms')
    expect(lossRate(info)).toBe('1%')
    info.peer.default_conn_id = 'active-b'
    expect(latencyMs(info)).toBe('8ms')
    expect(lossRate(info)).toBe('2%')
    info.peer.conns[3].is_closed = true
    info.peer.conns[4].is_closed = true
    expect(latencyMs(info)).toBe('')
    expect(lossRate(info)).toBe('')
    expect(latencyMs(peerRoutePair([{ stats: { latency_us: 0 } }, { stats: { latency_us: -10 } }]))).toBe('')
  })

  it('uses existing default-connection estimates when a pool has no active members', () => {
    const defaultConnId = '00000001-0002-0003-0004-000000000005'
    const info = peerRoutePairWithDefaultConn([
      { conn_id: defaultConnId, stats: { bandwidth_estimate_version: 1, estimated_tx_bps: 6_000_000, estimated_rx_bps: 7_000_000 } },
      { conn_id: 'backup', stats: { bandwidth_estimate_version: 1, estimated_tx_bps: 90_000_000, estimated_rx_bps: 80_000_000 } },
    ], defaultConnId)
    info.peer.parallel_connections = { target: 4, active_conn_ids: [], protocol: 'http3' }

    expect(estimatedBandwidth(info)).toEqual({ upload: '6.00 Mbit/s', download: '7.00 Mbit/s' })
  })

  it('keeps an omitted zero loss on the default connection', () => {
    const defaultConnId = '00000001-0002-0003-0004-000000000005'
    expect(lossRate(peerRoutePairWithDefaultConn([
      { conn_id: 'fallback', loss_rate: 0.25 },
      { conn_id: defaultConnId },
    ], defaultConnId))).toBe('0%')
  })

  it.each([
    [0, 'Unknown', 'Unknown'],
    [1, 'OpenInternet', 'Open Internet'],
    [2, 'NoPAT', 'No PAT'],
    [3, 'FullCone', 'Full Cone'],
    [4, 'Restricted', 'Restricted'],
    [5, 'PortRestricted', 'Port Restricted'],
    [6, 'Symmetric', 'Symmetric'],
    [7, 'SymUdpFirewall', 'Symmetric UDP Firewall'],
    [8, 'SymmetricEasyInc', 'Symmetric Easy Inc'],
    [9, 'SymmetricEasyDec', 'Symmetric Easy Dec'],
  ])('renders numeric and protobuf enum NAT type %s', (value, name, label) => {
    expect(udpNatTypeName({ udp_nat_type: value })).toBe(label)
    expect(udpNatTypeName({ udp_nat_type: name })).toBe(label)
    expect(udpNatTypeName({ udp_nat_type: String(value) })).toBe(label)
  })

  it('distinguishes missing STUN data from default or unrecognized NAT types', () => {
    expect(udpNatTypeName(undefined)).toBe('')
    expect(udpNatTypeName({})).toBe('Unknown')
    expect(udpNatTypeName({ udp_nat_type: 100 })).toBe('Unknown')
    expect(udpNatTypeName({ udp_nat_type: 'FutureNatType' })).toBe('Unknown')
  })
})
