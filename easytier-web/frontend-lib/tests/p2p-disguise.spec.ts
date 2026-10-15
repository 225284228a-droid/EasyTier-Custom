import { describe, expect, it } from 'vitest'
import { assertLegacyConfigPreservable, assertPatchCapabilities, filterConfigPayload } from '../src/modules/capabilities'
import { buildLocalConfigPatch, type LocalConfigEntry } from '../src/modules/localConfigPatch'
import { LEGACY_P2P_DISGUISE_FIELDS, P2P_DISGUISE_MODE_CAPABILITY, p2pDisguiseMode, setP2pDisguiseMode, type P2pDisguiseMode } from '../src/modules/p2pDisguise'
import { DEFAULT_NETWORK_CONFIG } from '../src/types/network'

const legacyCapabilities = LEGACY_P2P_DISGUISE_FIELDS.map(field => `config:${field}`)

describe('unified automatic P2P disguise policy', () => {
  it('distinguishes missing settings from explicit legacy off and gives mode precedence', () => {
    expect(p2pDisguiseMode({})).toBe('prefer')
    expect(p2pDisguiseMode({ prefer_wss_http3_for_p2p: false })).toBe('off')
    expect(p2pDisguiseMode({ disable_wss_http3_for_p2p: true })).toBe('disabled')
    expect(p2pDisguiseMode({ only_use_wss_http3_for_hole_punching: true })).toBe('only')
    expect(p2pDisguiseMode({ p2p_disguise_mode: 'off', only_use_wss_http3_for_hole_punching: true })).toBe('off')
    expect(p2pDisguiseMode({ p2p_disguise_mode: ' ONLY ' })).toBe('only')
    expect(filterConfigPayload({ p2p_disguise_mode: ' ONLY ' }, [P2P_DISGUISE_MODE_CAPABILITY]).p2p_disguise_mode).toBe('only')
    expect(filterConfigPayload({ p2p_disguise_mode: ' ONLY ' }, legacyCapabilities)).toMatchObject({ only_use_wss_http3_for_hole_punching: true })
  })

  it.each<P2pDisguiseMode>(['prefer', 'off', 'disabled', 'only'])('writes one field on new kernels and preserves the old custom protocol: %s', (mode) => {
    const config = DEFAULT_NETWORK_CONFIG()
    setP2pDisguiseMode(config, mode)
    expect(config.p2p_disguise_mode).toBe(mode)
    for (const field of LEGACY_P2P_DISGUISE_FIELDS) expect(config).not.toHaveProperty(field)
    expect(filterConfigPayload(config, [P2P_DISGUISE_MODE_CAPABILITY]).p2p_disguise_mode).toBe(mode)
    const legacy = filterConfigPayload(config, legacyCapabilities)
    expect(legacy).not.toHaveProperty('p2p_disguise_mode')
    expect(p2pDisguiseMode(legacy)).toBe(mode)
    expect(legacy.prefer_wss_http3_for_p2p).toBe(mode === 'prefer')
    expect(legacy.disable_wss_http3_for_p2p).toBe(mode === 'disabled')
    expect(legacy.only_use_wss_http3_for_hole_punching).toBe(mode === 'only')
    setP2pDisguiseMode(config, mode, legacyCapabilities)
    expect(config).not.toHaveProperty('p2p_disguise_mode')
    expect(p2pDisguiseMode(config)).toBe(mode)
  })

  it('never sends custom policy fields to official kernels or permits unsupported patches', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    setP2pDisguiseMode(config, 'only')
    const official = filterConfigPayload(config, [])
    for (const field of ['p2p_disguise_mode', ...LEGACY_P2P_DISGUISE_FIELDS]) expect(official).not.toHaveProperty(field)
    expect(() => assertPatchCapabilities(config, ['p2p_disguise_mode'], [])).toThrow('Unsupported configuration field')
    expect(() => assertPatchCapabilities(config, ['p2p_disguise_mode'], legacyCapabilities)).not.toThrow()
    expect(() => assertLegacyConfigPreservable({ p2p_disguise_mode: 'only' }, legacyCapabilities)).not.toThrow()
  })

  it('adapts legacy persisted patches including their field mask', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    setP2pDisguiseMode(config, 'only')
    const patch = buildLocalConfigPatch({ inst_id: config.instance_id, revision: 'rev' } as LocalConfigEntry,
      config, ['p2p_disguise_mode'], legacyCapabilities)
    expect(patch.field_mask).toEqual([...LEGACY_P2P_DISGUISE_FIELDS])
    expect(patch.config).not.toHaveProperty('p2p_disguise_mode')
    expect(patch.config.only_use_wss_http3_for_hole_punching).toBe(true)
    expect(patch.config.prefer_wss_http3_for_p2p).toBe(false)
  })
})
