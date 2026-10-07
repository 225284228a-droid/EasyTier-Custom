import { describe, expect, it } from 'vitest'
import { linkLocationGroup, nodeLocationGroup } from '../../frontend/src/modules/globeLabelGroups'
import type { LocatedNode } from '../../frontend/src/modules/globeGeography'

const node = (id: string, city = 'Shanghai'): LocatedNode => ({
  id, peerId: 1, label: id, networkIdentity: 'mesh', managed: true, approximate: false,
  latitude: 31.2, longitude: 121.5, country: 'China',
  nodeLocation: { country: 'China', region: 'Shanghai', city },
})

describe('globe city and geographic route groups', () => {
  it('groups distinct devices in the same city even if their coordinates differ', () => {
    const a = node('device-a')
    const b = { ...node('device-b'), latitude: 31.25, longitude: 121.55 }
    expect(nodeLocationGroup(a)).toBe(nodeLocationGroup(b))
    expect(a.id).not.toBe(b.id)
  })

  it('normalizes city case and whitespace without depending on the device name', () => {
    expect(nodeLocationGroup(node('a', '  SHANGHAI  '))).toBe(nodeLocationGroup(node('b')))
  })

  it('keeps equal city names in different countries or regions separate', () => {
    const a = node('a', 'Cambridge')
    const b = { ...node('b', 'Cambridge'), country: 'United Kingdom' }
    const c = node('c', 'Cambridge')
    c.nodeLocation = { ...c.nodeLocation, region: 'different-region' }
    expect(nodeLocationGroup(a)).not.toBe(nodeLocationGroup(b))
    expect(nodeLocationGroup(a)).not.toBe(nodeLocationGroup(c))
  })

  it('groups coincident country-only positions, not every unmapped city in a country', () => {
    const a = { ...node('a'), nodeLocation: undefined, approximate: true }
    expect(nodeLocationGroup(a)).toBe(nodeLocationGroup({ ...a, id: 'b' }))
    expect(nodeLocationGroup(a)).not.toBe(nodeLocationGroup({ ...a, id: 'c', latitude: 32 }))
  })

  it('groups both orientations of a geographic route without merging device identities', () => {
    const a = nodeLocationGroup(node('a'))
    const b = nodeLocationGroup({ ...node('b', 'Auckland'), country: 'New Zealand' })
    expect(linkLocationGroup(a, b)).toBe(linkLocationGroup(b, a))
    expect(linkLocationGroup(a, b)).not.toBe(linkLocationGroup(a, a))
  })
})
