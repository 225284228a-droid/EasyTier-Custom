import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { defineComponent, nextTick } from 'vue'
import RemoteManagement from '../src/components/RemoteManagement.vue'
import {
  DEFAULT_NETWORK_CONFIG,
  type NetworkConfig,
} from '../src/types/network'

const BOOLEAN_CONFIG_FIELDS = [
  'dhcp',
  'advanced_settings',
  'latency_first',
  'use_smoltcp',
  'disable_ipv6',
  'enable_kcp_proxy',
  'disable_kcp_input',
  'disable_p2p',
  'bind_device',
  'no_tun',
  'relay_all_peer_rpc',
  'multi_thread',
  'enable_relay_network_whitelist',
  'enable_manual_routes',
  'proxy_forward_by_system',
  'disable_encryption',
  'enable_socks5',
  'disable_udp_hole_punching',
  'enable_magic_dns',
  'enable_private_mode',
  'enable_quic_proxy',
  'disable_quic_input',
  'disable_sym_hole_punching',
  'p2p_only',
  'lazy_p2p',
  'need_p2p',
  'disable_upnp',
  'disable_relay_data',
  'enable_udp_broadcast_relay',
  'disable_tcp_hole_punching',
] as const satisfies readonly (keyof NetworkConfig)[]

vi.mock('vue-i18n', () => ({
  useI18n: () => ({
    t: (key: string, params?: Record<string, unknown>) =>
      params ? `${key} ${JSON.stringify(params)}` : key,
  }),
}))

const toastSpy = vi.hoisted(() => ({ add: vi.fn() }))
const confirmSpy = vi.hoisted(() => ({ require: vi.fn() }))

vi.mock('primevue', async () => {
  const { defineComponent, h } = await import('vue')

  const PassThrough = defineComponent({
    name: 'PassThrough',
    props: {
      label: String,
      value: String,
    },
    setup(props, { slots }) {
      return () => h('div', {
        'data-label': props.label,
        'data-value': props.value,
        'data-stub': 'pass-through',
      }, slots.default?.())
    },
  })

  const ButtonStub = defineComponent({
    name: 'Button',
    props: {
      label: String,
      icon: String,
      disabled: Boolean,
      loading: Boolean,
      loadingIcon: String,
    },
    emits: ['click'],
    setup(props, { slots, emit }) {
      return () => h('button', {
        type: 'button',
        disabled: props.disabled,
        'data-label': props.label ?? props.icon,
        onClick: (event: MouseEvent) => emit('click', event),
      }, slots.default?.() ?? props.label ?? props.icon)
    },
  })

  const SelectStub = defineComponent({
    name: 'Select',
    props: {
      modelValue: Object,
      options: Array,
    },
    emits: ['update:modelValue'],
    setup(props, { slots }) {
      return () => h('div', { 'data-stub': 'select' }, [
        slots.value?.({ value: props.modelValue, placeholder: '' }),
      ])
    },
  })

  const MenuStub = defineComponent({
    name: 'Menu',
    props: ['model'],
    setup(_, { expose }) {
      expose({ toggle: vi.fn() })
      return () => h('div', { 'data-stub': 'menu' })
    },
  })

  return {
    Button: ButtonStub,
    ConfirmPopup: PassThrough,
    Divider: PassThrough,
    IftaLabel: PassThrough,
    Menu: MenuStub,
    Message: PassThrough,
    Select: SelectStub,
    Tag: PassThrough,
    useConfirm: () => ({ require: confirmSpy.require }),
    useToast: () => ({ add: toastSpy.add }),
  }
})

const INSTANCE_ID = '00000000-0000-0000-0000-000000000001'
const INSTANCE_UUID = {
  part1: 0,
  part2: 0,
  part3: 0,
  part4: 1,
}

function makeFlagConfig(): NetworkConfig {
  const config = {
    ...DEFAULT_NETWORK_CONFIG(),
    instance_id: INSTANCE_ID,
    network_name: 'mesh-save',
  }

  BOOLEAN_CONFIG_FIELDS.forEach((field, index) => {
    config[field] = index % 2 === 0
  })

  return config
}

