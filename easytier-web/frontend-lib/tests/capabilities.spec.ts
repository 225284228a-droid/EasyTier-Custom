import { describe, expect, it } from 'vitest'
import { assertLegacyConfigPreservable, assertPatchCapabilities, CONFIG_FIELD_CAPABILITIES, filterConfigPayload, HTTP3_CAPABILITY } from '../src/modules/capabilities'
import { DEFAULT_NETWORK_CONFIG } from '../src/types/network'

describe('configuration capabilities', () => {
  it('removes unsupported extension fields including explicit false values', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    config.close_redundant_conns_when_disguised = false
    const payload = filterConfigPayload(config, [])
    for (const field of Object.keys(CONFIG_FIELD_CAPABILITIES)) expect(payload).not.toHaveProperty(field)
    expect(payload).toHaveProperty('disable_p2p', false)
    expect(config).toHaveProperty('close_redundant_conns_when_disguised', false)
  })

  it('supports each extension independently from management and node version', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    config.close_redundant_conns_when_disguised = true
    const payload = filterConfigPayload(config, ['config:close_redundant_conns_when_disguised', 'management:persisted-config-revision-v1'])
    expect(payload.close_redundant_conns_when_disguised).toBe(true)
    expect(payload).not.toHaveProperty('sni')
    expect(payload).not.toHaveProperty('p2p_prefer_protocol')
    expect(filterConfigPayload(config, ['supports_persisted_config_management'])).not.toHaveProperty('close_redundant_conns_when_disguised')
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
    expect(() => assertPatchCapabilities({ close_redundant_conns_when_disguised: false }, ['close_redundant_conns_when_disguised'], [])).toThrow('Unsupported configuration field')
    expect(() => assertPatchCapabilities({ listener_urls: ['http3://host:3'] }, ['listener_urls'], [])).toThrow('Unsupported HTTP3')
    expect(() => assertPatchCapabilities({ listener_urls: ['wss://host:2'] }, ['listener_urls'], [])).not.toThrow()
    expect(() => assertPatchCapabilities({ close_redundant_conns_when_disguised: true }, ['close_redundant_conns_when_disguised'], ['config:close_redundant_conns_when_disguised'])).not.toThrow()
  })

  it('blocks legacy replacements of stored TCP/UDP preferences or HTTP3 URLs without advertised capabilities', () => {
    for (const protocol of ['tcp', 'udp'] as const) {
      expect(() => assertLegacyConfigPreservable({ p2p_prefer_protocol: protocol }, [])).toThrow('p2p_prefer_protocol')
      expect(() => assertLegacyConfigPreservable({ p2p_prefer_protocol: protocol }, ['config:p2p_prefer_protocol'])).not.toThrow()
    }
    expect(() => assertLegacyConfigPreservable({ peer_urls: ['http3://saved:443'] }, [])).toThrow('peer_urls')
    expect(() => assertLegacyConfigPreservable({ peer_urls: ['http3://saved:443'] }, [HTTP3_CAPABILITY])).not.toThrow()
    expect(() => assertLegacyConfigPreservable({ close_redundant_conns_when_disguised: false, peer_urls: ['wss://saved:443'] }, [])).not.toThrow()
  })

  it('protects explicitly present false/empty raw overrides without treating legacy defaults as saved extensions', () => {
    const existing = { close_redundant_conns_when_disguised: false, sni: '' }
    expect(() => assertLegacyConfigPreservable(existing, [], true)).toThrow('close_redundant_conns_when_disguised')
    expect(() => assertLegacyConfigPreservable(existing, [])).not.toThrow()
    expect(() => assertLegacyConfigPreservable(existing, ['config:close_redundant_conns_when_disguised', 'config:sni'], true)).not.toThrow()
    expect(() => assertLegacyConfigPreservable({}, [], true)).not.toThrow()
  })
})
