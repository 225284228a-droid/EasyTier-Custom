export interface TrafficLink {
  source: string
  target: string
  stale?: boolean
  /** Estimated bits per second from source to target. */
  txBps?: number
  /** Estimated bits per second from target to source. */
  rxBps?: number
}

export interface TrafficObservation {
  source: string
  target: string
  connId: unknown
  txBytes: unknown
  rxBytes: unknown
  sampleTime?: number
}

interface CounterSample {
  time: number
  endpoint: string
  tx?: bigint
  rx?: bigint
}

interface DirectionTotal {
  value: number
  complete: boolean
  time?: number
}

interface EndpointTotal {
  tx: DirectionTotal
  rx: DirectionTotal
}

interface TimedRate {
  time: number
  value: number
}

interface CachedRate {
  tx?: TimedRate
  rx?: TimedRate
}

export const TOPOLOGY_CACHE_TTL_MS = 60_000
const UINT64_MAX = (1n << 64n) - 1n

function byteCounter(value: unknown): bigint | undefined {
  if (typeof value === 'number')
    return Number.isSafeInteger(value) && value >= 0 ? BigInt(value) : undefined
  if (typeof value !== 'string')
    return undefined
  const text = value.trim()
  if (!/^\d{1,20}$/.test(text))
    return undefined
  const parsed = BigInt(text)
  return parsed <= UINT64_MAX ? parsed : undefined
}

function connectionId(value: unknown): string | undefined {
  if (typeof value !== 'string')
    return undefined
  return value.trim() || undefined
}

function reportKey(source: string, target: string): string {
  return JSON.stringify([source, target])
}

function isRecent(time: number, sampleTime: number): boolean {
  return sampleTime >= time && sampleTime - time < TOPOLOGY_CACHE_TTL_MS
}

function rate(current: bigint | undefined, previous: bigint | undefined, elapsed: number): number | undefined {
  if (current === undefined || previous === undefined || current < previous || elapsed <= 0)
    return undefined
  const bps = Number(current - previous) * 8_000 / elapsed
  return Number.isFinite(bps) ? bps : undefined
}

function addRate(total: DirectionTotal, bps: number | undefined, time: number) {
  if (bps === undefined) {
    total.complete = false
    return
  }
  total.value += bps
  total.time = Math.max(total.time ?? time, time)
  if (!Number.isFinite(total.value))
    total.complete = false
}

function completeRate(total: DirectionTotal | undefined): TimedRate | undefined {
  return total?.complete && total.time !== undefined ? { value: total.value, time: total.time } : undefined
}

/** Keeps recent baselines through transient stale rounds, not confirmed disconnects. */
export class TrafficTracker {
  private samples = new Map<string, CounterSample>()
  private rates = new Map<string, CachedRate>()

  clear() {
    this.samples.clear()
    this.rates.clear()
  }

  /** Reads saved rates for a display-only refresh without advancing sample history. */
  applyCachedRates(links: TrafficLink[], time: number) {
    for (const link of links) {
      delete link.txBps
      delete link.rxBps
      if (!Number.isFinite(time))
        continue
      const saved = this.rates.get(reportKey(link.source, link.target))
      if (saved?.tx && isRecent(saved.tx.time, time))
        link.txBps = saved.tx.value
      if (saved?.rx && isRecent(saved.rx.time, time))
        link.rxBps = saved.rx.value
    }
  }

  update(links: TrafficLink[], observations: TrafficObservation[], sampleTime: number) {
    const nextSamples = new Map<string, CounterSample>()
    const nextRates = new Map<string, CachedRate>()
    const reports = new Map<string, EndpointTotal>()
    const seen = new Set<string>()

    for (const link of links) {
      delete link.txBps
      delete link.rxBps
    }
    if (!Number.isFinite(sampleTime)) {
      this.clear()
      return
    }

    for (const observation of observations) {
      const { source, target } = observation
      if (source === target)
        continue
      const connId = connectionId(observation.connId)
      const key = connId ? JSON.stringify([source, target, connId]) : undefined
      if (key && seen.has(key))
        continue
      if (key)
        seen.add(key)

      const endpoint = reportKey(source, target)
      const current: CounterSample = {
        time: observation.sampleTime ?? sampleTime,
        endpoint,
        tx: byteCounter(observation.txBytes),
        rx: byteCounter(observation.rxBytes),
      }
      const saved = key ? this.samples.get(key) : undefined
      const advancingTime = Number.isFinite(current.time)
        && current.time >= 0 && current.time <= sampleTime
        && (!saved || current.time > saved.time)
      const previous = advancingTime && saved && isRecent(saved.time, current.time) ? saved : undefined
      const elapsed = previous ? current.time - previous.time : 0
      const reset = previous && (
        (current.tx !== undefined && previous.tx !== undefined && current.tx < previous.tx)
        || (current.rx !== undefined && previous.rx !== undefined && current.rx < previous.rx)
      )
      const txBps = reset ? undefined : rate(current.tx, previous?.tx, elapsed)
      const rxBps = reset ? undefined : rate(current.rx, previous?.rx, elapsed)
      if (key && advancingTime)
        nextSamples.set(key, current)
      else if (key && saved && isRecent(saved.time, sampleTime))
        nextSamples.set(key, saved)

      const total = reports.get(endpoint) ?? {
        tx: { value: 0, complete: true },
        rx: { value: 0, complete: true },
      }
      addRate(total.tx, txBps, current.time)
      addRate(total.rx, rxBps, current.time)
      reports.set(endpoint, total)
    }

    for (const link of links) {
      const key = reportKey(link.source, link.target)
      if (link.stale) {
        const cached = this.rates.get(key)
        const tx = cached?.tx && isRecent(cached.tx.time, sampleTime) ? cached.tx : undefined
        const rx = cached?.rx && isRecent(cached.rx.time, sampleTime) ? cached.rx : undefined
        link.txBps = tx?.value
        link.rxBps = rx?.value
        if (tx || rx)
          nextRates.set(key, { tx, rx })
        continue
      }
      const preferred = reports.get(key)
      const reverse = reports.get(reportKey(link.target, link.source))
      // Both endpoints measure the same channels. Use one report per direction.
      const tx = completeRate(preferred?.tx) ?? completeRate(reverse?.rx)
      const rx = completeRate(preferred?.rx) ?? completeRate(reverse?.tx)
      link.txBps = tx?.value
      link.rxBps = rx?.value
      nextRates.set(key, { tx, rx })
    }
    const staleEndpoints = new Set(links.filter(link => link.stale).flatMap(link => [
      reportKey(link.source, link.target),
      reportKey(link.target, link.source),
    ]))
    for (const [key, saved] of this.samples) {
      if (!nextSamples.has(key) && staleEndpoints.has(saved.endpoint) && isRecent(saved.time, sampleTime))
        nextSamples.set(key, saved)
    }
    this.samples = nextSamples
    this.rates = nextRates
  }
}
