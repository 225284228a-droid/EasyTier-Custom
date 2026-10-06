import type { GeoPermissibleObjects } from 'd3-geo'
import world from '../assets/world-countries.json'
import countryLabels from '../assets/world-country-labels.json'
import type { TopologyNode } from './networkTopology'

export const worldGeography = world as unknown as GeoPermissibleObjects
const countryNames = new Map<string, { latitude: number, longitude: number }>()
for (const reference of [...countryLabels, ...world.features.map(feature => feature.properties)]) {
  const { en, zh, iso } = reference
  countryNames.set(en, reference)
  countryNames.set(zh, reference)
  if (/^[A-Z]{2}$/.test(iso))
    countryNames.set(iso, reference)
  if (/^[A-Z]{2}$/.test(iso) && typeof Intl.DisplayNames === 'function') {
    for (const locale of ['en', 'zh']) {
      const name = new Intl.DisplayNames([locale], { type: 'region' }).of(iso)
      if (name)
        countryNames.set(name, reference)
    }
  }
}

export interface LocatedNode extends TopologyNode {
  latitude: number
  longitude: number
  approximate: boolean
}

export function locateNode(node: TopologyNode): LocatedNode | undefined {
  if (typeof node.latitude === 'number' && typeof node.longitude === 'number'
    && Number.isFinite(node.latitude) && Math.abs(node.latitude) <= 90
    && Number.isFinite(node.longitude) && Math.abs(node.longitude) <= 180) {
    return { ...node, latitude: node.latitude, longitude: node.longitude, approximate: false }
  }
  const country = node.country ? countryNames.get(node.country) : undefined
  if (!country)
    return undefined
  return {
    ...node,
    latitude: country.latitude,
    longitude: country.longitude,
    approximate: true,
  }
}
