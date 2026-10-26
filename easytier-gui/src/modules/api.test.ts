import { beforeEach, describe, expect, it, vi } from 'vitest'
import { NetworkTypes, LocalConfigs, Capabilities, Utils } from 'easytier-frontend-lib'
import { GUIRemoteClient } from './api'
import creationFixtures from '../../../easytier-web/frontend-lib/tests/fixtures/network-create-patches.json'

const invoke = vi.hoisted(() => vi.fn())
vi.mock('@tauri-apps/api/core', () => ({ invoke }))
const id = '00000000-0000-0000-0000-000000000001'
beforeEach(() => invoke.mockReset())

describe('GUI management bridge capabilities and revisions', () => {
  it.each(creationFixtures)('forwards the real Core creation fixture: $name', async (fixture) => {
    invoke.mockImplementation(async command => command === 'list_network_instance_ids'
      ? { running_inst_ids: [], disabled_inst_ids: [], runtime_capabilities: fixture.capabilities }
      : { status: 'Success' })
    await new GUIRemoteClient().patch_local_config(fixture.request)
    expect(invoke).toHaveBeenCalledWith('patch_local_config', { request: {
      ...fixture.request, inst_id: Utils.StrToUuid(fixture.request.inst_id),
    } })
    expect(invoke.mock.calls.some(([command]) => command === 'save_network_config')).toBe(false)
  })

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

  it('removes unsupported extensions from validation and TOML generation, keeping WS/WSS', async () => {
    invoke.mockImplementation(async (command: string) => command === 'list_network_instance_ids'
      ? { running_inst_ids: [], disabled_inst_ids: [], runtime_capabilities: ['config:sni'] }
      : command === 'parse_network_config' ? 'network_name = "mesh"' : undefined)
    const api = new GUIRemoteClient()
    const config = { ...NetworkTypes.DEFAULT_NETWORK_CONFIG(), sni: 'example.com', close_redundant_conns_when_disguised: true,
      listener_urls: ['ws://0.0.0.0:1', 'wss://0.0.0.0:2', 'http3://0.0.0.0:3'] }
    await api.validate_config(config)
    await api.generate_config(config)
    for (const [command, args] of invoke.mock.calls.filter(([command]) => command !== 'list_network_instance_ids')) {
      const sent = args.cfg ?? args.config
      expect(sent.sni, command).toBe('example.com')
      expect(sent, command).not.toHaveProperty('close_redundant_conns_when_disguised')
      expect(sent, command).not.toHaveProperty('p2p_prefer_protocol')
      expect(sent.listener_urls, command).toEqual(['ws://0.0.0.0:1', 'wss://0.0.0.0:2'])
    }
    await expect(api.save_config()).rejects.toThrow('config directory')
    await expect(api.run_network()).rejects.toThrow('config directory')
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
    const save = api.observe_local_configs()
    scope = 'remote-b'
    resolve({ running_inst_ids: [], disabled_inst_ids: [], runtime_capabilities: [] })
    await expect(save).rejects.toThrow('Management connection changed')
    expect(invoke.mock.calls.map(([command]) => command)).toEqual(['list_network_instance_ids'])
    invoke.mockResolvedValue({ running_inst_ids: [], disabled_inst_ids: [], runtime_capabilities: [] })
    await expect(new GUIRemoteClient(() => scope).patch_local_config({ inst_id: id, expected_revision: 'revision-a', field_mask: ['close_redundant_conns_when_disguised'], config: { close_redundant_conns_when_disguised: true }, apply_mode: 0 })).rejects.toThrow(/unsupported/i)
    expect(invoke.mock.calls.some(([command]) => command === 'patch_local_config')).toBe(false)
  })
})
