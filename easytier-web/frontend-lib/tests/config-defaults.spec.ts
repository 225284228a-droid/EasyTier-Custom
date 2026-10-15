import { describe, expect, it } from 'vitest'
import { PRIVATE_MODE_DEFAULT_CAPABILITY } from '../src/modules/capabilities'
import { DEFAULT_NETWORK_CONFIG, defaultFlagValue, normalizeNetworkConfig, toBackendNetworkConfig } from '../src/types/network'

describe('kernel-specific configuration defaults', () => {
  it('leaves private mode implicit and displays the target kernel default', () => {
    const config = DEFAULT_NETWORK_CONFIG()
    expect(config.enable_private_mode).toBeUndefined()
    expect(defaultFlagValue('enable_private_mode')).toBe(true)
    expect(defaultFlagValue('enable_private_mode', [PRIVATE_MODE_DEFAULT_CAPABILITY])).toBe(true)
    expect(defaultFlagValue('enable_private_mode', [])).toBe(false)
    for (const capabilities of [[], [PRIVATE_MODE_DEFAULT_CAPABILITY]]) {
      const saved = toBackendNetworkConfig(config, capabilities)
      expect(saved).not.toHaveProperty('enable_private_mode')
      expect(normalizeNetworkConfig(saved).enable_private_mode).toBeUndefined()
    }
  })

  it.each([false, true])('preserves the user private mode choice %s for custom and official kernels', value => {
    const config = { ...DEFAULT_NETWORK_CONFIG(), enable_private_mode: value }
    for (const capabilities of [[], [PRIVATE_MODE_DEFAULT_CAPABILITY]]) {
      expect(toBackendNetworkConfig(config, capabilities).enable_private_mode).toBe(value)
    }
  })
})
