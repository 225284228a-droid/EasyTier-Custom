import type { NetworkConfig } from '../types/network'

export const LOCAL_CONFIG_REVISION_CAPABILITY = 'management:persisted-config-revision-v1'
export const LOCAL_CONFIG_APPLY_CAPABILITY = 'management:persisted-config-apply-v1'
export const HTTP3_CAPABILITY = 'transport:http3-framed-v1'

export function assertLocalConfigApplyCapability(capabilities: readonly string[]): void {
  if (!capabilities.includes(LOCAL_CONFIG_APPLY_CAPABILITY)) {
    throw new Error('Applying saved configuration without edits is unsupported. Upgrade the device before applying.')
  }
}

export const CONFIG_FIELD_CAPABILITIES: Readonly<Record<string, string>> = {
  sni: 'config:sni',
  enable_bbr: 'config:enable_bbr',
  p2p_prefer_protocol: 'config:p2p_prefer_protocol',
  only_use_wss_http3_for_hole_punching: 'config:only_use_wss_http3_for_hole_punching',
  prefer_wss_http3_for_p2p: 'config:prefer_wss_http3_for_p2p',
  disable_wss_http3_for_p2p: 'config:disable_wss_http3_for_p2p',
  close_redundant_conns_when_disguised: 'config:close_redundant_conns_when_disguised',
}

export function configFieldSupported(field: string, capabilities: readonly string[] = []): boolean {
  const required = CONFIG_FIELD_CAPABILITIES[field.split('.')[0]]
  return required === undefined || capabilities.includes(required)
}

function isHttp3Url(value: unknown): boolean {
  return typeof value === 'string' && /^http3:/i.test(value.trim())
}

const URL_FIELDS = ['peer_urls', 'listener_urls', 'mapped_listeners'] as const

/** Filter extensions individually. Official WS/WSS remain ordinary transports. */
export function filterConfigPayload<T extends object>(config: T, capabilities: readonly string[]): T {
  const filtered = { ...config } as Record<string, unknown>
  for (const [field, capability] of Object.entries(CONFIG_FIELD_CAPABILITIES)) {
    if (!capabilities.includes(capability)) delete filtered[field]
  }
  if (!capabilities.includes(HTTP3_CAPABILITY)) {
    for (const field of URL_FIELDS) {
      if (Array.isArray(filtered[field])) {
        filtered[field] = filtered[field].filter(value => !isHttp3Url(value))
      }
    }
    if (Array.isArray(filtered.peers)) {
      filtered.peers = filtered.peers.filter(peer => !isHttp3Url(peer?.uri))
    }
    if (isHttp3Url(filtered.public_server_url)) delete filtered.public_server_url
  }
  return filtered as T
}

/** A selected patch must never silently apply a smaller, different edit. */
export function assertPatchCapabilities(
  config: Partial<NetworkConfig>,
  fieldMask: readonly string[],
  capabilities: readonly string[],
): void {
  for (const path of fieldMask) {
    if (!configFieldSupported(path, capabilities)) throw new Error(`Unsupported configuration field: ${path}`)
    const field = path.split('.')[0] as keyof NetworkConfig
    const value = config[field]
    const containsHttp3 = field === 'peers'
      ? (config.peers ?? []).some(peer => isHttp3Url(peer.uri))
      : Array.isArray(value) ? value.some(isHttp3Url) : isHttp3Url(value)
    if (containsHttp3 && !capabilities.includes(HTTP3_CAPABILITY)) {
      throw new Error(`Unsupported HTTP3 transport in ${path}`)
    }
  }
}

/** Sparse raw overrides preserve explicit false/empty values; old node reads may contain generated defaults. */
export function assertLegacyConfigPreservable(
  existing: Partial<NetworkConfig> | undefined,
  capabilities: readonly string[],
  strictPresence = false,
): void {
  if (!existing) return
  const required: string[] = []
  for (const [field, capability] of Object.entries(CONFIG_FIELD_CAPABILITIES)) {
    const value = existing[field as keyof NetworkConfig]
    const present = strictPresence
      ? Object.prototype.hasOwnProperty.call(existing, field)
      : value === true || (typeof value === 'string' && value.trim() !== '')
    if (!capabilities.includes(capability) && present) {
      required.push(field)
    }
  }
  if (!capabilities.includes(HTTP3_CAPABILITY)) {
    for (const field of URL_FIELDS) {
      if (existing[field]?.some(isHttp3Url)) required.push(field)
    }
    if (existing.peers?.some(peer => isHttp3Url(peer.uri))) required.push('peers')
    if (isHttp3Url(existing.public_server_url)) required.push('public_server_url')
  }
  if (required.length) {
    throw new Error(`The device has saved custom settings (${required.join(', ')}) but does not advertise their capabilities. Upgrade the device before saving to preserve those settings.`)
  }
}
