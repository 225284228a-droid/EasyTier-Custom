import { readFileSync } from 'node:fs'
import { URL as NodeURL } from 'node:url'
import { describe, expect, it } from 'vitest'
import { spherePosition, sphericalArc } from '../src/modules/globeBoundaryGeometry'
import { appendSphericalArcSegments } from '../src/modules/globeGeographyLayer'

function sourceVertices(filename: string) {
  const data = JSON.parse(readFileSync(new NodeURL(`../src/assets/${filename}`, import.meta.url), 'utf8'))
  const count = (coordinates: unknown): number => {
    if (!Array.isArray(coordinates))
      return 0
    if (typeof coordinates[0] === 'number')
      return 1
    return coordinates.reduce((total, child) => total + count(child), 0)
  }
  return data.features.reduce((total: number, feature: { geometry: { coordinates: unknown } }) =>
    total + count(feature.geometry.coordinates), 0)
}

describe('globe boundary detail', () => {
  it('retains genuine higher-resolution source vertices in the zoom detail assets', () => {
    const low = sourceVertices('world-countries.json')
    const medium = sourceVertices('world-boundaries-50m.json')
    const high = sourceVertices('world-boundaries-10m.json')
    expect(medium).toBeGreaterThan(low * 5)
    expect(high).toBeGreaterThan(medium * 4)
  })

  it('retains tiny source edges instead of dropping high-resolution coast detail', () => {
    const from = spherePosition(0, 90)
    const to = spherePosition(0.0005, 90)
    const points = sphericalArc(from, to, 1.003, 0.003)
    expect(points).toHaveLength(2)
    expect(points[0].distanceTo(from.clone().multiplyScalar(1.003))).toBeLessThan(1e-10)
    expect(points[1].distanceTo(to.clone().multiplyScalar(1.003))).toBeLessThan(1e-10)
  })

  it('uses the correct axis for small non-equatorial and near-polar edges', () => {
    const from = spherePosition(89.9, 30)
    const to = spherePosition(89.91, 32)
    const points = sphericalArc(from, to, 1.003, 0.003)
    expect(points.at(-1)!.distanceTo(to.clone().multiplyScalar(1.003))).toBeLessThan(1e-9)
  })

  it('adds spherical subdivisions only when the angular step requires them', () => {
    const from = spherePosition(0, 90)
    const to = spherePosition(90, 0)
    const far = sphericalArc(from, to, 1.003, 0.045)
    const close = sphericalArc(from, to, 1.003, 0.003)
    expect(close.length).toBeGreaterThan(far.length)
    for (const point of close)
      expect(point.length()).toBeCloseTo(1.003, 12)
    expect(close.at(-1)!.distanceTo(to.clone().multiplyScalar(1.003))).toBeLessThan(1e-10)
  })

  it.each([
    [[0, 90], [0.0005, 90], 0.003],
    [[89.9, 30], [89.91, 32], 0.003],
    [[0, 90], [90, 0], 0.045],
    [[-33.9, 151.2], [51.5, -0.1], 0.012],
  ])('subdivides %j to %j like the vector arc', (start, end, step) => {
    const from = spherePosition(start[0], start[1])
    const to = spherePosition(end[0], end[1])
    const expected = sphericalArc(from, to, 1.003, step)
    const segments: number[] = []
    appendSphericalArcSegments(segments, [from.x, from.y, from.z], [to.x, to.y, to.z], 1.003, step)
    expect(segments).toHaveLength((expected.length - 1) * 6)
    for (let index = 0; index < expected.length - 1; index++) {
      const [a, b] = [expected[index], expected[index + 1]]
      const actual = segments.slice(index * 6, index * 6 + 6)
      ;[a.x, a.y, a.z, b.x, b.y, b.z].forEach((value, axis) => expect(actual[axis]).toBeCloseTo(value, 9))
    }
  })

  it('emits nothing for a repeated vertex', () => {
    const point = spherePosition(10, 20)
    const segments: number[] = []
    appendSphericalArcSegments(segments, [point.x, point.y, point.z], [point.x, point.y, point.z], 1.003, 0.003)
    expect(segments).toEqual([])
  })
})
