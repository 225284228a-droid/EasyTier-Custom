import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import {
  PersistentTopologyArchive,
  readGlobePreferences,
  saveGlobePreferences,
  type GlobePreferences,
} from '../src/modules/dashboardPersistence'

const DAY = 24 * 60 * 60 * 1_000
let cookies: Map<string, string>
let storage: Map<string, string>
let cookieWrites: string[]

const prefs = (): GlobePreferences => ({
  position: [0, 0, 2.8],
  rotating: true,
  selectedId: 'network:name:mesh:peer:1',
})

const device = (id = 'a', instances = ['mesh']) => ({
  machine_id: id,
  hostname: `host-${id}`,
  running_network_count: instances.length,
  running_network_instances: instances,
  public_ip: 'https://console.example/',
  location: { country: 'Proxy' },
  authorization: 'DEVICE_SECRET',
}) as any

const snapshot = (currentDevice = device(), instanceId = 'mesh') => ({
  device: currentDevice,
  instanceId,
  collectedAt: 2_000,
  detail: {
    running: true,
    network_name: 'mesh',
    network_secret: 'NETWORK_SECRET',
    config: { password: 'CONFIG_SECRET' },
    events: ['EVENT_SECRET'],
    my_node_info: {
      peer_id: 1,
      hostname: 'runtime-node',
      ips: { public_ipv4: { addr: 123 } },
      token: 'NODE_SECRET',
    },
    node_location: {
      public_ip: '1.1.1.1',
      country: 'Australia',
      city: 'Sydney',
      region: 'NSW',
      latitude: -33.86,
      longitude: 151.21,
      token: 'LOCATION_SECRET',
    },
    peers: [{
      peer_id: 2,
      conns: [{
        conn_id: 'udp-connection',
        peer_id: 2,
        my_peer_id: 1,
        is_closed: false,
        network_name: 'mesh',
        stats: { tx_bytes: '18446744073709551000', rx_bytes: 20 },
        tunnel: {
          tunnel_type: 'udp',
          local_addr: { url: 'udp://private-address/' },
          remote_addr: { url: 'udp://remote-address/' },
        },
      }],
    }],
    routes: [{
      peer_id: 2,
      inst_id: 'remote-instance',
      hostname: 'remote-node',
      feature_flag: { secret: 'ROUTE_SECRET' },
      proxy_cidrs: ['192.168.0.0/16'],
    }],
    peer_route_pairs: [{ secret: 'PAIR_SECRET' }],
  },
}) as any

function stored() {
  return JSON.parse([...storage.values()][0])
}

beforeEach(() => {
  cookies = new Map()
  storage = new Map()
  cookieWrites = []
  vi.stubGlobal('document', {
    get cookie() { return [...cookies].map(([name, value]) => `${name}=${value}`).join('; ') },
    set cookie(value: string) {
      cookieWrites.push(value)
      const pair = value.split(';')[0]
      const separator = pair.indexOf('=')
      cookies.set(pair.slice(0, separator), pair.slice(separator + 1))
    },
  })
  vi.stubGlobal('location', { protocol: 'http:' })
  vi.stubGlobal('localStorage', {
    getItem: vi.fn((key: string) => storage.get(key) ?? null),
    setItem: vi.fn((key: string, value: string) => storage.set(key, value)),
    removeItem: vi.fn((key: string) => storage.delete(key)),
  })
})

afterEach(() => {
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})

