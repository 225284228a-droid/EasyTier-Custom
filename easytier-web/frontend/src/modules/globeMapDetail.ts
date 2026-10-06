import type { GeoPermissibleObjects } from 'd3-geo'
import mediumUrl from '../assets/world-boundaries-50m.json?url'
import detailedUrl from '../assets/world-boundaries-10m.json?url'

const detailUrls = [mediumUrl, detailedUrl]
const pending = new Map<number, Promise<GeoPermissibleObjects>>()

/** Keep the larger source files out of the initial globe JavaScript chunk. */
export function loadGlobeMapDetail(level: 1 | 2): Promise<GeoPermissibleObjects> {
  const existing = pending.get(level)
  if (existing)
    return existing
  const request = fetch(detailUrls[level - 1])
    .then(async response => {
      if (!response.ok)
        throw new Error(`Globe detail asset failed: HTTP ${response.status}`)
      const geography = await response.json()
      if (geography.type !== 'FeatureCollection' || !Array.isArray(geography.features))
        throw new Error('Globe detail asset is not a GeoJSON FeatureCollection')
      return geography as GeoPermissibleObjects
    })
    .catch(error => {
      pending.delete(level)
      throw error
    })
  pending.set(level, request)
  return request
}