function cloneConfig(config: NetworkConfig): NetworkConfig {
  return JSON.parse(JSON.stringify(config)) as NetworkConfig
}

function snapshotBooleanConfigFields(config: NetworkConfig): Record<string, unknown> {
  return Object.fromEntries(
    BOOLEAN_CONFIG_FIELDS.map((field) => [field, config[field]]),
  )
}

const ConfigForm = defineComponent({
  name: 'Config', props: ['curNetwork', 'actionLabel', 'configInvalid'], emits: ['runNetwork'], template: '<div class="config-form" />',
})
const ConfigFileDialog = defineComponent({
  name: 'ConfigEditDialog', props: ['visible', 'curNetwork', 'generateConfig', 'saveConfig'], emits: ['update:visible'],
  template: '<div class="config-file-dialog" />',
})

const REVISION = 'management:persisted-config-revision-v2'

function revisionApi() {
  const config = { ...makeFlagConfig(), hostname: 'original-host' }
  const entry = { entry_key: 'mesh.toml', inst_id: INSTANCE_ID, revision: 'revision-a', config,
    persisted_toml: `# node file\ninstance_id = '${INSTANCE_ID}'\nhostname = 'original-host'\n`,
    config_permission: 0, enabled: false, running: false, persisted_raw_hash: 'a',
    status: 'ready', network_name: 'mesh', source: 1 }
  const snapshot = { catalog_epoch: 'boot-a', catalog_generation: 1, entries: [entry as any], online: true,
    capabilities: [REVISION] }
  const api = {
    scope: 'connection-a',
    list_network_instance_ids: vi.fn(async () => ({ disabled_inst_ids: [INSTANCE_UUID], running_inst_ids: [] as any[],
      runtime_capabilities: snapshot.capabilities })),
    get_network_metas: vi.fn(async () => ({ metas: { [INSTANCE_ID]: { network_name: 'mesh', config_permission: 0 } } })),
    get_network_config: vi.fn(async () => cloneConfig(config)), get_network_info: vi.fn(), get_vpn_portal_info: vi.fn(),
    observe_local_configs: vi.fn(async () => structuredClone(snapshot)),
    patch_local_config: vi.fn(async (request: any) => ({ status: 0, entry: { ...entry, revision: 'revision-b',
      config: { ...entry.config, ...request.config } } })),
    save_config: vi.fn(), run_network: vi.fn(), delete_network: vi.fn(), update_network_instance_state: vi.fn(),
    validate_config: vi.fn(), generate_config: vi.fn(async () => ({ toml_config: 'generated = true' })), parse_config: vi.fn(),
  }
  return { entry, snapshot, api }
}

function running(state: ReturnType<typeof revisionApi>) {
  Object.assign(state.entry, { enabled: true, running: true })
  state.api.list_network_instance_ids.mockResolvedValue({ running_inst_ids: [INSTANCE_UUID], disabled_inst_ids: [],
    runtime_capabilities: state.snapshot.capabilities })
  state.api.get_network_info.mockResolvedValue({ running: true })
  return state
}

function legacy(state: ReturnType<typeof revisionApi>) {
  state.snapshot.capabilities.length = 0
  delete (state.api as any).observe_local_configs
  delete (state.api as any).patch_local_config
  return state
}

async function mountManagement(api: any, instanceId: string | null = INSTANCE_ID, newConfigGenerator?: () => NetworkConfig) {
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
  const wrapper = mount(RemoteManagement, { props: { api, instanceId: instanceId ?? undefined, newConfigGenerator }, global: {
    stubs: { Config: ConfigForm, ConfigEditDialog: ConfigFileDialog, Status: true },
  } })
  await vi.advanceTimersByTimeAsync(1)
  await flushPromises()
  return wrapper
}

type Wrapper = Awaited<ReturnType<typeof mountManagement>>

async function editRunning(wrapper: Wrapper) {
  const actions = wrapper.findComponent({ name: 'Menu' }).props('model')
  await actions[0].command()
  await flushPromises()
  return wrapper.findComponent(ConfigForm)
}