describe('globe preferences', () => {
  it('round-trips preferences in a scope-hashed cookie with a 180-day lifetime', () => {
    const scope = 'https://console.example/user-a'
    saveGlobePreferences(scope, prefs())
    expect(readGlobePreferences(scope)).toEqual(prefs())
    expect(readGlobePreferences('https://console.example/user-b')).toBeUndefined()
    expect(cookieWrites[0]).toContain('Max-Age=15552000')
    expect(cookieWrites[0]).toContain('Path=/')
    expect(cookieWrites[0]).toContain('SameSite=Lax')
    expect(cookieWrites[0]).not.toContain('; Secure')
    expect([...cookies.keys()][0]).not.toContain(scope)
    expect(storage.size).toBe(0)
  })

  it('adds Secure only for HTTPS', () => {
    vi.stubGlobal('location', { protocol: 'https:' })
    saveGlobePreferences('https-scope', prefs())
    expect(cookieWrites[0]).toContain('; Secure')
  })

  it.each([
    { position: [0, 0, 1.24] },
    { position: [0, 0, 6.51] },
    { position: [0, 0, Number.NaN] },
    { position: [0, 0, Number.POSITIVE_INFINITY] },
    { position: [0, 2] },
    { position: ['0', 0, 2] },
    { selectedId: 'x'.repeat(513) },
    { rotating: 'true' },
  ])('ignores invalid preference fields %s', (invalid) => {
    saveGlobePreferences('scope', { ...prefs(), ...invalid } as any)
    expect(cookieWrites).toHaveLength(0)
    saveGlobePreferences('scope', prefs())
    const key = [...cookies.keys()][0]
    cookies.set(key, encodeURIComponent(JSON.stringify({ ...prefs(), ...invalid })))
    expect(readGlobePreferences('scope')).toBeUndefined()
  })

  it('accepts radius boundaries and strips unrecognized fields', () => {
    for (const radius of [1.25, 6.5, 1.25 - Number.EPSILON, 6.5 + Number.EPSILON * 4]) {
      saveGlobePreferences('scope', { ...prefs(), position: [radius, 0, 0], secret: 'EXCLUDED' } as any)
      expect(readGlobePreferences('scope')?.position).toEqual([radius, 0, 0])
      expect(cookieWrites.at(-1)).not.toContain('EXCLUDED')
    }
  })

  it('ignores malformed cookie values and cookie-access errors', () => {
    saveGlobePreferences('scope', prefs())
    const key = [...cookies.keys()][0]
    for (const value of ['%broken', 'not-json', encodeURIComponent('[]')]) {
      cookies.set(key, value)
      expect(readGlobePreferences('scope')).toBeUndefined()
    }
    vi.stubGlobal('document', {
      get cookie() { throw new Error('disabled') },
      set cookie(_value: string) { throw new Error('disabled') },
    })
    expect(readGlobePreferences('scope')).toBeUndefined()
    expect(() => saveGlobePreferences('scope', prefs())).not.toThrow()
  })
})

