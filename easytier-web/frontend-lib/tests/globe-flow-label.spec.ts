import { describe, expect, it } from 'vitest'
import { createFlowLabel } from '../../frontend/src/modules/globeFlowLabel'

describe('globe cross-shaped traffic labels', () => {
  it('places only directional throughput above and below the endpoint name row', () => {
    const label = createFlowLabel({
      sourceLabel: 'source-device',
      targetLabel: 'target-device',
      txBps: 20_000_000,
      rxBps: 3_000_000,
    })
    expect(label.querySelector('.globe-flow-source-stat')!.textContent).toBe('20.0 Mbit/s')
    expect(label.querySelector('.globe-flow-target-stat')!.textContent).toBe('3.00 Mbit/s')
    expect([...label.children].map(child => child.className)).toEqual([
      'globe-flow-stat globe-flow-source-stat',
      'traffic-endpoints',
      'globe-flow-stat globe-flow-target-stat',
    ])
    expect(label.querySelector('.traffic-endpoints .pi-arrow-right-arrow-left')).not.toBeNull()
    expect(label.getAttribute('aria-label')).toContain('source-device \u2192 target-device: 20.0 Mbit/s')
    expect(label.getAttribute('aria-label')).toContain('target-device \u2192 source-device: 3.00 Mbit/s')
    expect(label.textContent).not.toContain('RTT')
    expect(label.textContent).not.toContain(' ms')
  })

  it('keeps absent directional traffic unknown rather than displaying zero', () => {
    const label = createFlowLabel({ sourceLabel: 'a', targetLabel: 'b', txBps: 0 })
    expect(label.querySelector('.globe-flow-source-stat')!.textContent).toBe('0 bit/s')
    expect(label.querySelector('.globe-flow-target-stat')!.textContent).toBe('--')
  })

  it('retains full device names in tooltips and directional accessibility text', () => {
    const source = 'a-very-long-source-device-name'
    const target = 'a-very-long-target-device-name'
    const label = createFlowLabel({ sourceLabel: source, targetLabel: target })
    expect(label.querySelector('.globe-flow-source')!.getAttribute('title')).toBe(source)
    expect(label.querySelector('.globe-flow-target')!.getAttribute('title')).toBe(target)
    expect(label.getAttribute('aria-label')).toContain(`${source} \u2192 ${target}`)
  })
})
