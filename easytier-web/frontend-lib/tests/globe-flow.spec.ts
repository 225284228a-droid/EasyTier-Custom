import { describe, expect, it } from 'vitest'
import { flowTravelSeconds } from '../../frontend/src/modules/globeFlow'

describe('globe RTT travel speed', () => {
  it('moves lower-latency links faster without changing traffic density', () => {
    const periods = [1, 10, 100, 1_000].map(flowTravelSeconds)
    for (let index = 1; index < periods.length; index++)
      expect(periods[index]).toBeGreaterThan(periods[index - 1])
  })

  it('clamps extremely fast and slow RTTs to readable visual travel times', () => {
    expect(flowTravelSeconds(0.0001)).toBe(0.35)
    expect(flowTravelSeconds(1_000_000)).toBe(8)
  })

  it.each([undefined, 0, -1, Number.NaN, Number.POSITIVE_INFINITY])('uses a conservative fallback for missing RTT %s', latency => {
    expect(flowTravelSeconds(latency)).toBe(5)
  })
})