describe('persistent topology archive', () => {
  it('round-trips only whitelisted runtime data and substitutes the currently authorized device', () => {
    const archive = new PersistentTopologyArchive('server-a')
    const original = device()
    archive.update([original], [snapshot(original)], 1_000)
    const raw = [...storage.values()][0]
    expect(raw).not.toMatch(/SECRET|stats|tx_bytes|rx_bytes|events|config|authorization|collectedAt|local_addr|remote_addr|proxy_cidrs|feature_flag|peer_route_pairs|"ips"/)
    expect(cookies.size).toBe(0)
    const current = { ...device(), hostname: 'current-name' }
    const result = archive.read([current], 2_000)
    expect(result).toHaveLength(1)
    expect(result[0].device).toBe(current)
    expect(result[0].stale).toBe(true)
    expect(result[0]).not.toHaveProperty('collectedAt')
    expect(result[0].detail).not.toHaveProperty('events')
    expect(result[0].detail.my_node_info.peer_id).toBe(1)
    expect(result[0].detail.node_location?.city).toBe('Sydney')
    expect(result[0].detail.peers[0].conns[0]).toMatchObject({
      conn_id: 'udp-connection',
      peer_id: 2,
      my_peer_id: 1,
      tunnel: { tunnel_type: 'udp' },
    })
    expect(result[0].detail.peers[0].conns[0]).not.toHaveProperty('stats')
    expect(result[0].detail.routes[0]).toEqual({
      peer_id: 2,
      inst_id: 'remote-instance',
      hostname: 'remote-node',
    })
  })

  it('isolates server scopes and clears only its own archive', () => {
    const a = new PersistentTopologyArchive('server-a')
    const b = new PersistentTopologyArchive('server-b')
    a.update([device()], [snapshot()], 0)
    expect(b.read([device()], 1_000)).toEqual([])
    b.update([device()], [snapshot()], 0)
    a.clear()
    expect(a.read([device()], 1_000)).toEqual([])
    expect(b.read([device()], 1_000)).toHaveLength(1)
  })

  it('requires current machine authorization and an explicitly matching running instance', () => {
    const archive = new PersistentTopologyArchive('scope')
    archive.update([device()], [snapshot()], 0)
    expect(archive.read([], 1_000)).toEqual([])
    expect(archive.read([device('other')], 1_000)).toEqual([])
    expect(archive.read([device('a', ['other-instance'])], 1_000)).toEqual([])
    expect(archive.read([device('a', [])], 1_000)).toEqual([])
    expect(archive.read([{ ...device(), running_network_instances: undefined }], 1_000)).toEqual([])
  })

  it('deletes confirmed stopped instances while retaining machines missing from a failed collection', () => {
    const archive = new PersistentTopologyArchive('scope')
    const a = device('a', ['alpha', 'beta'])
    const b = device('b')
    archive.update([a, b], [snapshot(a, 'alpha'), snapshot(a, 'beta'), snapshot(b)], 0)
    archive.update([device('a', ['alpha'])], [], 1_000)
    expect(archive.read([a, b], 2_000).map(value => `${value.device.machine_id}:${value.instanceId}`))
      .toEqual(['a:alpha', 'b:mesh'])
    archive.update([device('a', [])], [], 3_000)
    expect(archive.read([a, b], 4_000).map(value => value.device.machine_id)).toEqual(['b'])
  })

  it('renews only successful snapshots, not failed machines, stale snapshots, or reads', () => {
    const archive = new PersistentTopologyArchive('scope')
    const a = device('a')
    const b = device('b')
    archive.update([a, b], [snapshot(a), snapshot(b)], 0)
    archive.update([a, b], [snapshot(a), { ...snapshot(b), stale: true }], 7 * DAY - 1_000)
    const before = [...storage.values()][0]
    const writes = vi.mocked(localStorage.setItem).mock.calls.length
    expect(archive.read([a, b], 7 * DAY).map(value => value.device.machine_id)).toEqual(['a'])
    expect([...storage.values()][0]).toBe(before)
    expect(vi.mocked(localStorage.setItem).mock.calls).toHaveLength(writes)
    expect(archive.read([a, b], 14 * DAY - 1_000)).toEqual([])
  })

  it('does not save snapshots that are not authorized or are explicitly stopped', () => {
    const archive = new PersistentTopologyArchive('scope')
    archive.update([device('a', [])], [snapshot(), snapshot(device('unauthorized'))], 0)
    expect(stored().entries).toEqual([])
  })

  it('ignores malformed schemas, future timestamps, and expired records', () => {
    const archive = new PersistentTopologyArchive('scope')
    archive.update([device()], [snapshot()], 1_000)
    const key = [...storage.keys()][0]
    const valid = stored()
    for (const raw of [
      '{broken',
      JSON.stringify({ version: 2, entries: valid.entries }),
      JSON.stringify({ version: 1, entries: {} }),
      JSON.stringify({ version: 1, entries: [{ ...valid.entries[0], savedAt: 3_000 }] }),
      JSON.stringify({ version: 1, entries: [{ ...valid.entries[0], savedAt: -1 }] }),
      JSON.stringify({ version: 1, entries: [{ ...valid.entries[0], detail: { my_node_info: { peer_id: '1' } } }] }),
    ]) {
      storage.set(key, raw)
      expect(archive.read([device()], 2_000)).toEqual([])
    }
    storage.set(key, JSON.stringify(valid))
    expect(archive.read([device()], 7 * DAY + 1_000)).toEqual([])
  })

  it('enforces serialized size and snapshot-count limits on both reads and writes', () => {
    const archive = new PersistentTopologyArchive('scope')
    const instances = Array.from({ length: 2_001 }, (_, index) => `instance-${index}`)
    const current = device('a', instances)
    archive.update([current], instances.map(instance => snapshot(current, instance)), 0)
    const raw = [...storage.values()][0]
    expect(new TextEncoder().encode(raw).byteLength).toBeLessThanOrEqual(1024 * 1024)
    expect(stored().entries.length).toBeLessThanOrEqual(2_000)
    const key = [...storage.keys()][0]
    const saved = stored().entries[0]
    storage.set(key, JSON.stringify({ version: 1, entries: Array(2_001).fill(saved) }))
    expect(archive.read([current], 1_000)).toEqual([])
    storage.set(key, 'x'.repeat(1024 * 1024 + 1))
    expect(archive.read([current], 1_000)).toEqual([])
  })

  it('handles unavailable storage and invalid clocks without throwing', () => {
    const archive = new PersistentTopologyArchive('scope')
    const denied = vi.fn(() => { throw new Error('denied') })
    vi.stubGlobal('localStorage', { getItem: denied, setItem: denied, removeItem: denied })
    expect(archive.read([device()], 0)).toEqual([])
    expect(() => archive.update([device()], [snapshot()], 0)).not.toThrow()
    expect(() => archive.clear()).not.toThrow()
    expect(() => archive.update([device()], [snapshot()], Number.NaN)).not.toThrow()
    expect(archive.read([device()], Number.POSITIVE_INFINITY)).toEqual([])
  })
})
