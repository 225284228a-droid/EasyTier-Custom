const MIN_TRAVEL_SECONDS = 0.08
const MAX_TRAVEL_SECONDS = 6
const FLOW_CURVE_KNEE_BPS = 16_000_000
export const MAX_FLOW_EMISSIONS_PER_SECOND = 6
export const MAX_FLOW_PARTICLES = Math.ceil(MAX_FLOW_EMISSIONS_PER_SECOND * MAX_TRAVEL_SECONDS) + 1

/** Expand RTT twentyfold for display, preserving ratios inside the readable limits. */
export function flowTravelSeconds(latencyMs: number | undefined): number {
  if (latencyMs === undefined || !Number.isFinite(latencyMs) || latencyMs <= 0)
    return 5
  return Math.min(MAX_TRAVEL_SECONDS, Math.max(MIN_TRAVEL_SECONDS, latencyMs * 0.02))
}

/** A shared soft curve keeps high throughput readable without involving RTT or other links. */
export function flowEmissionsPerSecond(rate: number | undefined, stale = false): number {
  if (stale || rate === undefined || !Number.isFinite(rate) || rate <= 0)
    return 0
  const weight = Math.sqrt(rate / FLOW_CURVE_KNEE_BPS)
  return MAX_FLOW_EMISSIONS_PER_SECOND * weight / (1 + weight)
}

/** One-shot particles: throughput schedules births; RTT only advances their journey. */
export class FlowEmitter {
  readonly progress: number[] = []
  emittedCount = 0
  private credit: number

  constructor(phase = 0) {
    this.credit = Number.isFinite(phase) ? Math.min(1 - Number.EPSILON, Math.max(0, phase)) : 0
  }

  advance(seconds: number, emissionsPerSecond: number, travelSeconds: number) {
    if (!Number.isFinite(emissionsPerSecond) || emissionsPerSecond <= 0) {
      this.progress.length = 0
      this.credit = 0
      return
    }
    if (!Number.isFinite(seconds) || seconds <= 0)
      return
    const frequency = Math.min(MAX_FLOW_EMISSIONS_PER_SECOND, emissionsPerSecond)
    const duration = Number.isFinite(travelSeconds) && travelSeconds > 0
      ? Math.min(MAX_TRAVEL_SECONDS, Math.max(MIN_TRAVEL_SECONDS, travelSeconds)) : 5
    const increment = seconds / duration
    let kept = 0
    for (const progress of this.progress) {
      const advanced = progress + increment
      if (advanced < 1 - 1e-9)
        this.progress[kept++] = advanced
    }
    this.progress.length = kept

    const previousCredit = this.credit
    const accumulated = previousCredit + seconds * frequency
    const emitted = Math.floor(accumulated + 1e-9)
    this.credit = Math.max(0, accumulated - emitted)
    this.emittedCount += emitted
    // A resumed tab may span many births. Only materialize particles still in transit.
    const first = Math.max(0, emitted - Math.ceil(duration * frequency) - 1)
    for (let index = first; index < emitted; index++) {
      const bornAt = (index + 1 - previousCredit) / frequency
      const progress = Math.max(0, (seconds - bornAt) / duration)
      if (progress < 1 - 1e-9 && this.progress.length < MAX_FLOW_PARTICLES)
        this.progress.push(progress)
    }
  }
}
