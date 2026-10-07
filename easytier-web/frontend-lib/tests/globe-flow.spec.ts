import { describe, expect, it } from 'vitest'
import {
  FlowEmitter, flowEmissionsPerSecond, flowTravelSeconds,
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
  it('keeps two megabits below two dots per second instead of saturating the display', () => {
    expect(flowEmissionsPerSecond(2_000_000)).toBeGreaterThan(1)
    expect(flowEmissionsPerSecond(2_000_000)).toBeLessThan(2)
    expect(flowEmissionsPerSecond(20_000_000)).toBeGreaterThan(3)
    expect(flowEmissionsPerSecond(20_000_000)).toBeLessThan(4)
    expect(flowEmissionsPerSecond(100_000_000)).toBeLessThan(5)
  })

  it('uses a monotonic soft curve without making busy links a continuous stripe', () => {
    const rates = [1_000, 200_000, 2_000_000, 20_000_000, 100_000_000, 1_000_000_000]
    const emissions = rates.map(rate => flowEmissionsPerSecond(rate))
    emissions.forEach((value, index) => {
      expect(value).toBeLessThan(MAX_FLOW_EMISSIONS_PER_SECOND)
      if (index)
        expect(value).toBeGreaterThan(emissions[index - 1])
    })
  })

  it('does not force sparse traffic to emit at least one point per second', () => {
    expect(flowEmissionsPerSecond(1_000)).toBeGreaterThan(0)
    expect(flowEmissionsPerSecond(1_000)).toBeLessThan(0.1)
    expect(flowEmissionsPerSecond(200_000)).toBeLessThan(1)
  })

  it.each([undefined, 0, -1, Number.NaN, Number.POSITIVE_INFINITY])(
    'does not emit for an invalid or absent throughput %s', rate => {
      expect(flowEmissionsPerSecond(rate)).toBe(0)
    },
  )

  it('suppresses stale traffic and keeps very large rates finite', () => {
    expect(flowEmissionsPerSecond(20_000_000, true)).toBe(0)
    expect(flowEmissionsPerSecond(Number.MAX_VALUE)).toBe(MAX_FLOW_EMISSIONS_PER_SECOND)
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
    const frequency = flowEmissionsPerSecond(2_000_000)
    run(short, 10, frequency, flowTravelSeconds(5))
    run(long, 10, frequency, flowTravelSeconds(100))
    expect(short.emittedCount).toBe(Math.floor(10 * frequency))
    expect(long.emittedCount).toBe(short.emittedCount)
    // Faster journeys reduce in-flight occupancy, never increase emission rate.
    expect(short.progress.length).toBeLessThan(long.progress.length)
  })

  it('increases emissions when throughput rises without changing RTT', () => {
    const low = new FlowEmitter()
    const high = new FlowEmitter()
    run(low, 10, flowEmissionsPerSecond(2_000_000), 1)
    run(high, 10, flowEmissionsPerSecond(20_000_000), 1)
    expect(high.emittedCount).toBeGreaterThan(low.emittedCount)
    expect(high.emittedCount).toBeLessThan(low.emittedCount * 10)
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
    run(fastFrames, 4, 4, 1, 60)
    run(slowFrames, 4, 4, 1, 30)
    expect(fastFrames.emittedCount).toBe(slowFrames.emittedCount)
    expect(fastFrames.progress.length).toBe(slowFrames.progress.length)
    fastFrames.progress.forEach((progress, index) =>
      expect(progress).toBeCloseTo(slowFrames.progress[index], 8))
  })

  it('keeps fractional emission credit through rate changes and display rebuilds', () => {
    const emitter = new FlowEmitter()
    emitter.advance(0.1, 4, 1)
    expect(emitter.emittedCount).toBe(0)
    emitter.advance(0.1, 6, 1)
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
    emitter.advance(3_600, MAX_FLOW_EMISSIONS_PER_SECOND, 0.2)
    expect(emitter.emittedCount).toBe(21_600)
    expect(emitter.progress.length).toBeLessThanOrEqual(2)
    expect(emitter.progress.every(progress => progress >= 0 && progress < 1)).toBe(true)
  })

  it('bounds slow high-throughput directions with the reduced visual ceiling', () => {
    const emitter = new FlowEmitter()
    run(emitter, 10, MAX_FLOW_EMISSIONS_PER_SECOND, 6)
    expect(emitter.progress.length).toBeLessThanOrEqual(MAX_FLOW_PARTICLES)
    expect(emitter.emittedCount).toBe(60)
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
