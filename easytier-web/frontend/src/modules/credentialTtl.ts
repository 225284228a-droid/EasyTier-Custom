// How long a temporary device's credential lasts, in seconds; zero asks the
// console for one that never expires.

export const PERMANENT_TTL = 0
export const MAX_TTL_SECONDS = 365 * 24 * 3600
/** The expiry the console gives a permanent credential, 9999-12-31. */
const PERMANENT_EXPIRY_UNIX = 253_402_300_799

export const HOUR = 3600
export const DAY = 24 * HOUR
export const TTL_PRESETS = [DAY, 7 * DAY, 30 * DAY, PERMANENT_TTL] as const

export function isPermanentExpiry(expiryUnix: number): boolean {
  return expiryUnix >= PERMANENT_EXPIRY_UNIX
}

/** A custom TTL in hours or days, kept within what the console accepts. */
export function customTtl(amount: number | null | undefined, unit: number): number {
  const seconds = Math.round(Math.max(1, amount ?? 1)) * unit
  return Math.min(seconds, MAX_TTL_SECONDS)
}
