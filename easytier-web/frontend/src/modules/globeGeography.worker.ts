import type { GeoPermissibleObjects } from 'd3-geo'
import world from '../assets/world-countries.json'
import { buildGeographyLayer, fetchGeography, type GeographyLayerReply, type GeographyLayerRequest } from './globeGeographyLayer'

// Rasterizing and subdividing the detailed map takes seconds on slower
// machines; doing it here keeps the dashboard responsive while zooming.
self.onmessage = async ({ data }: MessageEvent<GeographyLayerRequest>) => {
  let reply: GeographyLayerReply
  try {
    const geography = data.url ? await fetchGeography(data.url) : world as unknown as GeoPermissibleObjects
    reply = {
      id: data.id,
      layer: buildGeographyLayer(geography, data.count, data.angularStep,
        (width, height) => new OffscreenCanvas(width, height)),
    }
  } catch (error) {
    reply = { id: data.id, error: String(error) }
  }
  const layer = reply.layer
  self.postMessage(reply, { transfer: layer ? [layer.land.buffer, layer.ocean.buffer, layer.boundaries.buffer] : [] })
}
