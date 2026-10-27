import mediumUrl from '../assets/world-boundaries-50m.json?url'
import detailedUrl from '../assets/world-boundaries-10m.json?url'
import { worldGeography } from './globeGeography'
import {
  buildGeographyLayer,
  fetchGeography,
  type GeographyLayer,
  type GeographyLayerReply,
  type GeographyLayerRequest,
} from './globeGeographyLayer'

export type GeographyLevel = 0 | 1 | 2

/** The coarse map is bundled; larger sources stay out of the initial chunk. */
const detailUrls: Record<GeographyLevel, string | undefined> = { 0: undefined, 1: mediumUrl, 2: detailedUrl }
const layers = new Map<GeographyLevel, Promise<GeographyLayer>>()
const waiting = new Map<number, { resolve: (layer: GeographyLayer) => void, reject: (error: Error) => void }>()
let worker: Worker | undefined
let workerFailed = false
let nextId = 0

function geographyWorker(): Worker | undefined {
  if (worker || workerFailed)
    return worker
  if (typeof Worker !== 'function' || typeof OffscreenCanvas !== 'function') {
    workerFailed = true
    return undefined
  }
  try {
    worker = new Worker(new URL('./globeGeography.worker.ts', import.meta.url), { type: 'module' })
  } catch {
    workerFailed = true
    return undefined
  }
  worker.onmessage = ({ data }: MessageEvent<GeographyLayerReply>) => {
    const request = waiting.get(data.id)
    waiting.delete(data.id)
    if (data.layer)
      request?.resolve(data.layer)
    else
      request?.reject(new Error(data.error ?? 'Globe map worker failed'))
  }
  worker.onerror = (event) => {
    // A worker that cannot start fails every request; build on this thread.
    event.preventDefault()
    workerFailed = true
    worker?.terminate()
    worker = undefined
    for (const request of waiting.values())
      request.reject(new Error('Globe map worker is unavailable'))
    waiting.clear()
  }
  return worker
}

async function buildHere(level: GeographyLevel, count: number, angularStep: number) {
  const url = detailUrls[level]
  const geography = url ? await fetchGeography(url) : worldGeography
  return buildGeographyLayer(geography, count, angularStep, (width, height) =>
    Object.assign(document.createElement('canvas'), { width, height }))
}

function buildInWorker(target: Worker, level: GeographyLevel, count: number, angularStep: number) {
  const id = nextId++
  const url = detailUrls[level]
  // Asset URLs are relative to the page, not to the worker script.
  const request: GeographyLayerRequest = { id, url: url && new URL(url, document.baseURI).href, count, angularStep }
  return new Promise<GeographyLayer>((resolve, reject) => {
    waiting.set(id, { resolve, reject })
    target.postMessage(request)
  })
}

/**
 * Vertex data of one globe detail level. It is built at most once per page,
 * off the main thread when the browser supports workers with canvases.
 */
export function loadGeographyLayer(level: GeographyLevel, count: number, angularStep: number): Promise<GeographyLayer> {
  const existing = layers.get(level)
  if (existing)
    return existing
  const target = geographyWorker()
  const request = (target
    ? buildInWorker(target, level, count, angularStep).catch((error) => {
        if (!workerFailed)
          throw error
        return buildHere(level, count, angularStep)
      })
    : buildHere(level, count, angularStep))
    .catch((error) => {
      layers.delete(level)
      throw error
    })
  layers.set(level, request)
  return request
}
