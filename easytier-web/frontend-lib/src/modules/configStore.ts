import type { RemoteClient } from './api'
import { LOCAL_CONFIG_REVISION_CAPABILITY } from './capabilities'
import {
  buildLocalConfigCreatePatch, buildLocalConfigPatch, buildLocalConfigReplace, changedConfigFields,
  cloneEditableConfig, localConfigEditable, localConfigInstanceId, patchStatusName,
  type LocalConfigEntry, type LocalConfigPatchRequest, type LocalConfigPatchResult, type LocalConfigSnapshot,
} from './localConfigPatch'
import type { NetworkConfig } from '../types/network'

/** One network configuration as its node stores it. */
export interface ConfigDocument {
  instanceId: string
  config: NetworkConfig
  /** Original file contents (EasyTier Custom nodes only). */
  toml?: string
  /** File revision for compare-and-swap writes (EasyTier Custom nodes only). */
  revision?: string
}

/** The node changed the configuration since the editor read it. */
export class ConfigConflictError extends Error {
  constructor(message = 'Configuration changed elsewhere') {
    super(message)
    this.name = 'ConfigConflictError'
  }
}

/** Saved, but a running network could not start; the node kept its previous file. */
export class ConfigApplyError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ConfigApplyError'
  }
}

export interface SaveContext {
  /** The network is running, so saving it applies the configuration. */
  running: boolean
  /** Official nodes: whether the console may persist the configuration. */
  persist: boolean
}

/**
 * How the management page reads and writes one node's configurations.
 * Saving always writes the configuration; a running network applies it.
 */
export interface ConfigStore {
  readonly kind: 'revision' | 'legacy'
  /** The file editor edits the node's original file rather than a generated one. */
  readonly editsOriginalFile: boolean
  load(instanceId: string): Promise<ConfigDocument>
  /** Latest stored version, for detecting edits made elsewhere. */
  peek(instanceId: string): Promise<ConfigDocument | undefined>
  save(base: ConfigDocument, draft: NetworkConfig, context: SaveContext): Promise<ConfigDocument>
  saveFile(base: ConfigDocument, toml: string, context: SaveContext): Promise<ConfigDocument>
  create(config: NetworkConfig, start: boolean, persist: boolean): Promise<void>
  setEnabled(instanceId: string, enabled: boolean): Promise<void>
  remove(instanceId: string): Promise<void>
}

export function supportsRevisionStore(api: RemoteClient, capabilities: readonly string[], announced?: boolean): boolean {
  return !!api.observe_local_configs && !!api.patch_local_config
    && (announced === true || capabilities.includes(LOCAL_CONFIG_REVISION_CAPABILITY))
}

export function createConfigStore(api: RemoteClient, capabilities: readonly string[], revision: boolean): ConfigStore {
  return revision ? new RevisionConfigStore(api, capabilities) : new LegacyConfigStore(api)
}

function errorCode(error: unknown): string | undefined {
  const data = (error as { response?: { data?: { code?: unknown } } })?.response?.data
  return typeof data?.code === 'string' ? data.code : undefined
}

function errorMessage(error: unknown): string {
  const data = (error as { response?: { data?: unknown } })?.response?.data ?? error
  if (typeof data === 'string') return data
  if (data && typeof data === 'object' && typeof (data as { message?: unknown }).message === 'string') {
    return (data as { message: string }).message
  }
  return String(data)
}

function documentOf(entry: LocalConfigEntry): ConfigDocument {
  return {
    instanceId: localConfigInstanceId(entry),
    config: cloneEditableConfig(entry.config!),
    toml: entry.persisted_toml,
    revision: entry.revision,
  }
}

class RevisionConfigStore implements ConfigStore {
  readonly kind = 'revision'
  readonly editsOriginalFile = true

  constructor(private readonly api: RemoteClient, private readonly capabilities: readonly string[]) {}

  private async entry(instanceId: string): Promise<LocalConfigEntry | undefined> {
    const snapshot = await this.api.observe_local_configs!()
    const entry = snapshot.entries.find(entry => localConfigInstanceId(entry) === instanceId)
    return entry && localConfigEditable(snapshot, entry) ? entry : undefined
  }

  async load(instanceId: string): Promise<ConfigDocument> {
    const entry = await this.entry(instanceId)
    if (!entry) throw new Error('This configuration cannot be edited now')
    return documentOf(entry)
  }

