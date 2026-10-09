import { describe, expect, it } from 'vitest'
import { buildCloudPointPositions } from '../src/modules/globePointCloud'

describe('globe equal-area point cloud', () => {
  it.each([48_000, 96_000, 288_000])('retains every point at the %i-point detail level', (count) => {
    const { land, ocean } = buildCloudPointPositions(count, latitude => latitude >= 0)
    expect(land.length).toBe(count / 2 * 3)
    expect(ocean.length).toBe(count / 2 * 3)
  })

  it('does not change the sampling sequence when land and ocean change', () => {
    const allLand = buildCloudPointPositions(24_000, () => true)
    const allOcean = buildCloudPointPositions(24_000, () => false)
    expect(allLand.ocean).toEqual([])
    expect(allOcean.land).toEqual([])
    expect(allLand.land).toEqual(allOcean.ocean)
  })

  it('places every point on the unit sphere with finite geographic coordinates', () => {
    let geographicSamples = 0
    let invalidGeographicCoordinates = 0
    let invalidSphereCoordinates = 0
    let maxUnitSphereError = 0
    const { ocean } = buildCloudPointPositions(24_000, (latitude, longitude) => {
      geographicSamples++
      if (!Number.isFinite(latitude) || !Number.isFinite(longitude)
        || Math.abs(latitude) > 90 || Math.abs(longitude) > 180)
        invalidGeographicCoordinates++
      return false
    })
    for (let index = 0; index < ocean.length; index += 3) {
      const radius = Math.hypot(ocean[index], ocean[index + 1], ocean[index + 2])
      if (!Number.isFinite(radius))
        invalidSphereCoordinates++
      else
        maxUnitSphereError = Math.max(maxUnitSphereError, Math.abs(radius - 1))
    }
    expect(geographicSamples).toBe(24_000)
    expect(invalidGeographicCoordinates).toBe(0)
    expect(invalidSphereCoordinates).toBe(0)
    expect(maxUnitSphereError).toBeLessThan(5e-13)
  })

  it('keeps comparable density across equal-area latitude and longitude sectors', () => {
    const latitudeBands = 12
    const longitudeSectors = 12
    const sectors = Array<number>(latitudeBands * longitudeSectors).fill(0)
    const { ocean } = buildCloudPointPositions(24_000, () => false)
    for (let index = 0; index < ocean.length; index += 3) {
      const latitudeBand = Math.floor((ocean[index + 1] + 1) / 2 * latitudeBands)
      const longitude = Math.atan2(ocean[index], ocean[index + 2])
      const longitudeSector = Math.floor((longitude + Math.PI) / (2 * Math.PI) * longitudeSectors)
      sectors[latitudeBand * longitudeSectors + longitudeSector]++
    }
    expect(sectors.reduce((sum, count) => sum + count, 0)).toBe(24_000)
    const expected = 24_000 / sectors.length
    expect(Math.max(...sectors.map(count => Math.abs(count - expected)))).toBeLessThan(8)
  })
})
