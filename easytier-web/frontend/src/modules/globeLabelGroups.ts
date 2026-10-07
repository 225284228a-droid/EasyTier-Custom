import type { LocatedNode } from './globeGeography'

const normalize = (value?: string) => value?.normalize('NFKC').trim().replace(/\s+/g, ' ').toLowerCase() ?? ''

export function nodeLocationGroup(node: LocatedNode): string {
  const location = node.nodeLocation
  const city = normalize(location?.city)
  return city
    ? JSON.stringify(['city', normalize(node.country ?? location?.country), normalize(location?.region), city])
    : JSON.stringify(['position', node.latitude.toFixed(4), node.longitude.toFixed(4)])
}

export function linkLocationGroup(source: string, target: string): string {
  return JSON.stringify([source, target].sort())
}
