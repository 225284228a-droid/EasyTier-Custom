import { geoEquirectangular, geoPath, type GeoPermissibleObjects } from 'd3-geo'
import { buildCloudPointPositions } from './globePointCloud'

const MAP_WIDTH = 2048
const MAP_HEIGHT = 1024
const BOUNDARY_RADIUS = 1.003
const DEGREES_TO_RADIANS = Math.PI / 180

/** Vertex data of one globe detail level, ready for buffer geometry. */
export interface GeographyLayer {
  land: Float32Array
  ocean: Float32Array
  /** Line segment endpoint pairs of the country boundaries. */
  boundaries: Float32Array
  sourceVertices: number
}

export interface GeographyLayerRequest {
  id: number
  /** Absolute URL of a detail asset; the bundled coarse map when absent. */
  url?: string
  count: number
  angularStep: number
}

export interface GeographyLayerReply {
  id: number
  layer?: GeographyLayer
  error?: string
}

export async function fetchGeography(url: string): Promise<GeoPermissibleObjects> {
  const response = await fetch(url)
  if (!response.ok)
    throw new Error(`Globe detail asset failed: HTTP ${response.status}`)
  const geography = await response.json()
  if (geography?.type !== 'FeatureCollection' || !Array.isArray(geography.features))
    throw new Error('Globe detail asset is not a GeoJSON FeatureCollection')
  return geography as GeoPermissibleObjects
}

type RasterContext =Pick<CanvasRenderingContext2D, 'fillStyle' | 'beginPath' | 'fill' | 'getImageData'>
  & Parameters<typeof geoPath>[1]

export type RasterCanvasFactory = (width: number, height: number) => {
  getContext(type: '2d', options?: { willReadFrequently?: boolean }): unknown
}

function unitVector(latitude: number, longitude: number): [number, number, number] {
  const lat = latitude * DEGREES_TO_RADIANS
  const lon = longitude * DEGREES_TO_RADIANS
  return [Math.cos(lat) * Math.sin(lon), Math.sin(lat), Math.cos(lat) * Math.cos(lon)]
}

/**
 * Appends the line segments of the great-circle arc between two unit
 * vectors, subdivided like `sphericalArc` but without allocating vectors,
 * since the detailed map has hundreds of thousands of edges.
 */
export function appendSphericalArcSegments(
  out: number[],
  from: readonly [number, number, number],
  to: readonly [number, number, number],
  radius: number,
  angularStep: number,
) {
  const [ax, ay, az] = from
  const [bx, by, bz] = to
  let cx = ay * bz - az * by
  let cy = az * bx - ax * bz
  let cz = ax * by - ay * bx
  const sine = Math.hypot(cx, cy, cz)
  const angle = Math.atan2(sine, ax * bx + ay * by + az * bz)
  if (angle <= 1e-10)
    return
  if (sine <= 1e-10) {
    // Antipodal endpoints: any perpendicular axis gives a valid great circle.
    ;[cx, cy, cz] = Math.abs(ay) < 0.9 ? [-az, 0, ax] : [0, az, -ay]
  }
  const length = Math.hypot(cx, cy, cz)
  const [kx, ky, kz] = [cx / length, cy / length, cz / length]
  // Rodrigues' rotation of `from` about the axis; k·from is zero.
  const [px, py, pz] = [ky * az - kz * ay, kz * ax - kx * az, kx * ay - ky * ax]
  const segments = Math.max(1, Math.ceil(angle / angularStep))
  let previous = [ax * radius, ay * radius, az * radius]
  for (let index = 1; index <= segments; index++) {
    const theta = angle * index / segments
    const cos = Math.cos(theta)
    const sin = Math.sin(theta)
    const next = [
      (ax * cos + px * sin) * radius,
      (ay * cos + py * sin) * radius,
      (az * cos + pz * sin) * radius,
    ]
    out.push(previous[0], previous[1], previous[2], next[0], next[1], next[2])
    previous = next
  }
}

function boundarySegments(geography: GeoPermissibleObjects, angularStep: number) {
  const features = (geography as unknown as {
    features?: { geometry?: { type?: string, coordinates?: unknown } }[]
  }).features ?? []
  const points: number[] = []
  let sourceVertices = 0
  for (const feature of features) {
    const geometry = feature.geometry
    if (!geometry?.coordinates)
      continue
    const polygons = geometry.type === 'Polygon'
      ? [geometry.coordinates]
      : geometry.type === 'MultiPolygon' ? geometry.coordinates : []
    for (const polygon of polygons as unknown[]) {
      if (!Array.isArray(polygon))
        continue
      for (const ring of polygon as unknown[]) {
        if (!Array.isArray(ring))
          continue
        const coordinates = ring.filter((coordinate): coordinate is [number, number] =>
          Array.isArray(coordinate)
          && typeof coordinate[0] === 'number'
          && typeof coordinate[1] === 'number'
          && Number.isFinite(coordinate[0])
          && Number.isFinite(coordinate[1]))
        if (coordinates.length < 2)
          continue
        sourceVertices += coordinates.length
        let from = unitVector(coordinates[0][1], coordinates[0][0])
        for (let index = 1; index < coordinates.length; index++) {
          const to = unitVector(coordinates[index][1], coordinates[index][0])
          appendSphericalArcSegments(points, from, to, BOUNDARY_RADIUS, angularStep)
          from = to
        }
      }
    }
  }
  return { boundaries: new Float32Array(points), sourceVertices }
}

/**
 * Builds the land/ocean point cloud and boundary lines of one detail level.
 * This is the expensive part of the globe, so it normally runs in a worker.
 */
export function buildGeographyLayer(
  geography: GeoPermissibleObjects,
  count: number,
  angularStep: number,
  createCanvas: RasterCanvasFactory,
): GeographyLayer {
  const context = createCanvas(MAP_WIDTH, MAP_HEIGHT)
    .getContext('2d', { willReadFrequently: true }) as RasterContext | null
  if (!context)
    throw new Error('2D canvas is unavailable for the globe map')
  const projection = geoEquirectangular().scale(MAP_WIDTH / (2 * Math.PI))
    .translate([MAP_WIDTH / 2, MAP_HEIGHT / 2])
  context.fillStyle = '#ffffff'
  context.beginPath()
  geoPath(projection, context)(geography)
  context.fill()
  const pixels = context.getImageData(0, 0, MAP_WIDTH, MAP_HEIGHT).data
  const { land, ocean } = buildCloudPointPositions(count, (lat, lon) => {
    const x = Math.min(MAP_WIDTH - 1, Math.floor((lon + 180) / 360 * MAP_WIDTH))
    const y = Math.min(MAP_HEIGHT - 1, Math.floor((90 - lat) / 180 * MAP_HEIGHT))
    return pixels[(y * MAP_WIDTH + x) * 4 + 3] > 100
  })
  return {
    land: new Float32Array(land),
    ocean: new Float32Array(ocean),
    ...boundarySegments(geography, angularStep),
  }
}
