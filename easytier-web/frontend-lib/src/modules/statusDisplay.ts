import { NatType, type PeerRoutePair, type StunInfo } from '../types/network'

const udpNatTypeStrMap: Record<NatType, string> = {
  [NatType.Unknown]: 'Unknown',
  [NatType.OpenInternet]: 'Open Internet',
  [NatType.NoPAT]: 'No PAT',
  [NatType.FullCone]: 'Full Cone',
  [NatType.Restricted]: 'Restricted',
  [NatType.PortRestricted]: 'Port Restricted',
  [NatType.Symmetric]: 'Symmetric',
  [NatType.SymUdpFirewall]: 'Symmetric UDP Firewall',
  [NatType.SymmetricEasyInc]: 'Symmetric Easy Inc',
  [NatType.SymmetricEasyDec]: 'Symmetric Easy Dec',
}

export function numericValue(value: unknown): number | undefined {
  if (typeof value === 'number')
    return Number.isFinite(value) ? value : undefined

  if (typeof value !== 'string' || value.trim() === '')
    return undefined

  const parsed = Number(value)
  return Number.isFinite(parsed) ? parsed : undefined
}

export function peerConns(info: PeerRoutePair) {
  return info.peer?.conns || []
}

interface ParallelConnectionsState {
  active_conn_ids?: unknown
}

function parallelConnectionsState(info: PeerRoutePair): ParallelConnectionsState | undefined {
  const peer = info.peer as (NonNullable<PeerRoutePair['peer']> & {
    parallel_connections?: ParallelConnectionsState
  }) | undefined
  const state = peer?.parallel_connections
  return state && typeof state === 'object' ? state : undefined
}

export function parallelActiveConnectionIds(info: PeerRoutePair): string[] {
  const ids = parallelConnectionsState(info)?.active_conn_ids
  if (!Array.isArray(ids))
    return []

  return [...new Set(ids.filter((id): id is string => typeof id === 'string' && id.length > 0))]
}

export function tunnelProtocols(info: PeerRoutePair, protocolForConnection: (conn: ReturnType<typeof peerConns>[number]) => string) {
  const activeIds = new Set(parallelActiveConnectionIds(info))
  const groups = new Map<string, Set<string>>()

  for (const conn of peerConns(info)) {
    const protocol = protocolForConnection(conn)
    if (!groups.has(protocol))
      groups.set(protocol, new Set())

    if (protocol && conn.conn_id && !conn.is_closed && activeIds.has(conn.conn_id))
      groups.get(protocol)!.add(conn.conn_id)
  }

  return [...groups].map(([protocol, ids]) => ids.size > 1 ? `${protocol}[×${ids.size}]` : protocol).join(',')
}

function defaultConnId(info: PeerRoutePair) {
  const defaultConn = info.peer?.default_conn_id
  if (!defaultConn)
    return undefined
  if (typeof defaultConn === 'string')
    return defaultConn === '00000000-0000-0000-0000-000000000000' ? undefined : defaultConn

  const part1 = defaultConn.part1 ?? 0
  const part2 = defaultConn.part2 ?? 0
  const part3 = defaultConn.part3 ?? 0
  const part4 = defaultConn.part4 ?? 0
  if (part1 === 0 && part2 === 0 && part3 === 0 && part4 === 0)
    return undefined

  const toHex = (value: number) => value.toString(16).padStart(8, '0')
  const part1Hex = toHex(part1)
  const part2Hex = toHex(part2)
  const part3Hex = toHex(part3)
  const part4Hex = toHex(part4)
  return `${part1Hex}-${part2Hex.slice(0, 4)}-${part2Hex.slice(4, 8)}-${part3Hex.slice(0, 4)}-${part3Hex.slice(4, 8)}${part4Hex}`
}

function defaultConnFirst(info: PeerRoutePair) {
  const conns = peerConns(info)
  const connId = defaultConnId(info)
  if (!connId)
    return conns

  const defaultConn = conns.find(conn => conn.conn_id === connId)
  return defaultConn ? [defaultConn, ...conns.filter(conn => conn !== defaultConn)] : conns
}

function telemetryConns(info: PeerRoutePair) {
  const activeIds = new Set(parallelActiveConnectionIds(info))
  return defaultConnFirst(info).filter(conn => !conn.is_closed
    && (!activeIds.size || !!conn.conn_id && activeIds.has(conn.conn_id)))
}

