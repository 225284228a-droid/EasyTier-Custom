import { describe, expect, it } from 'vitest'
import { assertPatchCapabilities, CONFIG_FIELD_CAPABILITIES, filterConfigPayload } from '../src/modules/capabilities'
import { buildLocalConfigPatch, type LocalConfigEntry } from '../src/modules/localConfigPatch'
import { p2pDisguiseMode, setP2pDisguiseMode, type P2pDisguiseMode } from '../src/modules/p2pDisguise'
import { DEFAULT_NETWORK_CONFIG } from '../src/types/network'

const MODE_CAPABILITY = CONFIG_FIELD_CAPABILITIES.p2p_disguise_mode

describe('automatic P2P disguise policy', () => {
  it('reads a missing setting as the default and normalizes spelling', () => {
    expect(p2pDisguiseMode({})).toBe('prefer')
    expect(p2pDisguiseMode({ p2p_disguise_mode: '' })).toBe('prefer')
    expect(p2pDisguiseMode({ p2p_disguise_mode: ' ONLY ' })).toBe('only')
    expect(filterConfigPayload({ p2p_disguise_mode: ' ONLY ' }, [MODE_CAPABILITY]).p2p_disguise_mode).toBe('only')
  })

  it.each<P2pDisguiseMode>(['prefer', 'off', 'disabled', 'only'])('writes one field: %s', (mode) => {
    const config = DEFAULT_NETWORK_CONFIG()
    setP2pDisguiseMode(config, mode)
    expect(config.p2p_disguise_mode).toBe(mode)
    expect(filterConfigPayload(config, [MODE_CAPABILITY]).p2p_disguise_mode).toBe(mode)
    const patch = buildLocalConfigPatch({ inst_id: config.instance_id, revision: 'rev' } as LocalConfigEntry,
      config, ['p2p_disguise_mode'], [MODE_CAPABILITY])
    expect(patch.field_mask).toEqual(['p2p_disguise_mode'])
    expect(patch.config.p2p_disguise_mode).toBe(mode)
  })

  it('never sends the policy to kernels without the capability or permits unsupported patches', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    setP2pDisguiseMode(config, 'only')
    expect(filterConfigPayload(config, [])).not.toHaveProperty('p2p_disguise_mode')
    expect(() => assertPatchCapabilities(config, ['p2p_disguise_mode'], [])).toThrow('Unsupported configuration field')
    expect(() => assertPatchCapabilities(config, ['p2p_disguise_mode'], [MODE_CAPABILITY])).not.toThrow()
  })
})
