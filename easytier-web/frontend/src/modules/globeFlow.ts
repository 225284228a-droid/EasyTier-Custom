/** RTT changes travel speed, while byte-counter rates independently set density. */
export function flowTravelSeconds(latencyMs: number | undefined): number {
  if (latencyMs === undefined || !Number.isFinite(latencyMs) || latencyMs <= 0)
    return 5
  return Math.min(8, Math.max(0.35, 0.35 + Math.log10(Math.max(1, latencyMs)) * 2.4))
}
