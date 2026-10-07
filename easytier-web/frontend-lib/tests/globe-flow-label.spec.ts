import { describe, expect, it } from 'vitest'
import { createFlowLabel, updateFlowLabel } from '../../frontend/src/modules/globeFlowLabel'

describe('globe cross-shaped traffic labels', () => {
  it('keeps directional throughput around the endpoint row with RTT in a separate footer', () => {
    const label = createFlowLabel({
      sourceLabel: 'source-device',
      targetLabel: 'target-device',
      txBps: 20_000_000,
      rxBps: 3_000_000,
      latencyMs: 5,
    })
    expect(label.querySelector('.globe-flow-source-stat')!.textContent).toBe('20.0 Mbit/s')
    expect(label.querySelector('.globe-flow-target-stat')!.textContent).toBe('3.00 Mbit/s')
    expect([...label.children].map(child => child.className)).toEqual([
      'globe-flow-stat globe-flow-source-stat',
      'traffic-endpoints',
      'globe-flow-stat globe-flow-target-stat',
      'globe-flow-latency',
    ])
    expect(label.querySelector('.traffic-endpoints .pi-arrow-right-arrow-left')).not.toBeNull()
    expect(label.getAttribute('aria-label')).toContain('source-device \u2192 target-device: 20.0 Mbit/s')
    expect(label.getAttribute('aria-label')).toContain('target-device \u2192 source-device: 3.00 Mbit/s')
    expect(label.querySelector('.globe-flow-latency-value')!.textContent).toBe('RTT 5.00 ms')
    expect(label.getAttribute('aria-label')).toContain('RTT 5.00 ms')
  })

  it('keeps absent directional traffic unknown rather than displaying zero', () => {
    const label = createFlowLabel({ sourceLabel: 'a', targetLabel: 'b', txBps: 0 })
    expect(label.querySelector('.globe-flow-source-stat')!.textContent).toBe('0 bit/s')
    expect(label.querySelector('.globe-flow-target-stat')!.textContent).toBe('--')
    expect(label.querySelector('.globe-flow-latency-value')!.textContent).toBe('RTT --')
  })

  it('retains full device names in tooltips and directional accessibility text', () => {
    const source = 'a-very-long-source-device-name'
    const target = 'a-very-long-target-device-name'
    const label = createFlowLabel({ sourceLabel: source, targetLabel: target })
    expect(label.querySelector('.globe-flow-source')!.getAttribute('title')).toBe(source)
    expect(label.querySelector('.globe-flow-target')!.getAttribute('title')).toBe(target)
    expect(label.getAttribute('aria-label')).toContain(`${source} \u2192 ${target}`)
  })

  it('updates measurements and names in place without replacing the label content', () => {
    const label = createFlowLabel({
      sourceLabel: 'old-source', targetLabel: 'target', txBps: 20_000_000, latencyMs: 5,
    })
    const children = [...label.querySelectorAll('*')]
    updateFlowLabel(label, {
      sourceLabel: 'new-source', targetLabel: 'target', txBps: 2_000_000, rxBps: 300_000, latencyMs: 50,
    })
    expect([...label.querySelectorAll('*')].every((child, index) => child === children[index])).toBe(true)
    expect(label.querySelector('.globe-flow-source')!.textContent).toBe('new-source')
    expect(label.querySelector('.globe-flow-source')!.getAttribute('title')).toBe('new-source')
    expect(label.querySelector('.globe-flow-source-stat')!.textContent).toBe('2.00 Mbit/s')
    expect(label.querySelector('.globe-flow-target-stat')!.textContent).toBe('300 kbit/s')
    expect(label.querySelector('.globe-flow-latency-value')!.textContent).toBe('RTT 50.0 ms')
    expect(label.getAttribute('aria-label')).toContain('new-source \u2192 target: 2.00 Mbit/s')
    expect(label.getAttribute('aria-label')).not.toContain('old-source')
  })

  it.each([undefined, NaN, Infinity, -1, 0])('leaves invalid RTT %s unknown', latencyMs => {
    const label = createFlowLabel({ sourceLabel: 'a', targetLabel: 'b', latencyMs })
    expect(label.querySelector('.globe-flow-latency-value')!.textContent).toBe('RTT --')
  })
})
