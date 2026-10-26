import { describe, expect, it } from 'vitest'
import { DEFAULT_NETWORK_CONFIG, normalizeNetworkConfig, toBackendNetworkConfig } from '../src/types/network'

describe('P2P configuration defaults', () => {
  it('defaults new configurations to preferring disguise, UDP and enabled cleanup', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    expect(config.p2p_disguise_mode).toBe('prefer')
    expect(config.p2p_prefer_protocol).toBe('udp')
    expect(config.close_redundant_conns_when_disguised).toBe(true)
  })

  it('keeps unset settings unset through normalization, saving and readback', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    delete config.p2p_disguise_mode
    delete config.p2p_prefer_protocol
    delete config.close_redundant_conns_when_disguised
    const restored = normalizeNetworkConfig(config)
    const saved = toBackendNetworkConfig(restored)
    const readback = normalizeNetworkConfig(saved)
    for (const field of [
      'p2p_disguise_mode',
      'p2p_prefer_protocol',
      'close_redundant_conns_when_disguised',
    ] as const) {
      expect(restored[field], `${field} should remain unset in the form`).toBeUndefined()
      expect(saved, `${field} should not be written`).not.toHaveProperty(field)
      expect(readback[field], `${field} should remain unset on readback`).toBeUndefined()
    }
  })

  it.each([
    { p2p_disguise_mode: 'only', p2p_prefer_protocol: 'tcp', close_redundant_conns_when_disguised: false },
    { p2p_disguise_mode: 'off', p2p_prefer_protocol: 'udp', close_redundant_conns_when_disguised: true },
  ])('preserves explicit settings through saving and readback: %j', (settings) => {
    const config = normalizeNetworkConfig({ ...DEFAULT_NETWORK_CONFIG(), ...settings })
    const capabilities = ['config:p2p_disguise_mode', 'config:p2p_prefer_protocol', 'config:close_redundant_conns_when_disguised']
    const saved = toBackendNetworkConfig(config, capabilities)
    expect(saved).toMatchObject(settings)
    expect(normalizeNetworkConfig(saved)).toMatchObject(settings)
  })
})
