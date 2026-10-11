import { describe, expect, it } from 'vitest'
import { isValidHostIp } from '../src/modules/ip'

describe('host address validation', () => {
  it('accepts IPv4 addresses and rejects malformed ones', () => {
    expect(isValidHostIp('0.0.0.0')).toBe(true)
    expect(isValidHostIp('10.9.0.2')).toBe(true)
    expect(isValidHostIp('255.255.255.255')).toBe(true)
    expect(isValidHostIp(' 10.0.0.1 ')).toBe(true)
    expect(isValidHostIp('999.0.0.1')).toBe(false)
    expect(isValidHostIp('1.2.3')).toBe(false)
    expect(isValidHostIp('1.2.3.')).toBe(false)
    expect(isValidHostIp('1..2.3')).toBe(false)
    expect(isValidHostIp('01.2.3.4')).toBe(false)
    expect(isValidHostIp('host.example.com')).toBe(false)
  })
  it('accepts IPv6 addresses including the embedded IPv4 form', () => {
    expect(isValidHostIp('fe80::1')).toBe(true)
    expect(isValidHostIp('2001:db8::1')).toBe(true)
    expect(isValidHostIp('::')).toBe(true)
    expect(isValidHostIp('::1')).toBe(true)
    expect(isValidHostIp('1:2:3:4:5:6:7:8')).toBe(true)
    expect(isValidHostIp('::ffff:10.0.0.2')).toBe(true)
    expect(isValidHostIp('2001:db8::8:800:200c:417a')).toBe(true)
    expect(isValidHostIp('[fe80::1]')).toBe(false)
    expect(isValidHostIp('fe80::1:2:3:4:5:6:7')).toBe(false)
    expect(isValidHostIp('fe80::1:')).toBe(false)
    expect(isValidHostIp('12::1::2')).toBe(false)
  })
  it('treats empty input as not-yet-filled rather than invalid', () => {
    expect(isValidHostIp('')).toBe(false)
    expect(isValidHostIp(null)).toBe(false)
    expect(isValidHostIp(undefined)).toBe(false)
  })
})
