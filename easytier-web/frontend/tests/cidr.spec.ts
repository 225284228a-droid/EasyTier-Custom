import { describe, expect, it } from 'vitest'
import { toNetworkCidr } from '../src/modules/cidr'

describe('strict CIDR input parsing', () => {
  it('rejects a missing or malformed prefix instead of coercing it to zero', () => {
    expect(toNetworkCidr('192.168.1.4/')).toBeNull()
    expect(toNetworkCidr('192.168.1.4//24')).toBeNull()
    expect(toNetworkCidr('192.168.1.4/24/8')).toBeNull()
    expect(toNetworkCidr('192.168.1.4/abc')).toBeNull()
    expect(toNetworkCidr('192.168.1.4/-1')).toBeNull()
    expect(toNetworkCidr('192.168.1.4/33')).toBeNull()
    expect(toNetworkCidr('192.168.1.4/024')).toBeNull()
  })
  it('rejects empty fields, stray dots and non-IPv4 addresses', () => {
    expect(toNetworkCidr('1.2.3.')).toBeNull()
    expect(toNetworkCidr('.1.2.3')).toBeNull()
    expect(toNetworkCidr('1..2.3')).toBeNull()
    expect(toNetworkCidr('1.2.3.4.')).toBeNull()
    expect(toNetworkCidr('1.2.3')).toBeNull()
    expect(toNetworkCidr('1.2.3.4.5')).toBeNull()
    expect(toNetworkCidr('1.2.3.256')).toBeNull()
    expect(toNetworkCidr('fe80::1')).toBeNull()
    expect(toNetworkCidr('')).toBeNull()
    expect(toNetworkCidr('   ')).toBeNull()
  })
  it('clears the host bits of an explicit prefix', () => {
    expect(toNetworkCidr('192.168.1.4/24')).toBe('192.168.1.0/24')
    expect(toNetworkCidr('10.1.2.3/16')).toBe('10.1.0.0/16')
    expect(toNetworkCidr('192.168.1.4/32')).toBe('192.168.1.4/32')
    expect(toNetworkCidr('0.0.0.0/0')).toBe('0.0.0.0/0')
    expect(toNetworkCidr(' 192.168.1.4/24 ')).toBe('192.168.1.0/24')
  })
  it('expands a bare host address to its /24 network', () => {
    expect(toNetworkCidr('192.168.1.4')).toBe('192.168.1.0/24')
    expect(toNetworkCidr('0.0.0.0')).toBe('0.0.0.0/24')
  })
})
