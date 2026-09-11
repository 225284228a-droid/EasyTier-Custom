import { describe, expect, it } from 'vitest'
import { DEFAULT_NETWORK_CONFIG, normalizeNetworkConfig, toBackendNetworkConfig } from '../src/types/network'

describe('BBR configuration', () => {
  it('defaults to off for new and legacy configurations', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    expect(config.enable_bbr).toBe(false)
    delete config.enable_bbr
    expect(normalizeNetworkConfig(config).enable_bbr).toBe(false)
  })

  it.each([false, true])('preserves %s through saving and readback without enabling QUIC proxy', (enable_bbr) => {
    const config = normalizeNetworkConfig({ ...DEFAULT_NETWORK_CONFIG(), enable_bbr })
    const saved = toBackendNetworkConfig(config)
    expect(saved.enable_bbr).toBe(enable_bbr)
    expect(normalizeNetworkConfig(saved).enable_bbr).toBe(enable_bbr)
    expect(saved.enable_quic_proxy).toBe(false)
  })
})