const form = (wrapper: Wrapper) => wrapper.findComponent(ConfigForm)
const button = (wrapper: Wrapper, label: string) => wrapper.find(`button[data-label="${label}"]`)

async function submit(wrapper: Wrapper) {
  form(wrapper).vm.$emit('runNetwork')
  await flushPromises()
}

async function saveConfig(wrapper: Wrapper) {
  await button(wrapper, 'web.device_management.save_config').trigger('click')
  await flushPromises()
}

async function poll(ms = 2_000) {
  await vi.advanceTimersByTimeAsync(ms)
  await flushPromises()
}

afterEach(() => {
  vi.useRealTimers()
  toastSpy.add.mockClear()
  confirmSpy.require.mockClear()
})

describe('creating networks', () => {
  it('creates and starts an official network, then selects it', async () => {
    const { api, entry } = legacy(revisionApi())
    api.list_network_instance_ids.mockResolvedValue({ disabled_inst_ids: [], running_inst_ids: [], runtime_capabilities: [] })
    api.run_network.mockImplementation(async () => {
      api.list_network_instance_ids.mockResolvedValue({ disabled_inst_ids: [], running_inst_ids: [INSTANCE_UUID], runtime_capabilities: [] })
    })
    const wrapper = await mountManagement(api, null, () => cloneConfig(entry.config))
    try {
      await button(wrapper, 'web.device_management.create_network').trigger('click')
      await flushPromises()
      expect(form(wrapper).props('actionLabel')).toBe('web.local_configs.create_and_start')
      expect(api.save_config).not.toHaveBeenCalled()
      form(wrapper).props('curNetwork').hostname = 'new-host'
      await submit(wrapper)
      expect(api.run_network).toHaveBeenCalledWith(expect.objectContaining({ hostname: 'new-host' }), true)
      expect(wrapper.emitted('update:instanceId')).toEqual([[INSTANCE_ID]])
      expect(wrapper.emitted('update')).toHaveLength(1)
    } finally { wrapper.unmount() }
  })

  it('reports a failed creation and keeps the draft', async () => {
    const { api, entry } = legacy(revisionApi())
    api.list_network_instance_ids.mockResolvedValue({ disabled_inst_ids: [], running_inst_ids: [], runtime_capabilities: [] })
    api.run_network.mockRejectedValue({ response: { data: { message: 'config file config.d/x.toml is read-only' } } })
    const wrapper = await mountManagement(api, null, () => cloneConfig(entry.config))
    try {
      await button(wrapper, 'web.device_management.create_network').trigger('click')
      await flushPromises()
      await submit(wrapper)
      const toast = toastSpy.add.mock.calls.at(-1)![0] as { severity: string; detail: string }
      expect(toast.severity).toBe('error')
      expect(toast.detail).toContain('config file config.d/x.toml is read-only')
      expect(wrapper.emitted('update:instanceId')).toBeUndefined()
      expect(form(wrapper).exists()).toBe(true)
    } finally { wrapper.unmount() }
  })

  it('creates a node file with an empty revision and starts it with the created revision', async () => {
    const { api, entry } = revisionApi()
    api.list_network_instance_ids.mockResolvedValue({ disabled_inst_ids: [], running_inst_ids: [], runtime_capabilities: [REVISION] })
    const wrapper = await mountManagement(api, null, () => cloneConfig(entry.config))
    try {
      await button(wrapper, 'web.device_management.create_network').trigger('click')
      await flushPromises()
      expect(api.patch_local_config).not.toHaveBeenCalled()
      await submit(wrapper)
      const request = api.patch_local_config.mock.calls[0][0]
      expect(request).toMatchObject({ inst_id: INSTANCE_ID, expected_revision: '' })
      expect(request).not.toHaveProperty('apply_mode')
      expect(api.update_network_instance_state).toHaveBeenCalledWith(INSTANCE_ID, false, 'revision-b')
    } finally { wrapper.unmount() }
  })
})

