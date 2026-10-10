import { describe, expect, it } from 'vitest'
import { DEFAULT_NETWORK_CONFIG, normalizeNetworkConfig, toBackendNetworkConfig } from '../src/types/network'

describe('P2P configuration defaults', () => {
  it('defaults new and legacy configurations to passive disguise use, UDP and enabled cleanup', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    expect(config.prefer_wss_http3_for_p2p).toBe(false)
    expect(config.p2p_prefer_protocol).toBe('udp')
    expect(config.close_redundant_conns_when_disguised).toBe(true)

    delete config.prefer_wss_http3_for_p2p
    delete config.p2p_prefer_protocol
    delete config.close_redundant_conns_when_disguised
    const restored = normalizeNetworkConfig(config)
    expect(restored.prefer_wss_http3_for_p2p).toBe(false)
    expect(restored.p2p_prefer_protocol).toBe('udp')
    expect(restored.only_use_wss_http3_for_hole_punching).toBe(false)
    expect(restored.disable_wss_http3_for_p2p).toBe(false)
    expect(restored.close_redundant_conns_when_disguised).toBe(true)
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
