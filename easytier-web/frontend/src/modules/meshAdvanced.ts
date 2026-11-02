import type { MeshAdvancedValues } from './api'

/**
 * Advanced settings a network's members may share without depending on them
 * to connect. The console mirrors this list (`central_network/advanced.rs`);
 * settings that describe one device (addresses, routes, exits, proxies,
 * TUN, device binding) are never batch-edited, private mode keeps its
 * default and encryption is a core setting.
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
  'multi_thread',
  'enable_udp_broadcast_relay',
  'close_redundant_conns_when_disguised',
  'disable_parallel_connections',
]

export type AdvancedParameterKind = 'select' | 'number'

export interface AdvancedParameter {
  field: string
  kind: AdvancedParameterKind
  /** Choices of a select; `null` runs with the core's default. */
  options?: { label: string; value: string | number | null }[]
  min?: number
  max?: number
}

/** Transport parameters next to the switches; labels are i18n keys. */
export const ADVANCED_PARAMETERS: readonly AdvancedParameter[] = [
  {
    field: 'data_compress_algo',
    kind: 'select',
    options: [
      { label: 'web.mesh_network.value_default', value: null },
      { label: 'web.mesh_network.compress_none', value: 1 },
      { label: 'web.mesh_network.compress_zstd', value: 2 },
    ],
  },
  { field: 'mtu', kind: 'number', min: 576, max: 9000 },
  {
    field: 'traffic_disguise',
    kind: 'select',
    options: [
      { label: 'web.mesh_network.value_default', value: null },
      { label: 'traffic_disguise_speedtest', value: 'speedtest' },
      { label: 'traffic_disguise_video', value: 'video' },
    ],
  },
  { field: 'parallel_connections', kind: 'number', min: 1, max: 16 },
]

/** A field's label key; the core's flags reuse the config form's labels. */
export function advancedLabel(field: string): string {
  return field === 'data_compress_algo' ? 'web.mesh_network.compression' : field
}

/** How many members hold each value of each field. */
export type AdvancedDistribution = Record<string, Map<string, number>>

export function valueKey(value: unknown): string {
  return value === null || value === undefined ? 'default' : String(value)
}

export function advancedDistribution(members: MeshAdvancedValues[]): AdvancedDistribution {
  const distribution: AdvancedDistribution = {}
  const fields = [...ADVANCED_SWITCHES, ...ADVANCED_PARAMETERS.map(parameter => parameter.field)]
  for (const field of fields) {
    const counts = new Map<string, number>()
    for (const values of members) {
      const key = valueKey(values[field])
      counts.set(key, (counts.get(key) ?? 0) + 1)
    }
    distribution[field] = counts
  }
  return distribution
}

/** Only touched settings travel; `undefined` leaves a setting alone. */
export function touchedSettings(edits: Record<string, unknown>): MeshAdvancedValues {
  const change: MeshAdvancedValues = {}
  for (const [field, value] of Object.entries(edits)) {
    if (value !== undefined) change[field] = value as MeshAdvancedValues[string]
  }
  return change
}
