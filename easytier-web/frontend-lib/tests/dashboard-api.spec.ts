import { beforeEach, describe, expect, it, vi } from 'vitest'
import ApiClient from '../../frontend/src/modules/api'
import { normalizeNetworkConfig } from '../src/types/network'
import { LOCAL_CONFIG_APPLY_CAPABILITY, LOCAL_CONFIG_REVISION_CAPABILITY } from '../src/modules/capabilities'

const client = vi.hoisted(() => ({
  get: vi.fn(),
  post: vi.fn(),
  put: vi.fn(),
  delete: vi.fn(),
  interceptors: {
    request: { use: vi.fn() },
    response: { use: vi.fn() },
  },
}))

vi.mock('axios', () => ({
  default: { create: vi.fn(() => client) },
  AxiosError: class extends Error {},
}))

describe('dashboard API request limits', () => {
  beforeEach(() => {
    client.get.mockReset()
    client.post.mockReset()
    client.put.mockReset()
    client.delete.mockReset()
    client.interceptors.response.use.mockClear()
  })

  it('gates remote apply-only separately from revision-aware editing without changing the request shape', async () => {
    const api = new ApiClient('http://localhost')
    const id = '00000000-0000-0000-0000-000000000001'
    const request = { inst_id: id, expected_revision: 'observed-revision', config: {}, field_mask: [], apply_mode: 0 }
    const remote = api.get_remote_client('machine')
    client.get.mockResolvedValue({ running_inst_ids: [id], disabled_inst_ids: [], runtime_capabilities: [LOCAL_CONFIG_REVISION_CAPABILITY] })
    await remote.list_network_instance_ids()
    await expect(remote.patch_local_config!(request)).rejects.toThrow('Upgrade the device')
    expect(client.post).not.toHaveBeenCalled()
    client.get.mockResolvedValue({ running_inst_ids: [id], disabled_inst_ids: [],
      runtime_capabilities: [LOCAL_CONFIG_REVISION_CAPABILITY, LOCAL_CONFIG_APPLY_CAPABILITY] })
    await remote.list_network_instance_ids()
    await remote.patch_local_config!(request)
    expect(client.post).toHaveBeenCalledWith('/machines/machine/local-configs/patch', { ...request,
      inst_id: { part1: 0, part2: 0, part3: 0, part4: 1 } })
  })

  it('forwards timeout and cancellation to machine listing', async () => {
    const api = new ApiClient('http://localhost')
    const options = { timeout: 8_000, signal: new AbortController().signal }
    client.get.mockResolvedValue({ machines: [{ hostname: 'node-a' }] })
    expect(await api.list_machines(options)).toEqual([{ hostname: 'node-a' }])
    expect(client.get).toHaveBeenCalledWith('/machines', options)
  })

  it('forwards timeout and cancellation to topology collection', async () => {
    const api = new ApiClient('http://localhost')
    const options = { timeout: 8_000, signal: new AbortController().signal }
    client.post.mockResolvedValue({ info: { map: { mesh: { running: true } } } })
    expect(await api.collect_machine_network_info('node-a', options)).toEqual({ mesh: { running: true } })
    expect(client.post).toHaveBeenCalledWith('/machines/node-a/networks/info', {}, options)
  })

  it('keeps cached config liveness and capabilities when joining device names', async () => {
    const api = new ApiClient('http://localhost')
    const id = '00000000-0000-0000-0000-000000000001'
    const snapshot = { machine_id: id, entries: [], online: false, stale: true, capabilities: ['management:persisted-config-revision-v1'] }
    client.get.mockImplementation(path => Promise.resolve(path === '/local-configs'
      ? { machines: [snapshot] }
      : { machines: [{ info: { machine_id: id, hostname: 'node-name' }, alias: 'Node alias', online: true }] }))
    expect(await api.list_local_configs()).toEqual([{ ...snapshot, hostname: 'Node alias' }])
    client.get.mockImplementation(path => path === '/local-configs'
      ? Promise.resolve({ machines: [snapshot] }) : Promise.reject(new Error('metadata unavailable')))
    expect(await api.list_local_configs()).toEqual([{ ...snapshot, hostname: undefined }])
  })

  it('filters extension fields per target while retaining standard WS/WSS transports', async () => {
    const api = new ApiClient('http://localhost')
    const id = '00000000-0000-0000-0000-000000000001'
    client.get.mockImplementation(async path => path === '/machines'
      ? { machines: [{ info: { machine_id: id, runtime_capabilities: ['config:sni'] } }] } : {})
    await api.set_member_config('mesh', id, {
      sni: 'example.com', enable_bbr: false, p2p_prefer_protocol: 'tcp',
      listener_urls: ['ws://0.0.0.0:1', 'wss://0.0.0.0:2', 'http3://0.0.0.0:3'],
    })
    expect(client.put.mock.calls[0][1].config).toEqual({ sni: 'example.com', listener_urls: ['ws://0.0.0.0:1', 'wss://0.0.0.0:2'] })
    expect(client.get).toHaveBeenCalledWith(`/networks/mesh/members/${id}/config`)
  })

  it.each([
    { enable_bbr: false },
    { sni: '' },
    { p2p_prefer_protocol: 'udp' },
    { peer_urls: ['http3://saved.example:443'] },
  ])('refuses central full replacements that would erase an unadvertised raw override: %j', async existing => {
    const api = new ApiClient('http://localhost')
    const id = '00000000-0000-0000-0000-000000000001'
    client.get.mockImplementation(async path => path === '/machines'
      ? { machines: [{ info: { machine_id: id, runtime_capabilities: [] } }] } : existing)
    await expect(api.set_member_config('mesh', id, { hostname: 'edited' })).rejects.toThrow('saved custom settings')
    expect(client.get).toHaveBeenCalledWith(`/networks/mesh/members/${id}/config`)
    expect(client.put).not.toHaveBeenCalled()
  })

  it('keeps explicitly supported false/empty overrides in a central replacement', async () => {
    const api = new ApiClient('http://localhost')
    const id = '00000000-0000-0000-0000-000000000001'
    const existing = { enable_bbr: false, sni: '' }
    client.get.mockImplementation(async path => path === '/machines'
      ? { machines: [{ info: { machine_id: id, runtime_capabilities: ['config:enable_bbr', 'config:sni'] } }] } : existing)
    await api.set_member_config('mesh', id, { ...existing, hostname: 'edited' })
    expect(client.put.mock.calls[0][1].config).toEqual({ ...existing, hostname: 'edited' })
  })

  it('does not save central overrides if the latest raw configuration cannot be read', async () => {
    const api = new ApiClient('http://localhost')
    const id = '00000000-0000-0000-0000-000000000001'
    client.get.mockImplementation(async path => {
      if (path === '/machines') return { machines: [{ info: { machine_id: id, runtime_capabilities: [] } }] }
      throw new Error('configuration unavailable')
    })
    await expect(api.set_member_config('mesh', id, { hostname: 'edited' })).rejects.toThrow('configuration unavailable')
    expect(client.put).not.toHaveBeenCalled()
  })

  it('rejects a central save when the account changes during the raw configuration read', async () => {
    const api = new ApiClient('http://localhost')
    api.set_authenticated_account('alice')
    const id = '00000000-0000-0000-0000-000000000001'
    client.get.mockImplementation(async path => {
      if (path === '/machines') return { machines: [{ info: { machine_id: id, runtime_capabilities: [] } }] }
      api.set_authenticated_account('bob')
      return {}
    })
    await expect(api.set_member_config('mesh', id, { hostname: 'edited' })).rejects.toThrow('Connection scope changed')
    expect(client.put).not.toHaveBeenCalled()
  })

  it('encodes patch UUIDs for protobuf JSON and carries lifecycle revisions', async () => {
    const api = new ApiClient('http://localhost')
    const id = '00000000-0000-0000-0000-000000000001'
    const local = api.get_local_config_client()
    await local.patch(id, { inst_id: id, expected_revision: 'own-revision', field_mask: ['hostname'], config: { hostname: 'edited' }, apply_mode: 1 })
    expect(client.post.mock.calls[0][1]).toMatchObject({ inst_id: { part1: 0, part2: 0, part3: 0, part4: 1 }, expected_revision: 'own-revision', field_mask: ['hostname'], apply_mode: 1 })
    await local.setEnabled!(id, id, 'new-revision', true)
    expect(client.post.mock.calls[1][1]).toEqual({ expected_revision: 'new-revision', enabled: true })
    await local.remove!(id, id, 'delete-revision')
    expect(client.delete.mock.calls[0][1]).toEqual({ data: { expected_revision: 'delete-revision' } })
  })

  it('separates account scopes and clears the scope on an authentication failure', async () => {
    const api = new ApiClient('http://localhost/')
    expect(api.persistenceScope).toBe('')
    api.set_authenticated_account('alice')
    const first = api.persistenceScope
    api.set_authenticated_account('bob')
    expect(api.persistenceScope).not.toBe(first)
    const reject = client.interceptors.response.use.mock.calls.at(-1)![1]
    await expect(reject({ response: { status: 401, data: 'expired' } })).rejects.toBeDefined()
    expect(api.persistenceScope).toBe('')
  })

  it('blocks a legacy hostname replacement that would erase saved unadvertised custom settings', async () => {
    const api = new ApiClient('http://localhost')
    const id = '00000000-0000-0000-0000-000000000001'
    const stored = { instance_id: id, hostname: 'old', p2p_prefer_protocol: 'udp', peer_urls: ['http3://saved.example:443'] }
    client.get.mockImplementation(async path => path.endsWith(`/config/${id}`) ? stored
      : { running_inst_ids: [], disabled_inst_ids: [id], runtime_capabilities: [] })
    const remote = api.get_remote_client('machine')
    const config = await remote.get_network_config(id)
    await expect(remote.save_config({ ...config, hostname: 'edited' })).rejects.toThrow('saved custom settings')
    await expect(remote.run_network({ ...config, hostname: 'edited' }, true)).rejects.toThrow('saved custom settings')
    expect(client.put).not.toHaveBeenCalled()
    expect(client.post).not.toHaveBeenCalled()
  })

  it('preflights existing legacy configurations and still saves an ordinary official configuration', async () => {
    const api = new ApiClient('http://localhost')
    const id = '00000000-0000-0000-0000-000000000001'
    client.get.mockImplementation(async path => path.endsWith(`/config/${id}`)
      ? { instance_id: id, hostname: 'official', peer_urls: ['wss://saved.example:443'] }
      : { running_inst_ids: [], disabled_inst_ids: [id], runtime_capabilities: [] })
    await api.get_remote_client('machine').save_config(normalizeNetworkConfig({ instance_id: id, hostname: 'edited' }))
    expect(client.get).toHaveBeenCalledWith(`/machines/machine/networks/config/${id}`)
    expect(client.put.mock.calls[0][1].config.hostname).toBe('edited')
    expect(client.put.mock.calls[0][1].config).not.toHaveProperty('p2p_prefer_protocol')
  })
})