describe('creating networks without starting them', () => {
  it('saves an official network without running it', async () => {
    const { api, entry } = legacy(revisionApi())
    api.list_network_instance_ids.mockResolvedValue({ disabled_inst_ids: [], running_inst_ids: [], runtime_capabilities: [] })
    const wrapper = await mountManagement(api, null, () => cloneConfig(entry.config))
    try {
      await button(wrapper, 'web.device_management.create_network').trigger('click')
      await flushPromises()
      await saveConfig(wrapper)
      expect(api.save_config).toHaveBeenCalledOnce()
      expect(api.run_network).not.toHaveBeenCalled()
      expect(wrapper.emitted('update:instanceId')).toEqual([[INSTANCE_ID]])
    } finally { wrapper.unmount() }
  })

  it('creates a disabled node file without starting it', async () => {
    const { api, entry } = revisionApi()
    api.list_network_instance_ids.mockResolvedValue({ disabled_inst_ids: [], running_inst_ids: [], runtime_capabilities: [REVISION] })
    const wrapper = await mountManagement(api, null, () => cloneConfig(entry.config))
    try {
      await button(wrapper, 'web.device_management.create_network').trigger('click')
      await flushPromises()
      await saveConfig(wrapper)
      expect(api.patch_local_config).toHaveBeenCalledWith(expect.objectContaining({ inst_id: INSTANCE_ID, expected_revision: '' }))
      expect(api.update_network_instance_state).not.toHaveBeenCalled()
    } finally { wrapper.unmount() }
  })
})

describe('saving official networks', () => {
  it('saves a stopped network without dropping boolean fields or starting it', async () => {
    const { api, entry } = legacy(revisionApi())
    const expected = snapshotBooleanConfigFields(entry.config)
    const wrapper = await mountManagement(api)
    try {
      expect(form(wrapper).props('actionLabel')).toBe('web.local_configs.apply')
      expect(button(wrapper, 'web.device_management.save_config').attributes('disabled')).toBeDefined()
      form(wrapper).props('curNetwork').hostname = 'saved-host'
      await nextTick()
      await saveConfig(wrapper)
      expect(api.save_config).toHaveBeenCalledOnce()
      const saved = api.save_config.mock.calls[0][0] as NetworkConfig
      expect(saved.hostname).toBe('saved-host')
      for (const field of BOOLEAN_CONFIG_FIELDS) expect(saved[field], field).toBe(expected[field])
      expect(api.update_network_instance_state).not.toHaveBeenCalled()
      expect(api.run_network).not.toHaveBeenCalled()
      expect(form(wrapper).exists()).toBe(true)
    } finally { wrapper.unmount() }
  })

  it('applies a stopped network by saving it and then starting it', async () => {
    const { api } = legacy(revisionApi())
    const wrapper = await mountManagement(api)
    try {
      form(wrapper).props('curNetwork').hostname = 'applied-host'
      await submit(wrapper)
      expect(api.save_config).toHaveBeenCalledWith(expect.objectContaining({ hostname: 'applied-host' }))
      expect(api.update_network_instance_state).toHaveBeenCalledWith(INSTANCE_ID, false)
      expect(api.save_config.mock.invocationCallOrder[0]).toBeLessThan(api.update_network_instance_state.mock.invocationCallOrder[0])
      expect(toastSpy.add).toHaveBeenCalledWith(expect.objectContaining({ detail: 'web.local_configs.saved_applied' }))
    } finally { wrapper.unmount() }
  })

  it('applies a running network through run_network and returns to its status', async () => {
    const { api } = legacy(running(revisionApi()))
    const wrapper = await mountManagement(api)
    try {
      const editor = await editRunning(wrapper)
      editor.props('curNetwork').hostname = 'applied-host'
      await submit(wrapper)
      expect(api.run_network).toHaveBeenCalledWith(expect.objectContaining({ hostname: 'applied-host' }), true)
      expect(api.save_config).not.toHaveBeenCalled()
      expect(wrapper.find('.network-status-container').exists()).toBe(true)
    } finally { wrapper.unmount() }
  })
})

