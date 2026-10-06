import type { GeoPermissibleObjects } from 'd3-geo'
import world from '../assets/world-countries.json'
import type { TopologyNode } from './networkTopology'

export const worldGeography = world as unknown as GeoPermissibleObjects
const countryNames = new Map<string, (typeof world.features)[number]>()
for (const feature of world.features) {
  const { en, zh, iso } = feature.properties
  countryNames.set(en, feature)
  countryNames.set(zh, feature)
  if (/^[A-Z]{2}$/.test(iso) && typeof Intl.DisplayNames === 'function') {
    for (const locale of ['en', 'zh']) {
      const name = new Intl.DisplayNames([locale], { type: 'region' }).of(iso)
      if (name)
        countryNames.set(name, feature)
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
    latitude: country.properties.latitude,
    longitude: country.properties.longitude,
    approximate: true,
  }
}
