import type { NetworkConfig } from '../types/network'

export type P2pDisguiseMode = 'prefer' | 'off' | 'disabled' | 'only'

export function isP2pDisguiseMode(value: unknown): value is P2pDisguiseMode {
  return typeof value === 'string' && ['prefer', 'off', 'disabled', 'only'].includes(value)
}

function parsedP2pDisguiseMode(value: unknown): P2pDisguiseMode | undefined {
  const normalized = typeof value === 'string' ? value.trim().toLowerCase() : undefined
  return isP2pDisguiseMode(normalized) ? normalized : undefined
}

/** A missing setting uses the custom kernel default. */
export function p2pDisguiseMode(config: Partial<NetworkConfig>): P2pDisguiseMode {
  return parsedP2pDisguiseMode(config.p2p_disguise_mode) ?? 'prefer'
}

export function setP2pDisguiseMode(config: NetworkConfig, mode: P2pDisguiseMode): void {
  config.p2p_disguise_mode = mode
}

/** Writes use the canonical spelling; absence stays absent. */
export function canonicalizeP2pDisguisePayload(config: Record<string, unknown>): void {
  const mode = parsedP2pDisguiseMode(config.p2p_disguise_mode)
  if (mode) config.p2p_disguise_mode = mode
}