describe('saving node configuration files', () => {
  it.each([undefined, null])('edits and deletes a valid entry after an invalid entry with UUID %s', async instId => {
    const { api, snapshot } = revisionApi()
    snapshot.entries.unshift({ entry_key: 'broken.toml', inst_id: instId, status: 'invalid',
      revision: '', network_name: '', config_permission: 0 } as any)
    const wrapper = await mountManagement(api)
    try {
      expect(form(wrapper).props('curNetwork').hostname).toBe('original-host')
      form(wrapper).props('curNetwork').hostname = 'edited-valid-host'
      await nextTick()
      await saveConfig(wrapper)
      expect(api.patch_local_config).toHaveBeenCalledWith({
        inst_id: INSTANCE_ID, expected_revision: 'revision-a', field_mask: ['hostname'],
        config: { hostname: 'edited-valid-host' },
      })
      expect(api.update_network_instance_state).not.toHaveBeenCalled()

      const actions = wrapper.findComponent({ name: 'Menu' }).props('model')
      actions[2].command()
      await confirmSpy.require.mock.calls[0][0].accept()
      expect(api.delete_network).toHaveBeenCalledWith(INSTANCE_ID, 'revision-a')
      expect(toastSpy.add.mock.calls.filter(call => call[0].severity === 'error')).toEqual([])
    } finally { wrapper.unmount() }
  })

  it('applies a stopped node file by patching it and then starting it', async () => {
    const { api } = revisionApi()
    const wrapper = await mountManagement(api)
    try {
      form(wrapper).props('curNetwork').hostname = 'applied-host'
      await submit(wrapper)
      expect(api.patch_local_config).toHaveBeenCalledOnce()
      expect(api.update_network_instance_state).toHaveBeenCalledWith(INSTANCE_ID, false, 'revision-a')
      expect(api.patch_local_config.mock.invocationCallOrder[0]).toBeLessThan(api.update_network_instance_state.mock.invocationCallOrder[0])
    } finally { wrapper.unmount() }
  })

  it('saves a running network once and returns to its status', async () => {
    const { api } = running(revisionApi())
    const wrapper = await mountManagement(api)
    try {
      const editor = await editRunning(wrapper)
      editor.props('curNetwork').hostname = 'applied-host'
      await submit(wrapper)
      expect(api.patch_local_config).toHaveBeenCalledOnce()
      expect(api.patch_local_config.mock.calls[0][0].field_mask).toEqual(['hostname'])
      expect(toastSpy.add).toHaveBeenCalledWith(expect.objectContaining({ detail: 'web.local_configs.saved_applied' }))
      expect(wrapper.find('.network-status-container').exists()).toBe(true)
    } finally { wrapper.unmount() }
  })

  it('saves a running network without stopping it from the save action', async () => {
    const { api } = running(revisionApi())
    const wrapper = await mountManagement(api)
    try {
      const editor = await editRunning(wrapper)
      editor.props('curNetwork').hostname = 'saved-running-host'
      await nextTick()
      await saveConfig(wrapper)
      expect(api.patch_local_config).toHaveBeenCalledOnce()
      expect(api.update_network_instance_state).not.toHaveBeenCalled()
      expect(wrapper.find('.network-status-container').exists()).toBe(true)
    } finally { wrapper.unmount() }
  })

  it('keeps the edits when the network is stopped elsewhere', async () => {
    const state = running(revisionApi())
    const { api } = state
    const wrapper = await mountManagement(api)
    try {
      const editor = await editRunning(wrapper)
      editor.props('curNetwork').hostname = 'unsaved'
      api.list_network_instance_ids.mockResolvedValue({ running_inst_ids: [], disabled_inst_ids: [INSTANCE_UUID],
        runtime_capabilities: state.snapshot.capabilities })
      await poll()
      expect(form(wrapper).props('curNetwork').hostname).toBe('unsaved')
      expect(button(wrapper, 'web.local_configs.enable').attributes('disabled')).toBeDefined()
    } finally { wrapper.unmount() }
  })

  it('reports a failed apply and keeps the edits', async () => {
    const { api } = running(revisionApi())
    api.patch_local_config.mockResolvedValue({ status: 5, message: 'tun unavailable' } as any)
    const wrapper = await mountManagement(api)
    try {
      const editor = await editRunning(wrapper)
      editor.props('curNetwork').hostname = 'rejected-host'
      await submit(wrapper)
      expect(toastSpy.add.mock.calls.at(-1)![0].detail).toContain('web.local_configs.apply_failed')
      expect(toastSpy.add.mock.calls.at(-1)![0].detail).toContain('tun unavailable')
      expect(form(wrapper).props('curNetwork').hostname).toBe('rejected-host')
    } finally { wrapper.unmount() }
  })

  it('follows changes made elsewhere while the form is unchanged', async () => {
    const { api, entry } = revisionApi()
    const wrapper = await mountManagement(api)
    try {
      entry.revision = 'revision-local-edit'
      entry.config.hostname = 'edited-on-node'
      await poll()
      expect(form(wrapper).props('curNetwork').hostname).toBe('edited-on-node')
      expect(wrapper.text()).not.toContain('web.local_configs.merged_elsewhere')
    } finally { wrapper.unmount() }
  })

  it('merges changes made elsewhere to other fields into the edits', async () => {
    const { api, entry } = revisionApi()
    const wrapper = await mountManagement(api)
    try {
      form(wrapper).props('curNetwork').hostname = 'mine'
      entry.revision = 'revision-other-console'
      entry.config.network_name = 'renamed-elsewhere'
      await poll()
      const draft = form(wrapper).props('curNetwork')
      expect(draft.hostname).toBe('mine')
      expect(draft.network_name).toBe('renamed-elsewhere')
      expect(wrapper.text()).toContain('web.local_configs.merged_elsewhere')
      await submit(wrapper)
      expect(api.patch_local_config).toHaveBeenCalledWith(expect.objectContaining({
        expected_revision: 'revision-other-console', field_mask: ['hostname'],
      }))
    } finally { wrapper.unmount() }
  })

  it.each(['keep_mine', 'take_theirs'])('resolves overlapping changes with %s', async choice => {
    const { api, entry } = revisionApi()
    const wrapper = await mountManagement(api)
    try {
      form(wrapper).props('curNetwork').hostname = 'mine'
      entry.revision = 'revision-other-console'
      entry.config.hostname = 'theirs'
      await poll()
      expect(wrapper.text()).toContain('web.local_configs.conflict_fields')
      expect(wrapper.text()).toContain('hostname')
      expect(form(wrapper).props('configInvalid')).toBe(true)
      expect(form(wrapper).props('curNetwork').hostname).toBe('mine')

      await button(wrapper, `web.local_configs.${choice}`).trigger('click')
      await flushPromises()
      expect(wrapper.text()).not.toContain('web.local_configs.conflict_fields')
      expect(form(wrapper).props('curNetwork').hostname).toBe(choice === 'keep_mine' ? 'mine' : 'theirs')
      await submit(wrapper)
      if (choice === 'keep_mine') {
        expect(api.patch_local_config).toHaveBeenCalledWith(expect.objectContaining({
          expected_revision: 'revision-other-console', field_mask: ['hostname'], config: { hostname: 'mine' },
        }))
      } else {
        expect(api.patch_local_config).not.toHaveBeenCalled()
      }
    } finally { wrapper.unmount() }
  })

  it('surfaces a save conflict without resubmitting', async () => {
    const { api, entry } = revisionApi()
    const wrapper = await mountManagement(api)
    try {
      form(wrapper).props('curNetwork').hostname = 'mine'
      api.patch_local_config.mockImplementationOnce(async () => {
        entry.revision = 'revision-raced'
        entry.config.hostname = 'theirs'
        return { status: 1 } as any
      })
      await submit(wrapper)
      expect(api.patch_local_config).toHaveBeenCalledOnce()
      expect(toastSpy.add).toHaveBeenCalledWith(expect.objectContaining({ detail: 'web.local_configs.changed_elsewhere' }))
      expect(wrapper.text()).toContain('web.local_configs.conflict_fields')
      expect(form(wrapper).props('curNetwork').hostname).toBe('mine')
    } finally { wrapper.unmount() }
  })

  it('enables a stopped network only when the form has no unsaved changes', async () => {
    const { api, entry } = revisionApi()
    const wrapper = await mountManagement(api)
    try {
      form(wrapper).props('curNetwork').hostname = 'unsaved'
      await nextTick()
      expect(button(wrapper, 'web.local_configs.enable').attributes('disabled')).toBeDefined()
      await button(wrapper, 'web.local_configs.discard').trigger('click')
      await nextTick()
      expect(form(wrapper).props('curNetwork').hostname).toBe('original-host')
      entry.revision = 'revision-latest'
      await button(wrapper, 'web.local_configs.enable').trigger('click')
      await flushPromises()
      expect(api.update_network_instance_state).toHaveBeenCalledWith(INSTANCE_ID, false, 'revision-latest')
    } finally { wrapper.unmount() }
  })

  it('edits the original node file and saves it as a whole document', async () => {
    const { api, entry } = revisionApi()
    const wrapper = await mountManagement(api)
    try {
      const dialog = wrapper.findComponent(ConfigFileDialog)
      expect(await dialog.props('generateConfig')(form(wrapper).props('curNetwork'))).toBe(entry.persisted_toml)
      expect(api.generate_config).not.toHaveBeenCalled()
      const edited = entry.persisted_toml.replace('original-host', 'file-host')
      api.patch_local_config.mockResolvedValueOnce({ status: 0, entry: { ...entry, revision: 'revision-file',
        persisted_toml: edited, config: { ...entry.config, hostname: 'file-host' } } } as any)
      await dialog.props('saveConfig')(edited)
      await flushPromises()
      expect(api.patch_local_config).toHaveBeenCalledWith({
        inst_id: INSTANCE_ID, expected_revision: 'revision-a', field_mask: [], toml: edited,
      })
      expect(form(wrapper).props('curNetwork').hostname).toBe('file-host')

      form(wrapper).props('curNetwork').hostname = 'unsaved'
      await nextTick()
      expect(button(wrapper, 'web.device_management.edit_as_file').attributes('disabled')).toBeDefined()
    } finally { wrapper.unmount() }
  })

  it('reports a file changed elsewhere to the file editor', async () => {
    const { api, entry } = revisionApi()
    api.patch_local_config.mockResolvedValueOnce({ status: 1 } as any)
    const wrapper = await mountManagement(api)
    try {
      const dialog = wrapper.findComponent(ConfigFileDialog)
      await expect(dialog.props('saveConfig')(entry.persisted_toml + '# edit\n'))
        .rejects.toThrow('web.local_configs.file_changed_elsewhere')
    } finally { wrapper.unmount() }
  })

  it('keeps option order, selection and the form stable across shuffled catalog polls', async () => {
    const { api, snapshot, entry } = revisionApi()
    const nextId = '00000000-0000-0000-0000-000000000002'
    const nextUuid = { ...INSTANCE_UUID, part4: 2 }
    snapshot.entries.push({ ...entry, inst_id: nextId, entry_key: 'other.toml',
      config: { ...entry.config, instance_id: nextId } })
    let backwards = true
    api.list_network_instance_ids.mockImplementation(async () => {
      backwards = !backwards
      return { running_inst_ids: [], disabled_inst_ids: backwards ? [nextUuid, INSTANCE_UUID] : [INSTANCE_UUID, nextUuid],
        runtime_capabilities: snapshot.capabilities }
    })
    const wrapper = await mountManagement(api)
    try {
      const select = wrapper.findComponent({ name: 'Select' })
      const options = select.props('options')
      const selected = select.props('modelValue')
      const config = form(wrapper).props('curNetwork')
      expect(options.map((option: any) => option.uuid)).toEqual([INSTANCE_ID, nextId])
      await poll(3_000)
      expect(select.props('options')).toBe(options)
      expect(select.props('modelValue')).toBe(selected)
      expect(form(wrapper).props('curNetwork')).toBe(config)
      expect(wrapper.emitted('update:instanceId')).toBeUndefined()
    } finally { wrapper.unmount() }
  })
})
