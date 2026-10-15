import type { NetworkConfig } from '../types/network'

export const P2P_DISGUISE_MODE_CAPABILITY = 'config:p2p_disguise_mode'
export const LEGACY_P2P_DISGUISE_FIELDS = [
  'prefer_wss_http3_for_p2p',
  'disable_wss_http3_for_p2p',
  'only_use_wss_http3_for_hole_punching',
] as const

export type P2pDisguiseMode = 'prefer' | 'off' | 'disabled' | 'only'

export function isP2pDisguiseMode(value: unknown): value is P2pDisguiseMode {
  return typeof value === 'string' && ['prefer', 'off', 'disabled', 'only'].includes(value)
}

function parsedP2pDisguiseMode(value: unknown): P2pDisguiseMode | undefined {
  const normalized = typeof value === 'string' ? value.trim().toLowerCase() : undefined
  return isP2pDisguiseMode(normalized) ? normalized : undefined
}

export function legacyP2pDisguiseSupported(capabilities: readonly string[]): boolean {
  return LEGACY_P2P_DISGUISE_FIELDS.every(field => capabilities.includes(`config:${field}`))
}

/** Missing settings use the custom kernel default; explicit legacy false stays off. */
export function p2pDisguiseMode(config: Partial<NetworkConfig>): P2pDisguiseMode {
  const mode = parsedP2pDisguiseMode(config.p2p_disguise_mode)
  if (mode) return mode
  if (config.only_use_wss_http3_for_hole_punching) return 'only'
  if (config.disable_wss_http3_for_p2p) return 'disabled'
  return config.prefer_wss_http3_for_p2p === false ? 'off' : 'prefer'
}

export function legacyP2pDisguiseFlags(mode: P2pDisguiseMode) {
  return {
    prefer_wss_http3_for_p2p: mode === 'prefer',
    disable_wss_http3_for_p2p: mode === 'disabled',
    only_use_wss_http3_for_hole_punching: mode === 'only',
  }
}

/** New custom kernels save one policy; older custom kernels keep their wire format. */
export function setP2pDisguiseMode(config: NetworkConfig, mode: P2pDisguiseMode, capabilities?: readonly string[]): void {
  if (capabilities !== undefined && !capabilities.includes(P2P_DISGUISE_MODE_CAPABILITY)) {
    delete config.p2p_disguise_mode
    Object.assign(config, legacyP2pDisguiseFlags(mode))
    return
  }
  config.p2p_disguise_mode = mode
  for (const field of LEGACY_P2P_DISGUISE_FIELDS) delete config[field]
}

/** New writes use one setting; absence stays absent and old reads remain supported. */
export function canonicalizeP2pDisguisePayload(config: Record<string, unknown>): void {
  if (!parsedP2pDisguiseMode(config.p2p_disguise_mode)
    && !LEGACY_P2P_DISGUISE_FIELDS.some(field => typeof config[field] === 'boolean')) return
  config.p2p_disguise_mode = p2pDisguiseMode(config as Partial<NetworkConfig>)
  for (const field of LEGACY_P2P_DISGUISE_FIELDS) delete config[field]
}

/** Capability adaptation is the only boundary that emits legacy boolean policies. */
export function adaptP2pDisguisePayload(config: Record<string, unknown>, capabilities: readonly string[]): void {
  const mode = parsedP2pDisguiseMode(config.p2p_disguise_mode)
  if (capabilities.includes(P2P_DISGUISE_MODE_CAPABILITY)) {
    canonicalizeP2pDisguisePayload(config)
  } else if (legacyP2pDisguiseSupported(capabilities) && mode) {
    Object.assign(config, legacyP2pDisguiseFlags(mode))
    delete config.p2p_disguise_mode
  }
}
