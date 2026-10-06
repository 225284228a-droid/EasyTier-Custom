import { Md5 } from 'ts-md5'
import type { Utils } from 'easytier-frontend-lib'
import type { NetworkSnapshot } from './networkTopology'

export interface GlobePreferences {
  position: [number, number, number]
  rotating: boolean
  selectedId: string
}

const COOKIE_MAX_AGE = 180 * 24 * 60 * 60
const ARCHIVE_MAX_AGE = 7 * 24 * 60 * 60 * 1_000
const ARCHIVE_MAX_BYTES = 1024 * 1024
const ARCHIVE_MAX_SNAPSHOTS = 2_000

function record(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined
}

function text(value: unknown, limit = 512): string | undefined {
  return typeof value === 'string' && value.length <= limit ? value : undefined
}

function identifier(value: unknown, limit = 512): string | undefined {
  return text(value, limit)?.trim() || undefined
}

function peerId(value: unknown, allowZero = false): value is number {
  return typeof value === 'number' && Number.isInteger(value)
    && value >= (allowZero ? 0 : 1) && value <= 0xffffffff
}

function wallClock(value: number): boolean {
  return Number.isSafeInteger(value) && value >= 0
}

function preferences(value: unknown): GlobePreferences | undefined {
  const input = record(value)
  const position = input?.position
  if (!Array.isArray(position) || position.length !== 3
    || !position.every(coordinate => typeof coordinate === 'number' && Number.isFinite(coordinate))
    || typeof input?.rotating !== 'boolean' || typeof input.selectedId !== 'string'
    || input.selectedId.length > 512)
    return undefined
  const radius = Math.hypot(position[0], position[1], position[2])
  if (radius < 1.25 - 1e-9 || radius > 6.5 + 1e-9)
    return undefined
  return {
    position: [position[0], position[1], position[2]],
    rotating: input.rotating,
    selectedId: input.selectedId,
  }
}

function cookieName(scope: string): string {
  return `easytier-globe-v1-${Md5.hashStr(scope)}`
}

export function readGlobePreferences(scope: string): GlobePreferences | undefined {
  try {
    if (typeof document === 'undefined')
      return undefined
    const prefix = `${cookieName(scope)}=`
    const value = document.cookie.split(';').map(part => part.trim())
      .find(part => part.startsWith(prefix))?.slice(prefix.length)
    if (!value || value.length > 4096)
      return undefined
    return preferences(JSON.parse(decodeURIComponent(value)))
  } catch {
    return undefined
  }
}

export function saveGlobePreferences(scope: string, value: GlobePreferences): void {
  try {
    const valid = preferences(value)
    if (!valid || typeof document === 'undefined')
      return
    const secure = typeof location !== 'undefined' && location.protocol === 'https:' ? '; Secure' : ''
    document.cookie = `${cookieName(scope)}=${encodeURIComponent(JSON.stringify(valid))}; Max-Age=${COOKIE_MAX_AGE}; Path=/; SameSite=Lax${secure}`
  } catch {
    // Browsers may deny cookies in private or third-party contexts.
  }
}

interface ArchivedConnection {
  conn_id: string
  peer_id: number
  my_peer_id: number
  is_closed: boolean
  network_name?: string
  tunnel?: { tunnel_type: string }
}

interface ArchivedDetail {
  network_name?: string
  my_node_info: { peer_id: number, hostname: string }
  node_location?: {
    public_ip: string
    country: string
    city: string
    region: string
    latitude?: number
    longitude?: number
  }
  peers: { peer_id: number, conns: ArchivedConnection[] }[]
  routes: { peer_id: number, inst_id: string, hostname: string }[]
}

interface ArchivedSnapshot {
  savedAt: number
  device: { machine_id: string, hostname: string }
  instanceId: string
  detail: ArchivedDetail
}

function nodeLocation(value: unknown): ArchivedDetail['node_location'] {
  const input = record(value)
  if (!input)
    return undefined
  const result: NonNullable<ArchivedDetail['node_location']> = {
    public_ip: text(input.public_ip, 128) ?? '',
    country: text(input.country, 256) ?? '',
    city: text(input.city, 256) ?? '',
    region: text(input.region, 256) ?? '',
  }
  if (typeof input.latitude === 'number' && Number.isFinite(input.latitude)
    && input.latitude >= -90 && input.latitude <= 90
    && typeof input.longitude === 'number' && Number.isFinite(input.longitude)
    && input.longitude >= -180 && input.longitude <= 180) {
    result.latitude = input.latitude
    result.longitude = input.longitude
  }
  return result
}

function connection(value: unknown, remotePeerId: number): ArchivedConnection | undefined {
  const input = record(value)
  const connId = identifier(input?.conn_id, 128)
  if (!input || !connId || (input.is_closed !== undefined && typeof input.is_closed !== 'boolean'))
    return undefined
  const tunnel = record(input.tunnel)
  const tunnelType = text(tunnel?.tunnel_type, 64)
  return {
    conn_id: connId,
    peer_id: peerId(input.peer_id) ? input.peer_id : remotePeerId,
    my_peer_id: peerId(input.my_peer_id, true) ? input.my_peer_id : 0,
    is_closed: input.is_closed === true,
    network_name: text(input.network_name),
    tunnel: tunnelType ? { tunnel_type: tunnelType } : undefined,
  }
}

