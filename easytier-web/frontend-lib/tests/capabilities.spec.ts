import { describe, expect, it } from 'vitest'
import { assertLegacyConfigPreservable, assertPatchCapabilities, CONFIG_FIELD_CAPABILITIES, filterConfigPayload, HTTP3_CAPABILITY } from '../src/modules/capabilities'
import { DEFAULT_NETWORK_CONFIG } from '../src/types/network'

describe('configuration capabilities', () => {
  it('removes unsupported extension fields including explicit false values', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    config.enable_bbr = false
    const payload = filterConfigPayload(config, [])
    for (const field of Object.keys(CONFIG_FIELD_CAPABILITIES)) expect(payload).not.toHaveProperty(field)
    expect(payload).toHaveProperty('disable_p2p', false)
    expect(config).toHaveProperty('enable_bbr', false)
  })

  it('supports each extension independently from management and node version', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    config.enable_bbr = true
    const payload = filterConfigPayload(config, ['config:enable_bbr', 'management:persisted-config-revision-v1'])
    expect(payload.enable_bbr).toBe(true)
    expect(payload).not.toHaveProperty('sni')
    expect(payload).not.toHaveProperty('p2p_prefer_protocol')
    expect(filterConfigPayload(config, ['supports_persisted_config_management'])).not.toHaveProperty('enable_bbr')
  })

  it('retains official websocket URLs while filtering only the custom HTTP3 transport', () => {
    const config = {
      listener_urls: ['ws://host:1', 'wss://host:2?padding=100,64', 'http3://host:3'],
      peer_urls: ['tcp://host:1', 'http3://host:3'],
      peers: [{ uri: 'wss://host:2' }, { uri: 'http3://host:3' }],
    }
    expect(filterConfigPayload(config, []).listener_urls).toEqual(config.listener_urls.slice(0, 2))
    expect(filterConfigPayload(config, []).peers).toEqual([{ uri: 'wss://host:2' }])
    expect(filterConfigPayload(config, [HTTP3_CAPABILITY])).toEqual(config)
  })

  it('rejects an explicit unsupported patch instead of changing the requested edit', () => {
    expect(() => assertPatchCapabilities({ enable_bbr: false }, ['enable_bbr'], [])).toThrow('Unsupported configuration field')
    expect(() => assertPatchCapabilities({ listener_urls: ['http3://host:3'] }, ['listener_urls'], [])).toThrow('Unsupported HTTP3')
    expect(() => assertPatchCapabilities({ listener_urls: ['wss://host:2'] }, ['listener_urls'], [])).not.toThrow()
    expect(() => assertPatchCapabilities({ enable_bbr: true }, ['enable_bbr'], ['config:enable_bbr'])).not.toThrow()
  })

  it('blocks legacy replacements of stored TCP/UDP preferences or HTTP3 URLs without advertised capabilities', () => {
    for (const protocol of ['tcp', 'udp'] as const) {
      expect(() => assertLegacyConfigPreservable({ p2p_prefer_protocol: protocol }, [])).toThrow('p2p_prefer_protocol')
      expect(() => assertLegacyConfigPreservable({ p2p_prefer_protocol: protocol }, ['config:p2p_prefer_protocol'])).not.toThrow()
    }
    expect(() => assertLegacyConfigPreservable({ peer_urls: ['http3://saved:443'] }, [])).toThrow('peer_urls')
    expect(() => assertLegacyConfigPreservable({ peer_urls: ['http3://saved:443'] }, [HTTP3_CAPABILITY])).not.toThrow()
    expect(() => assertLegacyConfigPreservable({ enable_bbr: false, peer_urls: ['wss://saved:443'] }, [])).not.toThrow()
  })
})
