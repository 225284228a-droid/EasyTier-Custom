import { describe, expect, it } from 'vitest'
import { TopologyAvailability } from '../src/modules/topologyAvailability'

const device = (id: string, running = 1) => ({
  machine_id: id,
  running_network_count: running,
}) as any

describe('delayed topology availability warnings', () => {
  it('warns only after one minute without a successful report', () => {
    const tracker = new TopologyAvailability(0)
    tracker.updateDevices([device('a'), device('b')], 1_000)
    tracker.report('a', 10_000)
    tracker.report('b', 50_000)
    expect(tracker.incomplete(69_999)).toBe(false)
    expect(tracker.incomplete(70_000)).toBe(true)
    tracker.report('a', 71_000)
    expect(tracker.incomplete(71_000)).toBe(false)
  })

  it('successful machine listings never renew a missing topology report', () => {
    const tracker = new TopologyAvailability(0)
    tracker.updateDevices([device('a')], 0)
    tracker.updateDevices([device('a')], 59_000)
    expect(tracker.incomplete(60_000)).toBe(true)
  })

  it('uses the initial observation time for a node that has never reported', () => {
    const tracker = new TopologyAvailability(0)
    tracker.updateDevices([device('a')], 1_000)
    expect(tracker.incomplete(60_999)).toBe(false)
    expect(tracker.nextDeadline(60_999)).toBe(61_000)
    expect(tracker.incomplete(61_000)).toBe(true)
    expect(tracker.nextDeadline(61_000)).toBeUndefined()
  })

  it('does not warn for machines or instances no longer running', () => {
    const tracker = new TopologyAvailability(0)
    tracker.updateDevices([device('a'), device('b')], 0)
    tracker.updateDevices([device('b', 0)], 61_000)
    expect(tracker.incomplete(61_000)).toBe(false)
    expect(tracker.nextDeadline(61_000)).toBeUndefined()
  })

  it('delays listing errors and clears them immediately on recovery', () => {
    const tracker = new TopologyAvailability(0)
    tracker.updateDevices([], 10_000)
    tracker.failListing()
    expect(tracker.listUnavailable(69_999)).toBe(false)
    expect(tracker.nextDeadline(69_999)).toBe(70_000)
    expect(tracker.listUnavailable(70_000)).toBe(true)
    tracker.updateDevices([], 71_000)
    expect(tracker.listUnavailable(71_000)).toBe(false)
  })

  it('clears all deadlines on a server switch', () => {
    const tracker = new TopologyAvailability(0)
    tracker.updateDevices([device('a')], 0)
    tracker.failListing()
    tracker.clear(60_000)
    expect(tracker.incomplete(120_000)).toBe(false)
    expect(tracker.listUnavailable(120_000)).toBe(false)
    expect(tracker.nextDeadline(120_000)).toBeUndefined()
  })
})
