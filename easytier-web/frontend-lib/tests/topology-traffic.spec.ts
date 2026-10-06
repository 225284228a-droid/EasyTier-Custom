import { describe, expect, it } from 'vitest'
import { TrafficTracker, type TrafficLink, type TrafficObservation } from '../../frontend/src/modules/topologyTraffic'

const observation = (
  connId: string,
  txBytes: unknown,
  rxBytes: unknown,
  source = 'a',
  target = 'b',
): TrafficObservation => ({ source, target, connId, txBytes, rxBytes })

function sample(tracker: TrafficTracker, observations: TrafficObservation[], time: number, stale = false) {
  const link: TrafficLink = { source: 'a', target: 'b', stale: stale || undefined }
  tracker.update([link], observations, time)
  return link
}

describe('topology traffic sampling', () => {
  it('keeps the first sample unknown and reports zero for an established idle connection', () => {
    const tracker = new TrafficTracker()
    expect(sample(tracker, [observation('udp', 100, 200)], 1_000).txBps).toBeUndefined()
    expect(sample(tracker, [observation('udp', 100, 200)], 3_000)).toMatchObject({
      txBps: 0,
      rxBps: 0,
    })
  })

  it('prefers the canonical source report without adding the reverse report twice', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [
      observation('source-udp', 100, 200),
      observation('target-udp', 200, 100, 'b', 'a'),
    ], 1_000)
    expect(sample(tracker, [
      observation('target-udp', 900, 800, 'b', 'a'),
      observation('source-udp', 300, 300),
    ], 3_000)).toMatchObject({
      txBps: 800,
      rxBps: 400,
    })
  })

  it('maps a reverse-only report into the canonical directions', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [observation('reverse', 100, 200, 'b', 'a')], 1_000)
    expect(sample(tracker, [observation('reverse', 140, 220, 'b', 'a')], 2_000)).toMatchObject({
      txBps: 160,
      rxBps: 320,
    })
  })

  it('uses a reverse report only for a direction missing in the source report', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [
      observation('source', 100, undefined),
      observation('reverse', 200, 100, 'b', 'a'),
    ], 1_000)
    expect(sample(tracker, [
      observation('source', 150, undefined),
      observation('reverse', 300, 900, 'b', 'a'),
    ], 2_000)).toMatchObject({
      txBps: 400,
      rxBps: 800,
    })
  })

  it('aggregates separate channels and ignores duplicate reports of the same connection', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [observation('udp', 10, 20), observation('wss', 100, 200)], 1_000)
    expect(sample(tracker, [
      observation('udp', 20, 40),
      observation('wss', 130, 240),
      observation('udp', 20, 40),
    ], 2_000)).toMatchObject({
      txBps: 320,
      rxBps: 480,
    })
  })

  it('does not present a partial channel total as a complete rate', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [observation('udp', 10, 20)], 1_000)
    const result = sample(tracker, [
      observation('udp', 20, 40),
      observation('new-wss', 100, 200),
    ], 2_000)
    expect(result.txBps).toBeUndefined()
    expect(result.rxBps).toBeUndefined()
  })

  it('subtracts u64 strings before conversion so small differences stay accurate', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [observation('udp', '18446744073709551000', '9007199254740993000')], 1_000)
    expect(sample(tracker, [observation('udp', '18446744073709551010', '9007199254740993025')], 2_000))
      .toMatchObject({ txBps: 80, rxBps: 200 })
  })

  it.each([
    -1,
    0.5,
    Number.NaN,
    Number.POSITIVE_INFINITY,
    Number.MAX_SAFE_INTEGER + 1,
    '',
    ' ',
    '-1',
    '1.5',
    'NaN',
    '1e3',
    '18446744073709551616',
    null,
    undefined,
  ])('rejects invalid or unsafe counters %s without creating a baseline', (counter) => {
    const tracker = new TrafficTracker()
    sample(tracker, [observation('udp', counter, counter)], 1_000)
    const result = sample(tracker, [observation('udp', 100, 100)], 2_000)
    expect(result.txBps).toBeUndefined()
    expect(result.rxBps).toBeUndefined()
    expect(sample(tracker, [observation('udp', 110, 120)], 3_000)).toMatchObject({
      txBps: 80,
      rxBps: 160,
    })
  })

  it('drops both directions for a reset sample and resumes from the new baseline', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [observation('udp', 100, 100)], 1_000)
    const reset = sample(tracker, [observation('udp', 5, 200)], 2_000)
    expect(reset.txBps).toBeUndefined()
    expect(reset.rxBps).toBeUndefined()
    expect(sample(tracker, [observation('udp', 15, 220)], 3_000)).toMatchObject({
      txBps: 80,
      rxBps: 160,
    })
  })

  it('requires a new baseline when a connection ID is replaced or disappears', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [observation('old', 100, 100)], 1_000)
    expect(sample(tracker, [observation('new', 500, 500)], 2_000).txBps).toBeUndefined()
    sample(tracker, [], 3_000)
    expect(sample(tracker, [observation('new', 600, 600)], 4_000).txBps).toBeUndefined()
  })

  it('does not emit rates for missing connection IDs or non-forward sample times', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [observation('', 10, 20)], 1_000)
    expect(sample(tracker, [observation('', 20, 40)], 2_000).txBps).toBeUndefined()
    sample(tracker, [observation('udp', 100, 100)], 3_000)
    expect(sample(tracker, [observation('udp', 110, 120)], 3_000).txBps).toBeUndefined()
    expect(sample(tracker, [observation('udp', 120, 130)], 2_000).txBps).toBeUndefined()
    expect(sample(tracker, [observation('udp', 130, 140)], Number.NaN).txBps).toBeUndefined()
  })

  it('rejects invalid or non-forward observation timestamps without corrupting the last good baseline', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [{ ...observation('udp', 100, 200), sampleTime: 1_000 }], 1_000)
    for (const time of [Number.NaN, Number.POSITIVE_INFINITY, -1, 900, 1_000, 9_000]) {
      const invalid = sample(tracker, [{ ...observation('udp', 900, 900), sampleTime: time }], 2_000)
      expect(invalid.txBps).toBeUndefined()
      expect(invalid.rxBps).toBeUndefined()
    }
    expect(sample(tracker, [{ ...observation('udp', 120, 240), sampleTime: 3_000 }], 4_000))
      .toMatchObject({ txBps: 80, rxBps: 160 })
  })

  it('expires each cached direction from its actual source timestamp rather than the slow batch end', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [
      { ...observation('source', 100, undefined), sampleTime: 1_000 },
      { ...observation('reverse', 200, 100, 'b', 'a'), sampleTime: 10_000 },
    ], 15_000)
    expect(sample(tracker, [
      { ...observation('source', 200, undefined), sampleTime: 2_000 },
      { ...observation('reverse', 400, 900, 'b', 'a'), sampleTime: 20_000 },
    ], 30_000)).toMatchObject({ txBps: 800, rxBps: 160 })
    const stale = sample(tracker, [], 62_000, true)
    expect(stale.txBps).toBeUndefined()
    expect(stale.rxBps).toBe(160)
    expect(sample(tracker, [], 80_000, true).rxBps).toBeUndefined()
  })

  it('isolates identical connection IDs in different networks', () => {
    const tracker = new TrafficTracker()
    const links: TrafficLink[] = [
      { source: 'network:alpha:1', target: 'network:alpha:2' },
      { source: 'network:beta:1', target: 'network:beta:2' },
    ]
    tracker.update(links, [
      observation('udp', 10, 20, links[0].source, links[0].target),
      observation('udp', 100, 200, links[1].source, links[1].target),
    ], 1_000)
    tracker.update(links, [
      observation('udp', 20, 40, links[0].source, links[0].target),
      observation('udp', 140, 250, links[1].source, links[1].target),
    ], 2_000)
    expect(links[0]).toMatchObject({ txBps: 80, rxBps: 160 })
    expect(links[1]).toMatchObject({ txBps: 320, rxBps: 400 })
  })

  it('retains a stale rate for at most sixty seconds without resampling old counters', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [observation('udp', 100, 200)], 1_000)
    sample(tracker, [observation('udp', 150, 300)], 2_000)
    expect(sample(tracker, [], 5_000, true)).toMatchObject({ txBps: 400, rxBps: 800 })
    expect(sample(tracker, [], 61_999, true)).toMatchObject({ txBps: 400, rxBps: 800 })
    const expired = sample(tracker, [], 62_000, true)
    expect(expired.txBps).toBeUndefined()
    expect(expired.rxBps).toBeUndefined()
    expect(sample(tracker, [observation('udp', 150, 300)], 63_000).txBps).toBeUndefined()
    expect(sample(tracker, [observation('udp', 150, 300)], 64_000).txBps).toBe(0)
  })

  it('preserves both endpoint baselines through stale rounds and averages over the real gap', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [
      observation('source-udp', 100, 200),
      observation('target-udp', 200, 100, 'b', 'a'),
    ], 1_000)
    sample(tracker, [
      observation('source-udp', 200, 300),
      observation('target-udp', 300, 200, 'b', 'a'),
    ], 2_000)
    sample(tracker, [], 3_000, true)
    sample(tracker, [], 5_000, true)
    expect(sample(tracker, [
      observation('source-udp', 400, 700),
      observation('target-udp', 700, 400, 'b', 'a'),
    ], 6_000)).toMatchObject({
      txBps: 400,
      rxBps: 800,
    })
  })

  it('does not extend a stale baseline lifetime and requires a new sample after sixty seconds', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [observation('udp', 100, 200)], 1_000)
    sample(tracker, [observation('udp', 150, 300)], 2_000)
    sample(tracker, [], 30_000, true)
    sample(tracker, [], 61_999, true)
    const expired = sample(tracker, [observation('udp', 250, 500)], 62_000)
    expect(expired.txBps).toBeUndefined()
    expect(expired.rxBps).toBeUndefined()
    expect(sample(tracker, [observation('udp', 260, 520)], 63_000)).toMatchObject({
      txBps: 80,
      rxBps: 160,
    })
  })

  it('reads cached rates repeatedly without renewing their original expiry', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [observation('udp', 100, 200)], 1_000)
    sample(tracker, [observation('udp', 150, 300)], 2_000)
    const link: TrafficLink = { source: 'a', target: 'b', stale: true }
    tracker.applyCachedRates([link], 30_000)
    expect(link).toMatchObject({ txBps: 400, rxBps: 800 })
    tracker.applyCachedRates([link], 61_999)
    expect(link).toMatchObject({ txBps: 400, rxBps: 800 })
    tracker.applyCachedRates([link], 62_000)
    expect(link.txBps).toBeUndefined()
    expect(link.rxBps).toBeUndefined()
  })

  it('does not let display-only cached-rate reads change the next counter baseline', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [observation('udp', 100, 200)], 1_000)
    sample(tracker, [observation('udp', 150, 300)], 2_000)
    const view: TrafficLink = { source: 'a', target: 'b' }
    tracker.applyCachedRates([view], 2_500)
    tracker.applyCachedRates([view], 2_750)
    tracker.applyCachedRates([], 2_900)
    expect(sample(tracker, [observation('udp', 250, 500)], 3_000))
      .toMatchObject({ txBps: 800, rxBps: 1600 })
  })

  it('clears all baseline and rate history on a server switch', () => {
    const tracker = new TrafficTracker()
    sample(tracker, [observation('udp', 100, 200)], 1_000)
    sample(tracker, [observation('udp', 150, 300)], 2_000)
    tracker.clear()
    expect(sample(tracker, [], 3_000, true).txBps).toBeUndefined()
    expect(sample(tracker, [observation('udp', 200, 400)], 4_000).txBps).toBeUndefined()
  })
})
