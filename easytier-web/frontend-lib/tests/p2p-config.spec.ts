import { describe, expect, it } from 'vitest'
import { DEFAULT_NETWORK_CONFIG, defaultFlagValue, normalizeNetworkConfig, toBackendNetworkConfig } from '../src/types/network'

describe('P2P configuration defaults', () => {
  it('defaults new configurations to preferring disguise, UDP and enabled cleanup', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    expect(config.prefer_wss_http3_for_p2p).toBe(true)
    expect(config.only_use_wss_http3_for_hole_punching).toBe(false)
    expect(config.disable_wss_http3_for_p2p).toBe(false)
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
    const config = normalizeNetworkConfig({ ...DEFAULT_NETWORK_CONFIG(), ...settings })
    const saved = toBackendNetworkConfig(config)
    expect(saved).toMatchObject(settings)
    expect(normalizeNetworkConfig(saved)).toMatchObject(settings)
  })
})
