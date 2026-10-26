import { computed, ref, shallowRef } from 'vue'
import { normalizeNetworkConfig, toBackendNetworkConfig, type NetworkConfig } from '../types/network'
import { changedConfigFields, cloneEditableConfig } from './localConfigPatch'
import { ConfigConflictError, type ConfigDocument, type ConfigStore, type SaveContext } from './configStore'

function backend(config: NetworkConfig): Record<string, unknown> {
  return toBackendNetworkConfig(config) as unknown as Record<string, unknown>
}

function same(left: unknown, right: unknown): boolean {
  return JSON.stringify(left) === JSON.stringify(right)
}

/**
 * Carry the fields edited in `draft` (relative to `base`) over to `latest`.
 * A field is in conflict only if both sides changed it to different values.
 */
export function rebaseDraft(base: NetworkConfig, draft: NetworkConfig, latest: NetworkConfig): {
  draft: NetworkConfig
  conflicts: string[]
} {
  const mine = changedConfigFields(base, draft)
  const theirs = new Set(changedConfigFields(base, latest))
  const edited = backend(draft)
  const merged = backend(latest)
  const conflicts = mine.filter(field => theirs.has(field) && !same(edited[field], merged[field]))
  for (const field of mine) {
    if (edited[field] === undefined) delete merged[field]
    else merged[field] = edited[field]
  }
  return {
    draft: normalizeNetworkConfig({ ...merged, instance_id: latest.instance_id } as unknown as NetworkConfig),
    conflicts,
  }
}

export type SaveOutcome = 'saved' | 'unchanged' | 'conflict'

/**
 * One editing session of one network configuration. The base is the version
 * the draft started from; saving writes the draft's changes against it.
 */
export function createConfigEditor() {
  const store = shallowRef<ConfigStore>()
  const base = ref<ConfigDocument>()
  const draft = ref<NetworkConfig>()
  /** A newer stored version whose changes overlap the draft's. */
  const remote = ref<ConfigDocument>()
  const conflictFields = ref<string[]>([])
  /** Changes made elsewhere were merged into the draft. */
  const merged = ref(false)
  const saving = ref(false)
  let session = 0

  const dirtyFields = computed(() => base.value && draft.value ? changedConfigFields(base.value.config, draft.value) : [])
  const dirty = computed(() => dirtyFields.value.length > 0)

  function adopt(document: ConfigDocument) {
    base.value = document
    draft.value = cloneEditableConfig(document.config)
    remote.value = undefined
    conflictFields.value = []
  }

  function close() {
    session++
    store.value = undefined
    base.value = undefined
    draft.value = undefined
    remote.value = undefined
    conflictFields.value = []
    merged.value = false
    saving.value = false
  }

  async function open(target: ConfigStore, instanceId: string) {
    close()
    const current = session
    store.value = target
    const document = await target.load(instanceId)
    if (current !== session) return
    adopt(document)
  }

  /** Reconcile with a version stored elsewhere (a local edit or another console). */
  function receive(latest: ConfigDocument) {
    if (!base.value || latest.revision === undefined || latest.revision === base.value.revision) return
    if (remote.value?.revision === latest.revision) return
    if (!dirty.value) {
      adopt(latest)
      return
    }
    const result = rebaseDraft(base.value.config, draft.value!, latest.config)
    if (result.conflicts.length) {
      remote.value = latest
      conflictFields.value = result.conflicts
      return
    }
    base.value = latest
    draft.value = result.draft
    remote.value = undefined
    conflictFields.value = []
    merged.value = true
  }

  async function refresh() {
    const target = store.value
    const instanceId = base.value?.instanceId
    if (!target || !instanceId || saving.value) return
    const current = session
    const latest = await target.peek(instanceId)
    if (current !== session || saving.value) return
    if (latest) receive(latest)
  }

  /** Keep the draft's values for every edited field on top of the newer version. */
  function keepMine() {
    if (!remote.value || !base.value || !draft.value) return
    const result = rebaseDraft(base.value.config, draft.value, remote.value.config)
    base.value = remote.value
    draft.value = result.draft
    remote.value = undefined
    conflictFields.value = []
  }

  /** Drop the draft and continue from the newer version. */
  function takeTheirs() {
    if (remote.value) adopt(remote.value)
  }

  /** Abandon the draft's edits. */
  function discard() {
    if (base.value) adopt(remote.value ?? base.value)
    merged.value = false
  }

  async function conflict(target: ConfigStore, instanceId: string, current: number): Promise<SaveOutcome> {
    const latest = await target.peek(instanceId)
    if (current === session && latest) receive(latest)
    return 'conflict'
  }

  async function save(context: SaveContext): Promise<SaveOutcome> {
    const target = store.value
    if (!target || !base.value || !draft.value || saving.value || remote.value) return 'unchanged'
    if (!dirty.value) return 'unchanged'
    const current = session
    const submitted = cloneEditableConfig(draft.value)
    const instanceId = base.value.instanceId
    saving.value = true
    try {
      const saved = await target.save(base.value, submitted, context)
      if (current !== session) return 'saved'
      // Edits made while the request was in flight stay in the draft.
      const pending = rebaseDraft(submitted, draft.value!, saved.config).draft
      base.value = saved
      draft.value = pending
      merged.value = false
      return 'saved'
    } catch (error) {
      if (error instanceof ConfigConflictError) return conflict(target, instanceId, current)
      throw error
    } finally {
      if (current === session) saving.value = false
    }
  }

  async function saveFile(toml: string, context: SaveContext): Promise<SaveOutcome> {
    const target = store.value
    if (!target || !base.value || saving.value) return 'unchanged'
    const current = session
    const instanceId = base.value.instanceId
    saving.value = true
    try {
      const saved = await target.saveFile(base.value, toml, context)
      if (current === session) adopt(saved)
      return 'saved'
    } catch (error) {
      if (error instanceof ConfigConflictError) return conflict(target, instanceId, current)
      throw error
    } finally {
      if (current === session) saving.value = false
    }
  }

  return {
    store, base, draft, remote, conflictFields, merged, saving, dirtyFields, dirty,
    open, close, refresh, receive, keepMine, takeTheirs, discard, save, saveFile,
  }
}

export type ConfigEditor = ReturnType<typeof createConfigEditor>
