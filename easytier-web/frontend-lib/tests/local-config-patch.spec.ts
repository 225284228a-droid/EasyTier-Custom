import { describe, expect, it } from 'vitest'
import { DEFAULT_NETWORK_CONFIG, normalizeNetworkConfig } from '../src/types/network'
import { buildLocalConfigPatch, buildLocalConfigCreatePatch, buildLocalConfigReplace, changedConfigFields, localConfigEditable, localConfigInstanceId, type LocalConfigEntry } from '../src/modules/localConfigPatch'
import { LOCAL_CONFIG_REVISION_CAPABILITY } from '../src/modules/capabilities'
import { DEPRECATED_NETWORK_CONFIG_FIELDS } from '../src/generated/proto/networkConfigFields'
import creationFixtures from './fixtures/network-create-patches.json'

const entry = (): LocalConfigEntry => ({
  entry_key: 'a.toml', inst_id: '00000000-0000-0000-0000-000000000001', revision: 'revision-a',
  config: DEFAULT_NETWORK_CONFIG(), config_permission: 0, enabled: true, running: true,
  persisted_raw_hash: 'hash-a', status: 'ready', network_name: 'mesh', source: 0,
  persisted_toml: '[future]\noption = "preserved on this node"',
})

describe('online local configuration patches', () => {
  it.each([undefined, null])('isolates an entry without a UUID (%s) from valid catalog entries', instId => {
    const invalid = { ...entry(), inst_id: instId, status: 'invalid', config: undefined }
    const valid = entry()
    const snapshot = { online: true, stale: false, capabilities: [LOCAL_CONFIG_REVISION_CAPABILITY],
      entries: [invalid, valid], catalog_epoch: 'boot-a', catalog_generation: 1 }
    expect(localConfigInstanceId(invalid)).toBe('')
    expect(snapshot.entries.find(candidate => localConfigInstanceId(candidate) === valid.inst_id)).toBe(valid)
    expect(localConfigEditable(snapshot, invalid)).toBe(false)
    expect(localConfigEditable(snapshot, { ...valid, inst_id: instId })).toBe(false)
    expect(localConfigEditable(snapshot, valid)).toBe(true)
    expect(() => buildLocalConfigReplace(invalid, 'x = 1')).toThrow('identity is unavailable')
    expect(() => buildLocalConfigPatch(invalid, valid.config!, ['hostname'], [])).toThrow('identity is unavailable')
  })

  it.each(creationFixtures)('matches the Core creation contract: $name', (fixture) => {
    const config = { ...DEFAULT_NETWORK_CONFIG(), ...fixture.overrides }
    const patch = buildLocalConfigCreatePatch(config, fixture.capabilities)
    // Core consumes this same fixture through its real persisted RPC. Neither
    // side can bless a generated mask by merely mocking a successful save.
    expect(patch).toEqual(fixture.request)
    expect(patch.field_mask.filter(field => (DEPRECATED_NETWORK_CONFIG_FIELDS as readonly string[]).includes(field))).toEqual([])
  })

  it('excludes deprecated fields from edit diffs and rejects explicit patches for them', () => {
    const source = entry()
    const edited = normalizeNetworkConfig(source.config!)
    edited.enable_vpn_portal = true
    edited.quic_listen_port = 12345
    expect(changedConfigFields(source.config!, edited)).toEqual([])
    for (const field of DEPRECATED_NETWORK_CONFIG_FIELDS) {
      expect(() => buildLocalConfigPatch(source, edited, [field], [])).toThrow(`Invalid configuration field: ${field}`)
    }
  })

  it('replaces the whole file at the observed revision without typed fields', () => {
    const source = entry()
    expect(buildLocalConfigReplace(source, source.persisted_toml!)).toEqual({ inst_id: source.inst_id,
      expected_revision: source.revision, field_mask: [], toml: source.persisted_toml })
    expect(() => buildLocalConfigReplace({ ...source, revision: '' }, 'x = 1')).toThrow('revision is unavailable')
    expect(() => buildLocalConfigPatch(source, source.config!, [], [])).toThrow('No configuration fields selected')
  })

  it('sends only changed fields with the target revision and retains explicit false and deletion', () => {
    const source = entry()
    source.config!.close_redundant_conns_when_disguised = true
    source.config!.vpn_portal_config = { enabled: true, wireguard_listen: '0.0.0.0:22022', clients: [] }
    const edited = normalizeNetworkConfig(source.config!)
    edited.close_redundant_conns_when_disguised = false
    edited.vpn_portal_config = undefined
    const mask = changedConfigFields(source.config!, edited)
    expect(mask).toEqual(expect.arrayContaining(['close_redundant_conns_when_disguised', 'vpn_portal_config']))
    const patch = buildLocalConfigPatch(source, edited, mask, ['config:close_redundant_conns_when_disguised'])
    expect(patch.expected_revision).toBe('revision-a')
    expect(patch).not.toHaveProperty('apply_mode')
    expect(patch.config!.close_redundant_conns_when_disguised).toBe(false)
    expect(patch.config).not.toHaveProperty('vpn_portal_config')
    expect(patch.config).not.toHaveProperty('network_secret')
    expect(patch.config).not.toHaveProperty('persisted_toml')
    expect(source.persisted_toml).toContain('[future]')
  })

  it('uses each target own revision without copying source identity', () => {
    const first = entry()
    const second = { ...entry(), revision: 'revision-b', inst_id: '00000000-0000-0000-0000-000000000002' }
    const edit = normalizeNetworkConfig(first.config!)
    edit.hostname = 'new-hostname'
    const patch = buildLocalConfigPatch(second, edit, ['hostname'], [])
    expect(patch).toMatchObject({ inst_id: second.inst_id, expected_revision: 'revision-b', config: { hostname: 'new-hostname' } })
    expect(patch.config).not.toHaveProperty('instance_id')
    expect(() => buildLocalConfigPatch(second, edit, ['instance_id'], [])).toThrow('Invalid configuration field')
    expect(() => buildLocalConfigPatch(second, edit, ['future_option'], [])).toThrow('Invalid configuration field')
  })

  it('disables offline, stale, protected and older management nodes independently of transport caps', () => {
    const snapshot = { online: true, stale: false, capabilities: [LOCAL_CONFIG_REVISION_CAPABILITY], entries: [], catalog_epoch: 'boot-a', catalog_generation: 1 }
    expect(localConfigEditable(snapshot, entry())).toBe(true)
    expect(localConfigEditable({ ...snapshot, online: false }, entry())).toBe(false)
    expect(localConfigEditable({ ...snapshot, stale: true }, entry())).toBe(false)
    expect(localConfigEditable({ ...snapshot, capabilities: ['management:persisted-config-revision-v1'] }, entry())).toBe(false)
    expect(localConfigEditable(snapshot, { ...entry(), config_permission: 1 })).toBe(false)
    expect(localConfigEditable(snapshot, { ...entry(), status: 'protected', config: undefined })).toBe(false)
  })

  it('creates with an empty revision and individually supported fields', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    const patch = buildLocalConfigCreatePatch(config, ['config:close_redundant_conns_when_disguised'])
    expect(patch.expected_revision).toBe('')
    expect(patch).not.toHaveProperty('apply_mode')
    expect(patch.field_mask).toContain('close_redundant_conns_when_disguised')
    expect(patch.field_mask).not.toContain('sni')
    expect(patch.field_mask).not.toContain('instance_id')
    expect(patch.config).not.toHaveProperty('instance_id')
  })
})
