import { Capabilities } from 'easytier-frontend-lib'
import type { MeshAdvancedValues } from './api'

/**
 * Advanced settings a network's members may share without depending on them
 * to connect. The console mirrors this list (`central_network/advanced.rs`);
 * settings that describe one device (addresses, routes, exits, proxies,
 * TUN, device binding) are never batch-edited and encryption is a core
 * setting.
 */
export const ADVANCED_SWITCHES: readonly string[] = [
  'latency_first',
  'disable_ipv6',
  'enable_kcp_proxy',
  'disable_kcp_input',
  'enable_quic_proxy',
  'disable_quic_input',
  'disable_p2p',
  'p2p_only',
  'lazy_p2p',
  'need_p2p',
  'disable_udp_hole_punching',
  'disable_tcp_hole_punching',
  'disable_sym_hole_punching',
  'disable_upnp',
  'relay_all_peer_rpc',
  'disable_relay_data',
  'prefer_peer_relay',
  'enable_magic_dns',
  'enable_private_mode',
  'multi_thread',
  'enable_udp_broadcast_relay',
  'close_redundant_conns_when_disguised',
  'disable_parallel_connections',
]

export type AdvancedValue = boolean | number | string

/**
 * What the core runs with when a setting is unset (`common.proto` flag
 * defaults, negated where the API negates them). A member holding this value
 * explicitly behaves exactly like one holding nothing. Private mode has no
 * entry: custom and official kernels default it differently, so only unset
 * is its default.
 */
export const ADVANCED_DEFAULTS: Readonly<Record<string, AdvancedValue>> = {
  ...Object.fromEntries(ADVANCED_SWITCHES.filter(field => field !== 'enable_private_mode').map(field => [field, false])),
  multi_thread: true,
  close_redundant_conns_when_disguised: true,
  data_compress_algo: 1,
  mtu: 1380,
  traffic_disguise: 'speedtest',
  parallel_connections: 1,
  p2p_prefer_protocol: 'udp',
  p2p_disguise_mode: 'prefer',
}

export type AdvancedParameterKind = 'select' | 'number'

export interface AdvancedParameter {
  field: string
  kind: AdvancedParameterKind
  /** Label keys of a select's values; its default is offered as "default". */
  options?: { label: string; value: string | number }[]
  min?: number
  max?: number
}

/** Transport parameters next to the switches; labels are i18n keys. */
export const ADVANCED_PARAMETERS: readonly AdvancedParameter[] = [
  {
    field: 'data_compress_algo',
    kind: 'select',
    options: [
      { label: 'web.mesh_network.compress_none', value: 1 },
      { label: 'web.mesh_network.compress_zstd', value: 2 },
    ],
  },
  {
    field: 'p2p_prefer_protocol',
    kind: 'select',
    options: [
      { label: 'web.mesh_network.prefer_udp', value: 'udp' },
      { label: 'web.mesh_network.prefer_tcp', value: 'tcp' },
    ],
  },
  {
    field: 'p2p_disguise_mode',
    kind: 'select',
    options: [
      { label: 'web.mesh_network.disguise_prefer', value: 'prefer' },
      { label: 'web.mesh_network.disguise_off', value: 'off' },
      { label: 'web.mesh_network.disguise_disabled', value: 'disabled' },
      { label: 'web.mesh_network.disguise_only', value: 'only' },
    ],
  },
  {
    field: 'traffic_disguise',
    kind: 'select',
    options: [
      { label: 'traffic_disguise_speedtest', value: 'speedtest' },
      { label: 'traffic_disguise_video', value: 'video' },
    ],
  },
  { field: 'mtu', kind: 'number', min: 576, max: 9000 },
  { field: 'parallel_connections', kind: 'number', min: 1, max: 16 },
]

/** A field's label key; the core's flags reuse the config form's labels. */
export function advancedLabel(field: string): string {
  return field === 'data_compress_algo' ? 'web.mesh_network.compression' : field
}

/** Unset, or set to what the core would run with anyway. */
export function isDefaultValue(field: string, value: unknown): boolean {
  return value === null || value === undefined || value === ADVANCED_DEFAULTS[field]
}

/** The key a value is counted and chosen by: `default` covers both kinds of default. */
export function valueKey(field: string, value: unknown): string {
  return isDefaultValue(field, value) ? 'default' : String(value)
}

/** The key of members whose device cannot store the field. */
export const UNSUPPORTED = 'unsupported'

/** How many members hold each value of each field. */
export type AdvancedDistribution = Record<string, Map<string, number>>

export interface AdvancedMemberValues {
  /** What it held when last seen. */
  values: MeshAdvancedValues
  /** Capabilities of an online device; an offline one is judged by what it held. */
  capabilities?: readonly string[]
}

export function advancedDistribution(members: AdvancedMemberValues[]): AdvancedDistribution {
  const distribution: AdvancedDistribution = {}
  const fields = [...ADVANCED_SWITCHES, ...ADVANCED_PARAMETERS.map(parameter => parameter.field)]
  for (const field of fields) {
    const counts = new Map<string, number>()
    for (const { values, capabilities } of members) {
      const key = capabilities && !Capabilities.configFieldSupported(field, capabilities)
        ? UNSUPPORTED : valueKey(field, values[field])
      counts.set(key, (counts.get(key) ?? 0) + 1)
    }
    distribution[field] = counts
  }
  return distribution
}

/**
 * The value every member that can store the field holds, or `undefined`
 * when they differ or none can.
 */
export function sharedKey(counts: Map<string, number> | undefined): string | undefined {
  const keys = [...counts?.keys() ?? []].filter(key => key !== UNSUPPORTED)
  return keys.length === 1 ? keys[0] : undefined
}

/** Only touched settings travel; `undefined` leaves a setting alone. */
export function touchedSettings(edits: Record<string, unknown>): MeshAdvancedValues {
  const change: MeshAdvancedValues = {}
  for (const [field, value] of Object.entries(edits)) {
    if (value !== undefined) change[field] = value as MeshAdvancedValues[string]
  }
  return change
}
