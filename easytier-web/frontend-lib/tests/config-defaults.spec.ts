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

  it('round-trips configured DHCP subnets and leaves omitted defaults unset', () => {
    const defaults = DEFAULT_NETWORK_CONFIG()
    const backendDefaults = toBackendNetworkConfig(defaults)
    expect(backendDefaults).not.toHaveProperty('dhcp_ipv4_cidr')
    expect(backendDefaults).not.toHaveProperty('dhcpv6')
    expect(backendDefaults).not.toHaveProperty('dhcp_ipv6_cidr')

    const configured = {
      ...defaults,
      dhcp: true,
      dhcp_ipv4_cidr: '192.0.2.0/24',
      dhcpv6: true,
      dhcp_ipv6_cidr: 'fd12:3456::/64',
    }
    const normalized = normalizeNetworkConfig(configured)
    expect(normalized).toMatchObject({
      dhcp_ipv4_cidr: '192.0.2.0/24',
      dhcpv6: true,
      dhcp_ipv6_cidr: 'fd12:3456::/64',
    })
    expect(toBackendNetworkConfig(normalized)).toMatchObject({
      dhcp_ipv4_cidr: '192.0.2.0/24',
      dhcpv6: true,
      dhcp_ipv6_cidr: 'fd12:3456::/64',
    })

    const disabled = normalizeNetworkConfig({ ...configured, dhcpv6: false })
    expect(toBackendNetworkConfig(disabled)).toMatchObject({ dhcpv6: false, dhcp_ipv6_cidr: 'fd12:3456::/64' })

    const inferred = normalizeNetworkConfig({ ...defaults, dhcpv6: undefined, dhcp_ipv6_cidr: 'fd12:3456::/64' })
    expect(inferred.dhcpv6).toBeUndefined()
    expect(toBackendNetworkConfig(inferred)).not.toHaveProperty('dhcpv6')
    expect(toBackendNetworkConfig(inferred).dhcp_ipv6_cidr).toBe('fd12:3456::/64')
  })
})
