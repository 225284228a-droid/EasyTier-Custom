import { describe, expect, it, vi } from 'vitest'
import { DEFAULT_NETWORK_CONFIG, normalizeNetworkConfig, type NetworkConfig } from '../src/types/network'
import { createConfigEditor, rebaseDraft } from '../src/modules/configEditor'
import { ConfigConflictError, type ConfigDocument, type ConfigStore } from '../src/modules/configStore'

const ID = '00000000-0000-0000-0000-000000000001'

function config(overrides: Partial<NetworkConfig> = {}): NetworkConfig {
  return normalizeNetworkConfig({ ...DEFAULT_NETWORK_CONFIG(), instance_id: ID, hostname: 'base', ...overrides })
}

function document(revision: string, overrides: Partial<NetworkConfig> = {}): ConfigDocument {
  return { instanceId: ID, revision, config: config(overrides), toml: `# ${revision}` }
}

function fakeStore(initial: ConfigDocument) {
  let current = initial
  const store = {
    kind: 'revision',
    editsOriginalFile: true,
    set current(value: ConfigDocument) { current = value },
    load: vi.fn(async () => current),
    peek: vi.fn(async () => current),
    save: vi.fn(async (_base: ConfigDocument, draft: NetworkConfig) => {
      current = { ...current, revision: `${current.revision}+`, config: normalizeNetworkConfig(draft) }
      return current
    }),
    saveFile: vi.fn(),
    create: vi.fn(),
    setEnabled: vi.fn(),
    remove: vi.fn(),
  }
  return store as typeof store & ConfigStore
}

const context = { running: true, persist: true }

describe('rebaseDraft', () => {
  it('carries edited fields onto the latest version and leaves other changes intact', () => {
    const base = config()
    const draft = config({ hostname: 'mine' })
    const latest = config({ network_name: 'renamed' })
    const result = rebaseDraft(base, draft, latest)
    expect(result.conflicts).toEqual([])
    expect(result.draft.hostname).toBe('mine')
    expect(result.draft.network_name).toBe('renamed')
  })

  it('reports fields both sides changed to different values', () => {
    const base = config()
    expect(rebaseDraft(base, config({ hostname: 'mine' }), config({ hostname: 'theirs' })).conflicts).toEqual(['hostname'])
    expect(rebaseDraft(base, config({ hostname: 'same' }), config({ hostname: 'same' })).conflicts).toEqual([])
  })

  it('carries a cleared field', () => {
    const base = config({ mtu: 1300 })
    const draft = config({ mtu: undefined })
    const result = rebaseDraft(base, draft, config({ mtu: 1300, network_name: 'renamed' }))
    expect(result.draft.mtu ?? undefined).toBeUndefined()
    expect(result.draft.network_name).toBe('renamed')
  })
})

describe('createConfigEditor', () => {
  it('follows newer versions while the draft is unchanged', async () => {
    const store = fakeStore(document('a'))
    const editor = createConfigEditor()
    await editor.open(store, ID)
    store.current = document('b', { hostname: 'local-edit' })
    await editor.refresh()
    expect(editor.base.value?.revision).toBe('b')
    expect(editor.draft.value?.hostname).toBe('local-edit')
    expect(editor.dirty.value).toBe(false)
    expect(editor.merged.value).toBe(false)
  })

  it('merges independent changes and holds overlapping ones for a decision', async () => {
    const store = fakeStore(document('a'))
    const editor = createConfigEditor()
    await editor.open(store, ID)
    editor.draft.value!.hostname = 'mine'

    store.current = document('b', { network_name: 'renamed' })
    await editor.refresh()
    expect(editor.base.value?.revision).toBe('b')
    expect(editor.merged.value).toBe(true)
    expect(editor.dirtyFields.value).toEqual(['hostname'])

    store.current = document('c', { network_name: 'renamed', hostname: 'theirs' })
    await editor.refresh()
    expect(editor.base.value?.revision).toBe('b')
    expect(editor.remote.value?.revision).toBe('c')
    expect(editor.conflictFields.value).toEqual(['hostname'])
    expect(await editor.save(context)).toBe('unchanged')
    expect(store.save).not.toHaveBeenCalled()

    editor.keepMine()
    expect(editor.base.value?.revision).toBe('c')
    expect(editor.draft.value?.hostname).toBe('mine')
    expect(await editor.save(context)).toBe('saved')
    expect(store.save.mock.calls[0][0].revision).toBe('c')
    expect(editor.dirty.value).toBe(false)
  })

  it('takes the latest version on request', async () => {
    const store = fakeStore(document('a'))
    const editor = createConfigEditor()
    await editor.open(store, ID)
    editor.draft.value!.hostname = 'mine'
    store.current = document('b', { hostname: 'theirs' })
    await editor.refresh()
    editor.takeTheirs()
    expect(editor.draft.value?.hostname).toBe('theirs')
    expect(editor.dirty.value).toBe(false)
  })

  it('turns a save conflict into the newer version without resubmitting', async () => {
    const store = fakeStore(document('a'))
    const editor = createConfigEditor()
    await editor.open(store, ID)
    editor.draft.value!.hostname = 'mine'
    store.save.mockImplementationOnce(async () => {
      store.current = document('b', { hostname: 'theirs' })
      throw new ConfigConflictError()
    })
    expect(await editor.save(context)).toBe('conflict')
    expect(store.save).toHaveBeenCalledOnce()
    expect(editor.remote.value?.revision).toBe('b')
    expect(editor.draft.value?.hostname).toBe('mine')
  })

  it('keeps edits made while a save is in flight', async () => {
    const store = fakeStore(document('a'))
    const editor = createConfigEditor()
    await editor.open(store, ID)
    editor.draft.value!.hostname = 'submitted'
    let finish!: () => void
    const original = store.save.getMockImplementation()!
    store.save.mockImplementationOnce(async (base, draft) => {
      await new Promise<void>(resolve => { finish = resolve })
      return original(base, draft)
    })
    const saving = editor.save(context)
    await Promise.resolve()
    editor.draft.value!.network_name = 'typed-later'
    finish()
    expect(await saving).toBe('saved')
    expect(store.save.mock.calls[0][1].network_name).not.toBe('typed-later')
    expect(editor.draft.value?.hostname).toBe('submitted')
    expect(editor.dirtyFields.value).toEqual(['network_name'])
  })

  it('ignores a load that finishes after the session closed', async () => {
    const store = fakeStore(document('a'))
    let finish!: (value: ConfigDocument) => void
    store.load.mockImplementationOnce(() => new Promise(resolve => { finish = resolve }))
    const editor = createConfigEditor()
    const opening = editor.open(store, ID)
    editor.close()
    finish(document('a'))
    await opening
    expect(editor.base.value).toBeUndefined()
  })
})