function detail(value: unknown): ArchivedDetail | undefined {
  const input = record(value)
  const info = record(input?.my_node_info)
  if (!input || !info || !peerId(info.peer_id))
    return undefined
  const peers: ArchivedDetail['peers'] = []
  for (const value of (Array.isArray(input.peers) ? input.peers : []).slice(0, 256)) {
    const peer = record(value)
    if (!peer || !peerId(peer.peer_id))
      continue
    const conns = (Array.isArray(peer.conns) ? peer.conns : []).slice(0, 64)
      .map(value => connection(value, peer.peer_id as number))
      .filter((value): value is ArchivedConnection => !!value)
    peers.push({ peer_id: peer.peer_id, conns })
  }
  const routes: ArchivedDetail['routes'] = []
  for (const value of (Array.isArray(input.routes) ? input.routes : []).slice(0, 512)) {
    const route = record(value)
    if (route && peerId(route.peer_id)) {
      routes.push({
        peer_id: route.peer_id,
        inst_id: text(route.inst_id) ?? '',
        hostname: text(route.hostname, 256) ?? '',
      })
    }
  }
  return {
    network_name: text(input.network_name),
    my_node_info: { peer_id: info.peer_id, hostname: text(info.hostname, 256) ?? '' },
    node_location: nodeLocation(input.node_location),
    peers,
    routes,
  }
}

function entry(value: unknown, now: number): ArchivedSnapshot | undefined {
  const input = record(value)
  const device = record(input?.device)
  const machineId = identifier(device?.machine_id, 128)
  const instanceId = identifier(input?.instanceId)
  const runtime = detail(input?.detail)
  const savedAt = input?.savedAt
  if (!input || !device || !machineId || !instanceId || !runtime
    || typeof savedAt !== 'number' || !wallClock(savedAt) || savedAt > now
    || now - savedAt >= ARCHIVE_MAX_AGE)
    return undefined
  return {
    savedAt,
    device: { machine_id: machineId, hostname: text(device.hostname, 256) ?? '' },
    instanceId,
    detail: runtime,
  }
}

function entryKey(value: ArchivedSnapshot): string {
  return JSON.stringify([value.device.machine_id, value.instanceId])
}

function bytes(value: string): number {
  return new TextEncoder().encode(value).byteLength
}

/** A local-only, bounded visualization archive. It never contains authentication or counters. */
export class PersistentTopologyArchive {
  private readonly key: string

  constructor(scope: string) {
    this.key = `easytier-topology-v1-${Md5.hashStr(scope)}`
  }

  private load(now: number): ArchivedSnapshot[] {
    try {
      if (!wallClock(now) || typeof localStorage === 'undefined')
        return []
      const raw = localStorage.getItem(this.key)
      if (!raw || raw.length > ARCHIVE_MAX_BYTES || bytes(raw) > ARCHIVE_MAX_BYTES)
        return []
      const input = record(JSON.parse(raw))
      if (input?.version !== 1 || !Array.isArray(input.entries)
        || input.entries.length > ARCHIVE_MAX_SNAPSHOTS)
        return []
      const found = new Map<string, ArchivedSnapshot>()
      for (const value of input.entries) {
        const valid = entry(value, now)
        if (!valid)
          continue
        const key = entryKey(valid)
        if (!found.has(key) || found.get(key)!.savedAt < valid.savedAt)
          found.set(key, valid)
      }
      return [...found.values()]
    } catch {
      return []
    }
  }

  update(devices: Utils.DeviceInfo[], snapshots: NetworkSnapshot[], wallTime = Date.now()): void {
    try {
      if (!wallClock(wallTime) || typeof localStorage === 'undefined')
        return
      const currentDevices = new Map(devices.map(device => [device.machine_id, device]))
      const saved = new Map(this.load(wallTime).map(value => [entryKey(value), value]))
      for (const [key, value] of saved) {
        const device = currentDevices.get(value.device.machine_id)
        if (device && (device.running_network_count === 0
          || (Array.isArray(device.running_network_instances)
            && !device.running_network_instances.includes(value.instanceId))))
          saved.delete(key)
      }
      for (const snapshot of snapshots) {
        const device = currentDevices.get(snapshot.device.machine_id)
        if (!device || snapshot.stale || !snapshot.detail.running || device.running_network_count === 0
          || (Array.isArray(device.running_network_instances)
            && !device.running_network_instances.includes(snapshot.instanceId)))
          continue
        const valid = entry({ savedAt: wallTime, device, instanceId: snapshot.instanceId, detail: snapshot.detail }, wallTime)
        if (valid)
          saved.set(entryKey(valid), valid)
      }
      const kept: ArchivedSnapshot[] = []
      let size = bytes(JSON.stringify({ version: 1, entries: [] }))
      for (const value of [...saved.values()].sort((a, b) => b.savedAt - a.savedAt)) {
        const added = bytes(JSON.stringify(value)) + (kept.length ? 1 : 0)
        if (size + added > ARCHIVE_MAX_BYTES)
          continue
        kept.push(value)
        size += added
        if (kept.length >= ARCHIVE_MAX_SNAPSHOTS)
          break
      }
      localStorage.setItem(this.key, JSON.stringify({ version: 1, entries: kept }))
    } catch {
      // Storage quotas and browser privacy settings must not affect the dashboard.
    }
  }

  read(devices: Utils.DeviceInfo[], wallTime = Date.now()): NetworkSnapshot[] {
    const authorized = new Map(devices.map(device => [device.machine_id, device]))
    return this.load(wallTime).flatMap(value => {
      const device = authorized.get(value.device.machine_id)
      if (!device || !(device.running_network_count > 0)
        || !Array.isArray(device.running_network_instances)
        || !device.running_network_instances.includes(value.instanceId))
        return []
      return [{
        device,
        instanceId: value.instanceId,
        detail: { ...value.detail, running: true } as unknown as NetworkSnapshot['detail'],
        stale: true,
      }]
    })
  }

  clear(): void {
    try {
      if (typeof localStorage !== 'undefined')
        localStorage.removeItem(this.key)
    } catch {
      // Clearing a cache is best-effort when storage is unavailable.
    }
  }
}
