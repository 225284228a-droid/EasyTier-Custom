import { beforeEach, describe, expect, it, vi } from 'vitest'
import { NetworkTypes, LocalConfigs, Capabilities } from 'easytier-frontend-lib'
import { GUIRemoteClient } from './api'

const invoke = vi.hoisted(() => vi.fn())
vi.mock('@tauri-apps/api/core', () => ({ invoke }))
const id = '00000000-0000-0000-0000-000000000001'
beforeEach(() => invoke.mockReset())

describe('GUI management bridge capabilities and revisions', () => {
  it('gates apply-only independently and forwards an empty config/mask with the observed revision', async () => {
    const revision = Capabilities.LOCAL_CONFIG_REVISION_CAPABILITY
    invoke.mockImplementation(async command => command === 'list_network_instance_ids'
      ? { running_inst_ids: [id], disabled_inst_ids: [], runtime_capabilities: [revision] }
      : { status: 'Success' })
    const request = { inst_id: id, expected_revision: 'observed-revision', config: {}, field_mask: [], apply_mode: 0 }
    await expect(new GUIRemoteClient().patch_local_config(request)).rejects.toThrow('Upgrade the device')
    expect(invoke.mock.calls.some(([command]) => command === 'patch_local_config')).toBe(false)
    invoke.mockImplementation(async command => command === 'list_network_instance_ids'
      ? { running_inst_ids: [id], disabled_inst_ids: [], runtime_capabilities: [revision, Capabilities.LOCAL_CONFIG_APPLY_CAPABILITY] }
      : command === 'observe_local_configs' ? { catalog_epoch: 'boot-a', catalog_generation: '1', entries: [] } : { status: 'Success' })
    const api = new GUIRemoteClient()
    expect((await api.observe_local_configs()).capabilities).toContain(Capabilities.LOCAL_CONFIG_APPLY_CAPABILITY)
    await api.patch_local_config(LocalConfigs.buildLocalConfigApplyRequest({ inst_id: id, revision: 'observed-revision' },
      [Capabilities.LOCAL_CONFIG_APPLY_CAPABILITY]))
    expect(invoke).toHaveBeenCalledWith('patch_local_config', { request: { ...request, inst_id: { part1: 0, part2: 0, part3: 0, part4: 1 } } })
  })

  it('removes unsupported extensions from every legacy config operation, keeping WS/WSS', async () => {
    invoke.mockImplementation(async (command: string) => command === 'list_network_instance_ids'
      ? { running_inst_ids: [], disabled_inst_ids: [], runtime_capabilities: ['config:sni'] }
      : command === 'parse_network_config' ? 'network_name = "mesh"' : undefined)
    const api = new GUIRemoteClient()
    const config = { ...NetworkTypes.DEFAULT_NETWORK_CONFIG(), sni: 'example.com', enable_bbr: true,
      listener_urls: ['ws://0.0.0.0:1', 'wss://0.0.0.0:2', 'http3://0.0.0.0:3'] }
    await api.save_config(config)
    await api.validate_config(config)
    await api.run_network(config, true)
    await api.generate_config(config)
    for (const [command, args] of invoke.mock.calls.filter(([command]) => command !== 'list_network_instance_ids')) {
      expect(args.cfg.sni, command).toBe('example.com')
      expect(args.cfg, command).not.toHaveProperty('enable_bbr')
      expect(args.cfg, command).not.toHaveProperty('p2p_prefer_protocol')
      expect(args.cfg.listener_urls, command).toEqual(['ws://0.0.0.0:1', 'wss://0.0.0.0:2'])
    }
  })

  it('passes sparse patches as JSON objects with protobuf UUID parts and handles void lifecycle results', async () => {
    invoke.mockImplementation(async (command: string) => command === 'list_network_instance_ids'
      ? { running_inst_ids: [], disabled_inst_ids: [], runtime_capabilities: ['management:persisted-config-revision-v1'] }
      : command === 'patch_local_config' ? { status: 'Success' }
      : command === 'observe_local_configs' ? { catalog_epoch: 'boot-a', catalog_generation: '1', entries: [] } : undefined)
    const api = new GUIRemoteClient()
    const snapshot = await api.observe_local_configs()
    expect(snapshot).toMatchObject({ online: true, stale: false, capabilities: ['management:persisted-config-revision-v1'] })
    await api.patch_local_config({ inst_id: id, expected_revision: 'revision-a', field_mask: ['hostname'], config: { hostname: 'edited' }, apply_mode: 1 })
    const args = invoke.mock.calls.find(([command]) => command === 'patch_local_config')![1]
    expect(args.request).toEqual({ inst_id: { part1: 0, part2: 0, part3: 0, part4: 1 }, expected_revision: 'revision-a', field_mask: ['hostname'], config: { hostname: 'edited' }, apply_mode: 1 })
    expect(await api.update_network_instance_state(id, false, 'revision-b')).toBeUndefined()
    expect(invoke).toHaveBeenCalledWith('update_network_config_state', { instanceId: id, expectedRevision: 'revision-b', disabled: false })
    expect(await api.delete_network(id, 'revision-c')).toBeUndefined()
    expect(invoke).toHaveBeenCalledWith('remove_network_instance', { instanceId: id, expectedRevision: 'revision-c' })
  })

  it('blocks an old-node extension patch and a pending write after switching the active connection', async () => {
    let resolve!: (response: object) => void
    invoke.mockImplementation(() => new Promise(done => { resolve = done }))
    let scope = 'remote-a'
    const api = new GUIRemoteClient(() => scope)
    const save = api.save_config(NetworkTypes.DEFAULT_NETWORK_CONFIG())
    scope = 'remote-b'
    resolve({ running_inst_ids: [], disabled_inst_ids: [], runtime_capabilities: [] })
    await expect(save).rejects.toThrow('Management connection changed')
    expect(invoke.mock.calls.map(([command]) => command)).toEqual(['list_network_instance_ids'])
    invoke.mockResolvedValue({ running_inst_ids: [], disabled_inst_ids: [], runtime_capabilities: [] })
    await expect(new GUIRemoteClient(() => scope).patch_local_config({ inst_id: id, expected_revision: 'revision-a', field_mask: ['enable_bbr'], config: { enable_bbr: true }, apply_mode: 0 })).rejects.toThrow('unsupported')
    expect(invoke.mock.calls.some(([command]) => command === 'patch_local_config')).toBe(false)
  })

  it.each(['udp', 'tcp'])('keeps an old custom node saved %s preference intact by refusing an unsafe legacy write', async protocol => {
    invoke.mockImplementation(async command => command === 'list_network_instance_ids'
      ? { running_inst_ids: [], disabled_inst_ids: [id], runtime_capabilities: [] }
      : command === 'get_config' ? { instance_id: id, hostname: 'saved', p2p_prefer_protocol: protocol, peer_urls: ['wss://saved.example:443'] } : undefined)
    const api = new GUIRemoteClient()
    const config = await api.get_network_config(id)
    await expect(api.save_config({ ...config, hostname: 'edited' })).rejects.toThrow('saved custom settings')
    await expect(api.run_network({ ...config, hostname: 'edited' }, true)).rejects.toThrow('saved custom settings')
    expect(invoke.mock.calls.some(([command]) => command === 'save_network_config' || command === 'run_network_instance')).toBe(false)
  })
})
