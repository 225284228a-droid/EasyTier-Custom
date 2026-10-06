export interface CloudPointPositions {
  land: number[]
  ocean: number[]
}

const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5))
const RADIANS_TO_DEGREES = 180 / Math.PI

/** Classify one complete equal-area sequence without thinning either surface. */
export function buildCloudPointPositions(
  count: number,
  isLand: (latitude: number, longitude: number) => boolean,
): CloudPointPositions {
  const land: number[] = []
  const ocean: number[] = []
  for (let index = 0; index < count; index++) {
    const y = 1 - 2 * (index + 0.5) / count
    const longitude = (index * GOLDEN_ANGLE) % (2 * Math.PI) - Math.PI
    const latitude = Math.asin(y)
    const radius = Math.sqrt(Math.max(0, 1 - y * y))
    const points = isLand(latitude * RADIANS_TO_DEGREES, longitude * RADIANS_TO_DEGREES)
      ? land : ocean
    points.push(radius * Math.sin(longitude), y, radius * Math.cos(longitude))
  }
  return { land, ocean }
}
