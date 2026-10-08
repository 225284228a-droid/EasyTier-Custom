import { afterEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createI18n } from 'vue-i18n'
import { Button } from 'primevue'
import PrimeVue from 'primevue/config'
import LocalConfigBatch from '../src/components/LocalConfigBatch.vue'
import { DEFAULT_NETWORK_CONFIG } from '../src/types/network'
import type { LocalConfigClient, LocalConfigMachine } from '../src/modules/localConfigPatch'

const configForm = { props: ['curNetwork'], emits: ['update:curNetwork', 'runNetwork'], template: '<div class="config-form" />' }
const management = 'management:persisted-config-revision-v1'
function machine(id: string, extensions: string[] = []): LocalConfigMachine {
  return {
    machine_id: id, hostname: id, online: true, capabilities: [management, ...extensions],
    catalog_epoch: 'boot-a', catalog_generation: 1,
    entries: [{ entry_key: `${id}.toml`, inst_id: '00000000-0000-0000-0000-000000000001', revision: `${id}-revision`,
      config: DEFAULT_NETWORK_CONFIG(), config_permission: 0, enabled: true, running: true,
      persisted_raw_hash: 'a', pending_apply: false, status: 'ready', network_name: 'mesh', source: 0 }],
  }
}

const wrappers: ReturnType<typeof mount>[] = []
afterEach(() => { wrappers.splice(0).forEach(wrapper => wrapper.unmount()); vi.restoreAllMocks() })
function client(machines: LocalConfigMachine[]): LocalConfigClient & { list: any; observe: any; patch: any } {
  return {
    scope: 'connection-a',
    list: vi.fn().mockResolvedValue(machines),
    observe: vi.fn((id: string) => Promise.resolve(machines.find(machine => machine.machine_id === id)!)),
    patch: vi.fn((id: string) => Promise.resolve({ ...machines.find(machine => machine.machine_id === id)!, catalog_generation: 2 })),
  }
}
async function open(api: LocalConfigClient) {
  const wrapper = mount(LocalConfigBatch, { props: { client: api }, global: {
    plugins: [PrimeVue, createI18n({ legacy: false, locale: 'en', missingWarn: false, fallbackWarn: false })],
    stubs: { Config: configForm },
  } })
  wrappers.push(wrapper)
  await flushPromises()
  return wrapper
}
function button(wrapper: ReturnType<typeof mount>, label: string) {
  return wrapper.findAllComponents(Button).find(component => component.props('label') === label)!
}
async function edit(wrapper: ReturnType<typeof mount>) {
  for (const input of wrapper.findAll('tbody input[type="checkbox"]')) await input.setValue(true)
  await button(wrapper, 'web.local_configs.edit_selected').trigger('click')
  await flushPromises()
}

describe('online local configuration batch editor', () => {
  it('filters individual unsupported fields for mixed-capability devices', async () => {
    const api = client([machine('custom', ['config:enable_bbr']), machine('standard')])
    const wrapper = await open(api)
    await edit(wrapper)
    const form = wrapper.findComponent(configForm)
    form.vm.$emit('update:curNetwork', { ...form.props('curNetwork'), hostname: 'edited-host', enable_bbr: true })
    await flushPromises()
    form.vm.$emit('runNetwork')
    await flushPromises()
    expect(api.patch).toHaveBeenCalledTimes(2)
    const custom = api.patch.mock.calls.find(([id]: string[]) => id === 'custom')[1]
    const standard = api.patch.mock.calls.find(([id]: string[]) => id === 'standard')[1]
    expect(custom.field_mask).toEqual(expect.arrayContaining(['hostname', 'enable_bbr']))
    expect(custom.expected_revision).toBe('custom-revision')
    expect(standard.field_mask).toEqual(['hostname'])
    expect(standard.config).not.toHaveProperty('enable_bbr')
    expect(wrapper.find('.result-partial').exists()).toBe(true)
  })

  it('keeps dirty values and the original revision after a conflict and background refresh', async () => {
    const original = machine('node-a')
    const api = client([original])
    const wrapper = await open(api)
    await edit(wrapper)
    const form = wrapper.findComponent(configForm)
    form.vm.$emit('update:curNetwork', { ...form.props('curNetwork'), hostname: 'unsaved-host' })
    await flushPromises()
    api.patch.mockRejectedValue({ response: { status: 409, data: { message: 'Configuration changed; reread' } } })
    form.vm.$emit('runNetwork')
    await flushPromises()
    expect(wrapper.findComponent(configForm).props('curNetwork').hostname).toBe('unsaved-host')
    const fresh = machine('node-a')
    fresh.entries[0].revision = 'fresh-revision'
    fresh.entries[0].config!.hostname = 'remote-change'
    api.list.mockResolvedValue([fresh])
    api.observe.mockResolvedValue(fresh)
    await button(wrapper, 'web.console.refresh').trigger('click')
    await flushPromises()
    wrapper.findComponent(configForm).vm.$emit('runNetwork')
    await flushPromises()
    expect(api.patch.mock.calls[1][1].expected_revision).toBe('node-a-revision')
    expect(wrapper.findComponent(configForm).props('curNetwork').hostname).toBe('unsaved-host')
    await button(wrapper, 'web.local_configs.reread').trigger('click')
    await flushPromises()
    wrapper.findComponent(configForm).vm.$emit('runNetwork')
    await flushPromises()
    expect(api.patch.mock.calls[2][1].expected_revision).toBe('fresh-revision')
  })

  it('clears private selections and edits when switching connection scope', async () => {
    const first = client([machine('first')])
    const wrapper = await open(first)
    await edit(wrapper)
    const form = wrapper.findComponent(configForm)
    form.vm.$emit('update:curNetwork', { ...form.props('curNetwork'), hostname: 'private-edit' })
    await flushPromises()
    await wrapper.setProps({ client: { ...client([machine('second')]), scope: 'connection-b' } })
    await flushPromises()
    expect(wrapper.findComponent(configForm).exists()).toBe(false)
    expect(wrapper.text()).not.toContain('first')
    expect(wrapper.text()).toContain('second')
    expect(first.patch).not.toHaveBeenCalled()
  })

  it('shows offline snapshots but cannot select or submit them', async () => {
    const offline = { ...machine('offline'), online: false, stale: true }
    const api = client([offline])
    const wrapper = await open(api)
    expect(wrapper.find('tbody input[type="checkbox"]').attributes('disabled')).toBeDefined()
    expect(api.observe).not.toHaveBeenCalled()
    expect(api.patch).not.toHaveBeenCalled()
  })

  it('uses the row revision for explicit enable and confirmed delete actions', async () => {
    const node = machine('node-a')
    const instanceId = node.entries[0].inst_id
    node.entries[0].enabled = false
    const api = { ...client([node]), setEnabled: vi.fn().mockResolvedValue({ status: 0 }), remove: vi.fn().mockResolvedValue({ status: 0 }) }
    const wrapper = await open(api)
    await button(wrapper, 'web.local_configs.enable').trigger('click')
    await flushPromises()
    expect(api.setEnabled).toHaveBeenCalledWith('node-a', instanceId, 'node-a-revision', true)
    await button(wrapper, 'web.local_configs.delete').trigger('click')
    expect(api.remove).not.toHaveBeenCalled()
    await button(wrapper, 'web.local_configs.confirm_delete').trigger('click')
    await flushPromises()
    expect(api.remove).toHaveBeenCalledWith('node-a', instanceId, 'node-a-revision')
  })
})
