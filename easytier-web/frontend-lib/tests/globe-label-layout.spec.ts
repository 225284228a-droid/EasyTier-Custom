import { describe, expect, it } from 'vitest'
import {
  arrangeGlobeLabels, MAX_NODE_LEADER_LENGTH, MAX_TRAFFIC_LEADER_LENGTH,
  type LayoutLabel, type LabelPlacement,
} from '../../frontend/src/modules/globeLabelLayout'

const label = (id: string, x = 300, y = 220): LayoutLabel => ({
  id, kind: 'traffic', priority: 1, width: 198, height: 56,
  anchors: [{ x, y, index: 0, tangent: { x: 1, y: 0 } }],
})

function verify(placements: LabelPlacement[], width = 800, height = 500) {
  placements.forEach((placement, index) => {
    expect(placement.x).toBeGreaterThanOrEqual(5)
    expect(placement.y).toBeGreaterThanOrEqual(5)
    expect(placement.x + placement.width).toBeLessThanOrEqual(width - 5)
    expect(placement.y + placement.height).toBeLessThanOrEqual(height - 34)
    expect(placement.leaderLength).toBeLessThanOrEqual(MAX_TRAFFIC_LEADER_LENGTH)
    for (const previous of placements.slice(0, index)) {
      const overlaps = placement.x < previous.x + previous.width + 4
        && placement.x + placement.width + 4 > previous.x
        && placement.y < previous.y + previous.height + 4
        && placement.y + placement.height + 4 > previous.y
      expect(overlaps).toBe(false)
    }
  })
}

describe('local globe label distribution', () => {
  it('uses a short connector to the link instead of a distant row', () => {
    const placed = arrangeGlobeLabels([label('a')], 800, 500)
    expect(placed).toHaveLength(1)
    expect(placed[0].leaderLength).toBeCloseTo(12)
    verify(placed)
  })

  it('keeps diagonal connectors short by attaching to a nearby corner', () => {
    const diagonal = label('a')
    diagonal.anchors[0].tangent = { x: 2, y: 1 }
    const placed = arrangeGlobeLabels([diagonal], 800, 500)
    expect(placed).toHaveLength(1)
    expect(placed[0].leaderLength).toBeLessThan(20)
    verify(placed)
  })

  it('uses both sides of an overlapping path rather than pushing boxes apart vertically', () => {
    const placed = arrangeGlobeLabels([label('a'), label('b'), label('c')], 800, 500)
    expect(placed).toHaveLength(2)
    expect(placed[0].y < 220).not.toBe(placed[1].y < 220)
    verify(placed)
  })

  it('tries other path anchors when the midpoint is full', () => {
    const a = label('a')
    const b = label('b')
    const c = label('c')
    c.anchors.push({ x: 580, y: 220, index: 1, tangent: { x: 1, y: 0 } })
    const placed = arrangeGlobeLabels([a, b, c], 800, 500)
    expect(placed).toHaveLength(3)
    expect(placed.find(placement => placement.id === 'c')?.choice.anchorIndex).toBe(1)
    verify(placed)
  })

  it('declutters crowded mobile paths without extending connector lengths', () => {
    const labels = Array.from({ length: 12 }, (_, index) => label(`link-${index}`, 160, 150))
    const placed = arrangeGlobeLabels(labels, 329, 300)
    expect(placed.length).toBeGreaterThan(0)
    expect(placed.length).toBeLessThan(labels.length)
    verify(placed, 329, 300)
  })

  it('preserves a valid previous attachment as the camera moves slightly', () => {
    const original = label('a')
    const first = arrangeGlobeLabels([original], 800, 500)[0]
    const moved = { ...original, previous: first.choice }
    moved.anchors = [{ ...original.anchors[0], x: 302, y: 222 }]
    const next = arrangeGlobeLabels([moved], 800, 500)[0]
    expect(next.choice).toEqual(first.choice)
    expect(next.x - first.x).toBeCloseTo(2)
    expect(next.y - first.y).toBeCloseTo(2)
  })

  it('gives selected labels precedence and keeps node names close to their markers', () => {
    const node: LayoutLabel = {
      id: 'node', kind: 'node', priority: 10, width: 90, height: 22,
      anchors: [{ x: 300, y: 220, index: 0 }],
    }
    const placed = arrangeGlobeLabels([label('link'), node], 800, 500,
      [{ x: 293, y: 213, width: 14, height: 14 }])
    expect(placed[0].id).toBe('node')
    expect(placed[0].leaderLength).toBeLessThanOrEqual(MAX_NODE_LEADER_LENGTH)
    verify(placed)
  })

  it('does not place boxes over markers or duplicate a link label', () => {
    const current = label('a')
    const reserved = { x: 190, y: 135, width: 220, height: 170 }
    expect(arrangeGlobeLabels([current], 800, 500, [reserved])).toEqual([])
    expect(arrangeGlobeLabels([current, current], 800, 500)).toHaveLength(1)
  })

  it('hides labels with no visible anchors or insufficient viewport space', () => {
    expect(arrangeGlobeLabels([{ ...label('a'), anchors: [] }], 800, 500)).toEqual([])
    expect(arrangeGlobeLabels([label('a')], 100, 50)).toEqual([])
    expect(arrangeGlobeLabels([label('a', -10, 200)], 800, 500)).toEqual([])
  })

  it('places an entire vertical stack beside its shared route with one short connector', () => {
    const stack = { ...label('stack'), stacked: true, height: 176, heights: [176, 116, 56] }
    const placed = arrangeGlobeLabels([stack], 800, 500)
    expect(placed).toHaveLength(1)
    expect(placed[0].height).toBe(176)
    expect(placed[0].leaderLength).toBeCloseTo(12)
    verify(placed)
  })

  it('shrinks a scrollable stack by whole rows when the full column would cover a marker', () => {
    const stack = { ...label('stack', 160, 150), stacked: true, height: 220, heights: [220, 148, 72] }
    const placed = arrangeGlobeLabels([stack], 329, 300,
      [{ x: 153, y: 143, width: 14, height: 14 }])
    expect(placed).toHaveLength(1)
    expect(placed[0].height).toBe(72)
    verify(placed, 329, 300)
  })

  it('keeps a city column and its overlapping route column visible on mobile', () => {
    const city: LayoutLabel = {
      id: 'city', kind: 'node', stacked: true, priority: 10,
      width: 80, height: 100, heights: [100, 74, 48, 22],
      anchors: [{ x: 160, y: 150, index: 0 }],
    }
    const routes = { ...label('routes', 160, 250), width: 206, stacked: true,
      height: 220, heights: [220, 148, 72] }
    const placed = arrangeGlobeLabels([routes, city], 329, 300,
      [{ x: 153, y: 143, width: 14, height: 14 }])
    expect(placed).toHaveLength(2)
    expect(placed.find(placement => placement.id === 'city')!.y).toBeLessThan(150)
    verify(placed, 329, 300)
  })
})