  async peek(instanceId: string): Promise<ConfigDocument | undefined> {
    const entry = await this.entry(instanceId)
    return entry && documentOf(entry)
  }

  private async write(instanceId: string, request: LocalConfigPatchRequest): Promise<ConfigDocument> {
    let response: LocalConfigPatchResult | LocalConfigSnapshot
    try {
      response = await this.api.patch_local_config!(request)
    } catch (error) {
      // The Web console reports outcomes as HTTP errors with a stable code.
      const code = errorCode(error)
      if (code === 'local_config_revision_conflict') throw new ConfigConflictError()
      if (code === 'local_config_apply_failed') throw new ConfigApplyError(errorMessage(error))
      throw error
    }
    // The GUI forwards the node response, which carries a status.
    if ('status' in response) {
      const status = patchStatusName(response.status)
      if (status === 'conflict') throw new ConfigConflictError(response.message)
      if (status === 'apply_failed') throw new ConfigApplyError(response.message ?? status)
      if (status !== 'success') throw new Error(response.message || status)
    }
    const entry = 'entries' in response
      ? response.entries.find(entry => localConfigInstanceId(entry) === instanceId)
      : response.entry ?? response.snapshot?.entries.find(entry => localConfigInstanceId(entry) === instanceId)
    if (!entry?.config || !entry.revision) return this.load(instanceId)
    return documentOf(entry)
  }

  async save(base: ConfigDocument, draft: NetworkConfig): Promise<ConfigDocument> {
    const fields = changedConfigFields(base.config, draft)
    if (!fields.length) return base
    return this.write(base.instanceId, buildLocalConfigPatch(
      { inst_id: base.instanceId, revision: base.revision ?? '' } as LocalConfigEntry,
      draft, fields, this.capabilities))
  }

  async saveFile(base: ConfigDocument, toml: string): Promise<ConfigDocument> {
    if (toml === base.toml) return base
    return this.write(base.instanceId, buildLocalConfigReplace(
      { inst_id: base.instanceId, revision: base.revision ?? '' }, toml))
  }

  async create(config: NetworkConfig, start: boolean): Promise<void> {
    const created = await this.write(config.instance_id, buildLocalConfigCreatePatch(config, this.capabilities))
    if (start) await this.api.update_network_instance_state(created.instanceId, false, created.revision)
  }

  private async revision(instanceId: string): Promise<string> {
    const entry = await this.entry(instanceId)
    if (!entry) throw new Error('This configuration cannot be changed now')
    return entry.revision
  }

  async setEnabled(instanceId: string, enabled: boolean): Promise<void> {
    await this.api.update_network_instance_state(instanceId, !enabled, await this.revision(instanceId))
  }

  async remove(instanceId: string): Promise<void> {
    await this.api.delete_network(instanceId, await this.revision(instanceId))
  }
}

/** Official EasyTier nodes: no revisions and no access to the original file. */
class LegacyConfigStore implements ConfigStore {
  readonly kind = 'legacy'
  readonly editsOriginalFile = false

  constructor(private readonly api: RemoteClient) {}

  async load(instanceId: string): Promise<ConfigDocument> {
    return { instanceId, config: cloneEditableConfig(await this.api.get_network_config(instanceId)) }
  }

  async peek(): Promise<ConfigDocument | undefined> {
    return undefined
  }

  async save(base: ConfigDocument, draft: NetworkConfig, context: SaveContext): Promise<ConfigDocument> {
    const config = { ...cloneEditableConfig(draft), instance_id: base.instanceId }
    if (context.running) await this.api.run_network(config, context.persist)
    else await this.api.save_config(config)
    return { instanceId: base.instanceId, config: cloneEditableConfig(config) }
  }

  async saveFile(base: ConfigDocument, toml: string, context: SaveContext): Promise<ConfigDocument> {
    const parsed = await this.api.parse_config(toml)
    if (parsed.error || !parsed.config) throw new Error(parsed.error ?? 'Parsed config is empty')
    return this.save(base, parsed.config, context)
  }

  async create(config: NetworkConfig, start: boolean, persist: boolean): Promise<void> {
    if (start) await this.api.run_network(config, persist)
    else await this.api.save_config(config)
  }

  async setEnabled(instanceId: string, enabled: boolean): Promise<void> {
    await this.api.update_network_instance_state(instanceId, !enabled)
  }

  async remove(instanceId: string): Promise<void> {
    await this.api.delete_network(instanceId)
  }
}
