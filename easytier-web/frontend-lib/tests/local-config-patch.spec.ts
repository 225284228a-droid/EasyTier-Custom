import { describe, expect, it } from 'vitest'
import { DEFAULT_NETWORK_CONFIG, normalizeNetworkConfig } from '../src/types/network'
import { buildLocalConfigApplyRequest, buildLocalConfigPatch, buildLocalConfigCreatePatch, changedConfigFields, localConfigEditable, LocalConfigApplyMode, type LocalConfigEntry } from '../src/modules/localConfigPatch'
import { LOCAL_CONFIG_APPLY_CAPABILITY, LOCAL_CONFIG_REVISION_CAPABILITY } from '../src/modules/capabilities'

const entry = (): LocalConfigEntry => ({
  entry_key: 'a.toml', inst_id: '00000000-0000-0000-0000-000000000001', revision: 'revision-a',
  config: DEFAULT_NETWORK_CONFIG(), config_permission: 0, enabled: true, running: true,
  persisted_raw_hash: 'hash-a', pending_apply: false, status: 'ready', network_name: 'mesh', source: 0,
  persisted_toml: '[future]\noption = "preserved on this node"',
})

describe('online local configuration patches', () => {
  it('applies only the observed revision without copying its projected fields or TOML', () => {
    const source = entry()
    const patch = buildLocalConfigApplyRequest(source, [LOCAL_CONFIG_APPLY_CAPABILITY])
    expect(patch).toEqual({ inst_id: source.inst_id, expected_revision: source.revision,
      config: {}, field_mask: [], apply_mode: LocalConfigApplyMode.SaveAndApply })
    expect(() => buildLocalConfigApplyRequest(source, [LOCAL_CONFIG_REVISION_CAPABILITY])).toThrow('Upgrade the device')
    expect(() => buildLocalConfigApplyRequest({ ...source, revision: '' }, [LOCAL_CONFIG_APPLY_CAPABILITY])).toThrow('revision is unavailable')
    expect(() => buildLocalConfigPatch(source, source.config!, [], [LOCAL_CONFIG_APPLY_CAPABILITY])).toThrow('No configuration fields selected')
    expect(() => buildLocalConfigPatch(source, source.config!, [], [LOCAL_CONFIG_APPLY_CAPABILITY], LocalConfigApplyMode.PersistOnly)).toThrow('No configuration fields selected')
  })

  it('sends only changed fields with the target revision and retains explicit false and deletion', () => {
    const source = entry()
    source.config!.enable_bbr = true
    source.config!.vpn_portal_config = { enabled: true, wireguard_listen: '0.0.0.0:22022', clients: [] }
    const edited = normalizeNetworkConfig(source.config!)
    edited.enable_bbr = false
    edited.vpn_portal_config = undefined
    const mask = changedConfigFields(source.config!, edited)
    expect(mask).toEqual(expect.arrayContaining(['enable_bbr', 'vpn_portal_config']))
    const patch = buildLocalConfigPatch(source, edited, mask, ['config:enable_bbr'])
    expect(patch.expected_revision).toBe('revision-a')
    expect(patch.apply_mode).toBe(0)
    expect(patch.config.enable_bbr).toBe(false)
    expect(patch.config).not.toHaveProperty('vpn_portal_config')
    expect(patch.config).not.toHaveProperty('network_secret')
    expect(patch.config).not.toHaveProperty('persisted_toml')
    expect(source.persisted_toml).toContain('[future]')
  })

  it('uses each target own revision and allows persist-only without copying source identity', () => {
    const first = entry()
    const second = { ...entry(), revision: 'revision-b', inst_id: '00000000-0000-0000-0000-000000000002' }
    const edit = normalizeNetworkConfig(first.config!)
    edit.hostname = 'new-hostname'
    const patch = buildLocalConfigPatch(second, edit, ['hostname'], [], LocalConfigApplyMode.PersistOnly)
    expect(patch).toMatchObject({ inst_id: second.inst_id, expected_revision: 'revision-b', apply_mode: 1, config: { hostname: 'new-hostname' } })
    expect(patch.config).not.toHaveProperty('instance_id')
    expect(() => buildLocalConfigPatch(second, edit, ['instance_id'], [])).toThrow('Invalid configuration field')
    expect(() => buildLocalConfigPatch(second, edit, ['future_option'], [])).toThrow('Invalid configuration field')
  })

  it('disables offline, stale, protected and old management nodes independently of transport caps', () => {
    const snapshot = { online: true, stale: false, capabilities: ['management:persisted-config-revision-v1'], entries: [], catalog_epoch: 'boot-a', catalog_generation: 1 }
    expect(localConfigEditable(snapshot, entry())).toBe(true)
    expect(localConfigEditable({ ...snapshot, online: false }, entry())).toBe(false)
    expect(localConfigEditable({ ...snapshot, stale: true }, entry())).toBe(false)
    expect(localConfigEditable({ ...snapshot, capabilities: ['transport:http3-framed-v1'] }, entry())).toBe(false)
    expect(localConfigEditable(snapshot, { ...entry(), config_permission: 1 })).toBe(false)
    expect(localConfigEditable(snapshot, { ...entry(), status: 'protected', config: undefined })).toBe(false)
  })

  it('creates with an empty revision, persist-only and individually supported fields', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    const patch = buildLocalConfigCreatePatch(config, ['config:enable_bbr'])
    expect(patch.expected_revision).toBe('')
    expect(patch.apply_mode).toBe(1)
    expect(patch.field_mask).toContain('enable_bbr')
    expect(patch.field_mask).not.toContain('sni')
    expect(patch.field_mask).not.toContain('instance_id')
    expect(patch.config).not.toHaveProperty('instance_id')
  })
})
