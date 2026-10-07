import { describe, expect, it } from 'vitest'
import {
  FlowEmitter, flowBitsPerParticle, flowEmissionsPerSecond, flowTravelSeconds,
  MAX_FLOW_EMISSIONS_PER_SECOND, MAX_FLOW_PARTICLES,
} from '../../frontend/src/modules/globeFlow'

describe('globe RTT travel speed', () => {
  it('preserves latency ratios instead of compressing low-latency differences', () => {
    expect(flowTravelSeconds(10)).toBe(0.2)
    expect(flowTravelSeconds(20)).toBe(0.4)
    expect(flowTravelSeconds(50)).toBe(1)
    expect(flowTravelSeconds(100) / flowTravelSeconds(10)).toBe(10)
  })

  it('moves lower-latency links faster inside the visual limits', () => {
    const periods = [5, 10, 100, 250].map(flowTravelSeconds)
    for (let index = 1; index < periods.length; index++)
      expect(periods[index]).toBeGreaterThan(periods[index - 1])
  })

  it('clamps extremely fast and slow RTTs to readable visual travel times', () => {
    expect(flowTravelSeconds(0.0001)).toBe(0.08)
    expect(flowTravelSeconds(1_000_000)).toBe(6)
  })

  it.each([undefined, 0, -1, Number.NaN, Number.POSITIVE_INFINITY])('uses a conservative fallback for missing RTT %s', latency => {
    expect(flowTravelSeconds(latency)).toBe(5)
  })
})

describe('throughput-based globe emissions', () => {
  it('uses a common payload unit and preserves rate ratios across links', () => {
    const unit = flowBitsPerParticle([20_000_000, 10_000_000, 200_000])
    expect(flowEmissionsPerSecond(20_000_000, unit)).toBe(MAX_FLOW_EMISSIONS_PER_SECOND)
    expect(flowEmissionsPerSecond(10_000_000, unit)).toBe(12)
    expect(flowEmissionsPerSecond(200_000, unit)).toBeCloseTo(0.24)
  })

  it('does not force sparse traffic to emit at least one point per second', () => {
    const unit = flowBitsPerParticle([1_000])
    expect(unit).toBe(64_000)
    expect(flowEmissionsPerSecond(1_000, unit)).toBe(1 / 64)
  })

  it.each([undefined, 0, -1, Number.NaN, Number.POSITIVE_INFINITY])(
    'does not emit for an invalid or absent throughput %s', rate => {
      expect(flowEmissionsPerSecond(rate, flowBitsPerParticle([rate]))).toBe(0)
    },
  )

  it('suppresses stale traffic and rejects invalid payload units', () => {
    expect(flowEmissionsPerSecond(20_000_000, 64_000, true)).toBe(0)
    for (const unit of [0, -1, Number.NaN, Number.POSITIVE_INFINITY])
      expect(flowEmissionsPerSecond(100_000, unit)).toBe(0)
    expect(flowBitsPerParticle([undefined, 0, -1, Number.NaN, Number.POSITIVE_INFINITY])).toBe(64_000)
  })
})

function run(emitter: FlowEmitter, seconds: number, rate: number, duration: number, frames = 60) {
  for (let index = 0; index < seconds * frames; index++)
    emitter.advance(1 / frames, rate, duration)
}

describe('one-shot globe particle scheduler', () => {
  it('emits the same count for equal throughput despite different RTTs', () => {
    const short = new FlowEmitter()
    const long = new FlowEmitter()
    run(short, 10, 10, flowTravelSeconds(5))
    run(long, 10, 10, flowTravelSeconds(100))
    expect(short.emittedCount).toBe(100)
    expect(long.emittedCount).toBe(100)
    // Faster journeys reduce in-flight occupancy, never increase emission rate.
    expect(short.progress.length).toBeLessThan(long.progress.length)
  })

  it('doubles emission count when throughput doubles without changing RTT', () => {
    const unit = flowBitsPerParticle([10_000_000, 20_000_000])
    const low = new FlowEmitter()
    const high = new FlowEmitter()
    run(low, 10, flowEmissionsPerSecond(10_000_000, unit), 1)
    run(high, 10, flowEmissionsPerSecond(20_000_000, unit), 1)
    expect(high.emittedCount).toBe(low.emittedCount * 2)
  })

  it('retires arriving points instead of wrapping them into fake new emissions', () => {
    const emitter = new FlowEmitter()
    emitter.advance(1, 1, 0.1)
    expect(emitter.progress).toEqual([0])
    emitter.advance(0.2, 1, 0.1)
    expect(emitter.progress).toEqual([])
    expect(emitter.emittedCount).toBe(1)
  })

  it('accounts for birth time within each frame and is independent of frame rate', () => {
    const fastFrames = new FlowEmitter()
    const slowFrames = new FlowEmitter()
    run(fastFrames, 4, 10, 1, 60)
    run(slowFrames, 4, 10, 1, 30)
    expect(fastFrames.emittedCount).toBe(slowFrames.emittedCount)
    expect(fastFrames.progress.length).toBe(slowFrames.progress.length)
    fastFrames.progress.forEach((progress, index) =>
      expect(progress).toBeCloseTo(slowFrames.progress[index], 8))
  })

  it('keeps fractional emission credit through rate changes and display rebuilds', () => {
    const emitter = new FlowEmitter()
    emitter.advance(0.1, 5, 1)
    expect(emitter.emittedCount).toBe(0)
    emitter.advance(0.05, 10, 1)
    expect(emitter.emittedCount).toBe(1)
    expect(emitter.progress[0]).toBeCloseTo(0)
  })

  it('changes RTT without resetting or teleporting existing points', () => {
    const emitter = new FlowEmitter()
    emitter.advance(1.2, 1, 1)
    expect(emitter.progress[0]).toBeCloseTo(0.2)
    emitter.advance(0.1, 1, 2)
    expect(emitter.progress[0]).toBeCloseTo(0.25)
    expect(emitter.emittedCount).toBe(1)
  })

  it('handles suspended-tab gaps without a burst of already-arrived points', () => {
    const emitter = new FlowEmitter()
    emitter.advance(3_600, 24, 0.2)
    expect(emitter.emittedCount).toBe(86_400)
    expect(emitter.progress.length).toBeLessThanOrEqual(5)
    expect(emitter.progress.every(progress => progress >= 0 && progress < 1)).toBe(true)
  })

  it('bounds slow high-throughput directions without sacrificing the common emission scale', () => {
    const emitter = new FlowEmitter()
    run(emitter, 10, MAX_FLOW_EMISSIONS_PER_SECOND, 6)
    expect(emitter.progress.length).toBeLessThanOrEqual(MAX_FLOW_PARTICLES)
    expect(emitter.emittedCount).toBe(240)
  })

  it('clears idle or stale directions and ignores non-forward frame times', () => {
    const emitter = new FlowEmitter()
    emitter.advance(1, 10, 1)
    const before = [...emitter.progress]
    for (const seconds of [0, -1, Number.NaN, Number.POSITIVE_INFINITY])
      emitter.advance(seconds, 10, 1)
    expect(emitter.progress).toEqual(before)
    emitter.advance(0.1, 0, 1)
    expect(emitter.progress).toEqual([])
  })
})