export function latencyMs(info: PeerRoutePair) {
  const connId = defaultConnId(info)
  let minLatencyUs: number | undefined

  for (const conn of telemetryConns(info)) {
    if (!conn.stats)
      continue

    const latencyUs = numericValue(conn.stats.latency_us)
    if (latencyUs === undefined || latencyUs <= 0)
      continue

    if (connId === conn.conn_id)
      return `${Math.ceil(latencyUs / 1000)}ms`

    minLatencyUs = Math.min(minLatencyUs ?? latencyUs, latencyUs)
  }

  if (minLatencyUs === undefined)
    return ''

  return `${Math.ceil(minLatencyUs / 1000)}ms`
}

export function lossRate(info: PeerRoutePair) {
  for (const conn of telemetryConns(info)) {
    const loss = numericValue(conn.loss_rate ?? 0)
    if (loss === undefined)
      continue

    return `${Math.round(loss * 100)}%`
  }

  return ''
}

function formatBitRate(bitsPerSecond: number): string {
  if (!Number.isFinite(bitsPerSecond) || bitsPerSecond < 0)
    return '--'

  const units = ['bit/s', 'kbit/s', 'Mbit/s', 'Gbit/s', 'Tbit/s']
  let value = bitsPerSecond
  let unit = 0
  while (value >= 1000 && unit < units.length - 1) {
    value /= 1000
    unit += 1
  }

  const precision = value >= 100 ? 0 : value >= 10 ? 1 : 2
  return `${value.toFixed(precision)} ${units[unit]}`
}

function connectionBandwidth(conn: ReturnType<typeof peerConns>[number], direction: 'tx' | 'rx') {
  if (conn.is_closed || numericValue(conn.stats?.bandwidth_estimate_version) !== 1)
    return undefined
  const value = numericValue(direction === 'tx' ? conn.stats?.estimated_tx_bps : conn.stats?.estimated_rx_bps)
  return value !== undefined && value > 0 ? value : undefined
}

export function estimatedBandwidth(info: PeerRoutePair): { upload: string, download: string } {
  // Congestion windows are per connection, not independent link capacities.
  // Adding them double-counts shared bottlenecks. Prefer the default active
  // connection and keep both directions on the same connection.
  const conn = telemetryConns(info).find(conn => connectionBandwidth(conn, 'tx') !== undefined
    || connectionBandwidth(conn, 'rx') !== undefined)
  return conn ? estimatedConnectionBandwidth(conn) : { upload: '--', download: '--' }
}

export function estimatedConnectionBandwidth(conn: ReturnType<typeof peerConns>[number]) {
  const upload = connectionBandwidth(conn, 'tx')
  const download = connectionBandwidth(conn, 'rx')
  return {
    upload: upload === undefined ? '--' : formatBitRate(upload),
    download: download === undefined ? '--' : formatBitRate(download),
  }
}

export function trafficBytes(value: unknown): string {
  let bytes = numericValue(value)
  if (bytes === undefined || bytes < 0) return '--'
  if (bytes < 1024) return `${bytes} B`
  const units = ['KiB', 'MiB', 'GiB', 'TiB', 'PiB', 'EiB']
  let unit = -1
  do { bytes /= 1024; unit++ } while (bytes >= 1024 && unit < units.length - 1)
  return `${bytes.toFixed(1)} ${units[unit]}`
}

export function totalTraffic(info: PeerRoutePair) {
  const sum = (direction: 'tx_bytes' | 'rx_bytes') => {
    const values = peerConns(info).map(conn => numericValue(conn.stats?.[direction])).filter((value): value is number => value !== undefined)
    return values.length ? values.reduce((total, value) => total + value, 0) : undefined
  }
  return { upload: trafficBytes(sum('tx_bytes')), download: trafficBytes(sum('rx_bytes')) }
}

export function udpNatTypeName(stunInfo: StunInfo | undefined) {
  if (!stunInfo)
    return ''

  const value = stunInfo.udp_nat_type ?? NatType.Unknown
  const natType = numericValue(value) ?? NatType[value as keyof typeof NatType]
  return udpNatTypeStrMap[natType as NatType] ?? udpNatTypeStrMap[NatType.Unknown]
}
