import { NETWORK_CONFIG_FIELDS } from '../generated/proto/networkConfigFields'
import { normalizeNetworkConfig, toBackendNetworkConfig, type NetworkConfig } from '../types/network'
import { type UUID, UuidToStr } from './utils'
import { ConfigFilePermission } from './api'
import { assertPatchCapabilities, configFieldSupported, LOCAL_CONFIG_REVISION_CAPABILITY } from './capabilities'

export interface LocalConfigEntry {
  entry_key: string
  inst_id?: string | UUID | null
  revision: string
  persisted_toml?: string
  config?: NetworkConfig
  config_permission: number
  enabled: boolean
  running: boolean
  persisted_raw_hash: string
  active_raw_hash?: string
  status: string
  network_name: string
  source: string | number
}

export interface LocalConfigSnapshot {
  catalog_epoch: string
  catalog_generation: string | number
  entries: LocalConfigEntry[]
  online: boolean
  stale?: boolean
  support_local_config_revision?: boolean
  capabilities: string[]
  observed_at?: string
}

/**
 * Every successful write is the new file; a running network restarts with it.
 * A typed patch merges `field_mask` into the file, while `toml` replaces it.
 */
export interface LocalConfigPatchRequest {
  inst_id: string
  expected_revision: string
  config?: Partial<NetworkConfig>
  field_mask: string[]
  toml?: string
}

export interface LocalConfigPatchResult {
  status: string | number
  message?: string
  entry?: LocalConfigEntry
  catalog_epoch?: string
  catalog_generation?: string | number
  snapshot?: LocalConfigSnapshot
}

const NON_EDIT_FIELDS = new Set(['instance_id', 'advanced_settings', 'networking_method', 'public_server_url'])
const EDIT_FIELDS = new Set<string>(NETWORK_CONFIG_FIELDS.filter(field => !NON_EDIT_FIELDS.has(field)))

export function localConfigInstanceId(entry: Pick<LocalConfigEntry, 'inst_id'>): string {
  // Invalid catalog entries can have no UUID. They still carry diagnostics,
  // but must not interfere with finding another entry by its instance ID.
  if (!entry.inst_id) return ''
  return typeof entry.inst_id === 'string' ? entry.inst_id : UuidToStr(entry.inst_id)
}

export function localConfigEditable(snapshot: LocalConfigSnapshot, entry: LocalConfigEntry): boolean {
  return snapshot.online && !snapshot.stale
    && (snapshot.support_local_config_revision === true || snapshot.capabilities.includes(LOCAL_CONFIG_REVISION_CAPABILITY))
    && !!localConfigInstanceId(entry) && entry.status === 'ready' && !!entry.revision && !!entry.config
    && ConfigFilePermission.isEditable(entry.config_permission ?? 0)
}

export function cloneEditableConfig(config: NetworkConfig): NetworkConfig {
  // Unknown target fields stay in its raw TOML. They are never copied from
  // another device into the editor or a typed configuration patch.
  return normalizeNetworkConfig(config)
}

function jsonValue(value: unknown): string {
  return JSON.stringify(value) ?? 'undefined'
}

export function changedConfigFields(baseline: NetworkConfig, current: NetworkConfig): string[] {
  const before = toBackendNetworkConfig(baseline) as unknown as Record<string, unknown>
  const after = toBackendNetworkConfig(current) as unknown as Record<string, unknown>
  return [...EDIT_FIELDS].filter(field => jsonValue(before[field]) !== jsonValue(after[field]))
}

function valueAt(source: unknown, path: string[]): unknown {
  return path.reduce<unknown>((value, field) => value && typeof value === 'object'
    ? (value as Record<string, unknown>)[field] : undefined, source)
}

function putPath(target: Record<string, unknown>, path: string[], value: unknown): void {
  if (value === undefined) return // the mask represents an explicit reset/deletion
  let container = target
  for (const field of path.slice(0, -1)) {
    container[field] ??= {}
    container = container[field] as Record<string, unknown>
  }
  container[path[path.length - 1]] = value
}

export function buildLocalConfigPatch(
  entry: LocalConfigEntry,
  current: NetworkConfig,
  fieldMask: readonly string[],
  capabilities: readonly string[],
  allowCreation = false,
): LocalConfigPatchRequest {
  const instId = localConfigInstanceId(entry)
  if (!instId) throw new Error('Configuration identity is unavailable')
  if (!entry.revision && !allowCreation) throw new Error('Configuration revision is unavailable')
  const mask = [...new Set(fieldMask)]
  if (!mask.length) throw new Error('No configuration fields selected')
  for (const field of mask) {
    if (!EDIT_FIELDS.has(field.split('.')[0]) || !/^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$/.test(field)) {
      throw new Error(`Invalid configuration field: ${field}`)
    }
  }
  assertPatchCapabilities(current, mask, capabilities)
  const backend = toBackendNetworkConfig(current, capabilities)
  const config: Record<string, unknown> = {}
  for (const field of mask) putPath(config, field.split('.'), valueAt(backend, field.split('.')))
  return {
    inst_id: instId,
    expected_revision: entry.revision,
    config: config as Partial<NetworkConfig>,
    field_mask: mask,
  }
}

/** Replace the whole file, as an editor on the node would. */
export function buildLocalConfigReplace(
  entry: Pick<LocalConfigEntry, 'inst_id' | 'revision'>,
  toml: string,
): LocalConfigPatchRequest {
  const instId = localConfigInstanceId(entry)
  if (!instId) throw new Error('Configuration identity is unavailable')
  if (!entry.revision) throw new Error('Configuration revision is unavailable')
  return { inst_id: instId, expected_revision: entry.revision, field_mask: [], toml }
}

export function buildLocalConfigCreatePatch(
  config: NetworkConfig,
  capabilities: readonly string[],
): LocalConfigPatchRequest {
  // Creation always writes a disabled network; starting it is a separate step.
  const fields = [...EDIT_FIELDS].filter(field => configFieldSupported(field, capabilities))
  return buildLocalConfigPatch({ inst_id: config.instance_id, revision: '' } as LocalConfigEntry,
    config, fields, capabilities, true)
}

export function patchStatusName(status: string | number | undefined): string {
  if (status === undefined || status === 0 || status === 'Success' || status === 'SUCCESS') return 'success'
  if (typeof status === 'number') {
    return ['success', 'conflict', 'protected', 'invalid', 'not_found', 'apply_failed', 'unsupported'][status] ?? 'unknown'
  }
  return status.replace(/^CONFIG_PATCH_/i, '').replace(/([a-z])([A-Z])/g, '$1_$2').toLowerCase()
}
