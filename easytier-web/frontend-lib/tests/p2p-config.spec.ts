import { describe, expect, it } from 'vitest'
import { DEFAULT_NETWORK_CONFIG, defaultFlagValue, normalizeNetworkConfig, toBackendNetworkConfig } from '../src/types/network'
import { LEGACY_P2P_DISGUISE_FIELDS, P2P_DISGUISE_MODE_CAPABILITY, p2pDisguiseMode } from '../src/modules/p2pDisguise'

describe('P2P configuration defaults', () => {
  it('defaults new configurations to preferring disguise, UDP and enabled cleanup', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    expect(config.p2p_disguise_mode).toBe('prefer')
    for (const field of LEGACY_P2P_DISGUISE_FIELDS) expect(config[field]).toBeUndefined()
    expect(config.p2p_prefer_protocol).toBe('udp')
    expect(config.close_redundant_conns_when_disguised).toBe(true)
  })

  it('reads the disguise preference back from the schema default when unset', () => {
    // A network that states nothing runs with the annotation default, so an
    // unset config displays as preferring disguise rather than as "do not
    // initiate".
    expect(defaultFlagValue('prefer_wss_http3_for_p2p')).toBe(true)
    expect(defaultFlagValue('only_use_wss_http3_for_hole_punching')).toBe(false)
    expect(defaultFlagValue('disable_wss_http3_for_p2p')).toBe(false)
  })

  it('keeps legacy settings unset through normalization, saving and readback', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    delete config.p2p_disguise_mode
    delete config.prefer_wss_http3_for_p2p
    delete config.only_use_wss_http3_for_hole_punching
    delete config.disable_wss_http3_for_p2p
    delete config.p2p_prefer_protocol
    delete config.close_redundant_conns_when_disguised
    const restored = normalizeNetworkConfig(config)
    const saved = toBackendNetworkConfig(restored)
    const readback = normalizeNetworkConfig(saved)
    for (const field of [
      'prefer_wss_http3_for_p2p',
      'only_use_wss_http3_for_hole_punching',
      'disable_wss_http3_for_p2p',
      'p2p_prefer_protocol',
      'close_redundant_conns_when_disguised',
    ] as const) {
      expect(restored[field], `${field} should remain unset in the form`).toBeUndefined()
      expect(saved, `${field} should not be written`).not.toHaveProperty(field)
      expect(readback[field], `${field} should remain unset on readback`).toBeUndefined()
    }
  })

  it.each([
    { prefer_wss_http3_for_p2p: true, p2p_prefer_protocol: 'tcp', close_redundant_conns_when_disguised: false },
    { prefer_wss_http3_for_p2p: false, p2p_prefer_protocol: 'udp', close_redundant_conns_when_disguised: true },
  ])('preserves explicit settings through saving and readback: %j', (settings) => {
    const config = normalizeNetworkConfig({ ...DEFAULT_NETWORK_CONFIG(), ...settings, p2p_disguise_mode: undefined })
    const capabilities = [...LEGACY_P2P_DISGUISE_FIELDS.map(field => `config:${field}`), 'config:p2p_prefer_protocol', 'config:close_redundant_conns_when_disguised']
    const saved = toBackendNetworkConfig(config, capabilities)
    expect(saved).toMatchObject(settings)
    expect(normalizeNetworkConfig(saved)).toMatchObject(settings)
    const modern = toBackendNetworkConfig(config, [...capabilities, P2P_DISGUISE_MODE_CAPABILITY])
    expect(modern.p2p_disguise_mode).toBe(p2pDisguiseMode(config))
    for (const field of LEGACY_P2P_DISGUISE_FIELDS) expect(modern).not.toHaveProperty(field)
  })
})
