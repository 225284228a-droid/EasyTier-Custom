import { flushPromises, mount } from '@vue/test-utils'
import { describe, expect, it, vi } from 'vitest'
import { defineComponent, nextTick } from 'vue'
import RemoteManagement from '../src/components/RemoteManagement.vue'
import { LOCAL_CONFIG_APPLY_CAPABILITY } from '../src/modules/capabilities'
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
  'enable_exit_node',
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
  'ipv6_public_addr_provider',
  'ipv6_public_addr_auto',
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
    useConfirm: () => ({ require: vi.fn() }),
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

async function settleRemoteManagement() {
  for (let i = 0; i < 3; i++) {
    await new Promise((resolve) => setTimeout(resolve, 0))
    await flushPromises()
    await nextTick()
  }
}

describe('RemoteManagement config save', () => {
  it('emits a string instance id when creating a network', async () => {
    const config = {
      ...DEFAULT_NETWORK_CONFIG(),
      instance_id: INSTANCE_ID,
    }
    const api = {
      delete_network: vi.fn(),
      generate_config: vi.fn(),
      get_network_config: vi.fn(async () => cloneConfig(config)),
      get_network_info: vi.fn(),
      get_vpn_portal_info: vi.fn(),
      get_network_metas: vi.fn(async () => ({ metas: {} })),
      list_network_instance_ids: vi.fn()
        .mockResolvedValueOnce({ disabled_inst_ids: [], running_inst_ids: [] })
        .mockResolvedValue({ disabled_inst_ids: [INSTANCE_UUID], running_inst_ids: [] }),
      parse_config: vi.fn(),
      run_network: vi.fn(),
      save_config: vi.fn(async () => undefined),
      update_network_instance_state: vi.fn(),
      validate_config: vi.fn(),
    }

    const wrapper = mount(RemoteManagement, {
      props: {
        api,
        newConfigGenerator: () => config,
      },
      global: {
        stubs: {
          Config: true,
          ConfigEditDialog: true,
          Status: true,
        },
      },
    })

    try {
      await settleRemoteManagement()

      await wrapper.find('button[data-label="web.device_management.create_network"]').trigger('click')
      await flushPromises()

      expect(api.save_config).toHaveBeenCalledOnce()
      expect(wrapper.emitted('update:instanceId')).toEqual([[INSTANCE_ID]])
    } finally {
      wrapper.unmount()
    }
  })

  it('reports a failed network creation instead of doing nothing', async () => {
    const config = {
      ...DEFAULT_NETWORK_CONFIG(),
      instance_id: INSTANCE_ID,
    }
    const api = {
      delete_network: vi.fn(),
      generate_config: vi.fn(),
      get_network_config: vi.fn(),
      get_network_info: vi.fn(),
      get_vpn_portal_info: vi.fn(),
      get_network_metas: vi.fn(async () => ({ metas: {} })),
      list_network_instance_ids: vi.fn(async () => ({ disabled_inst_ids: [], running_inst_ids: [] })),
      parse_config: vi.fn(),
      run_network: vi.fn(),
      save_config: vi.fn(async () => {
        throw { response: { data: { message: 'config file config.d/x.toml is read-only' } } }
      }),
      update_network_instance_state: vi.fn(),
      validate_config: vi.fn(),
    }

    const wrapper = mount(RemoteManagement, {
      props: {
        api,
        newConfigGenerator: () => config,
      },
      global: {
        stubs: {
          Config: true,
          ConfigEditDialog: true,
          Status: true,
        },
      },
    })

    try {
      await settleRemoteManagement()
      toastSpy.add.mockClear()

      await wrapper.find('button[data-label="web.device_management.create_network"]').trigger('click')
      await flushPromises()

      expect(toastSpy.add).toHaveBeenCalledTimes(1)
      const toast = toastSpy.add.mock.calls[0][0] as { severity: string; detail: string }
      expect(toast.severity).toBe('error')
      expect(toast.detail).toContain('config file config.d/x.toml is read-only')
      expect(wrapper.emitted('update:instanceId')).toBeUndefined()
    } finally {
      wrapper.unmount()
    }
  })

  it('saves the current network config without dropping boolean fields', async () => {
    const config = makeFlagConfig()
    const expectedFlags = snapshotBooleanConfigFields(config)
    const api = {
      delete_network: vi.fn(),
      generate_config: vi.fn(),
      get_network_config: vi.fn(async () => cloneConfig(config)),
      get_network_info: vi.fn(),
      get_vpn_portal_info: vi.fn(),
      get_network_metas: vi.fn(async (instanceIds: string[]) => ({
        metas: Object.fromEntries(instanceIds.map((id) => [id, {
          config_permission: 0xffffffff,
          inst_id: INSTANCE_UUID,
          instance_name: 'mesh-save',
          network_name: 'mesh-save',
          source: 2,
        }])),
      })),
      list_network_instance_ids: vi.fn(async () => ({
        disabled_inst_ids: [INSTANCE_UUID],
        running_inst_ids: [],
      })),
      parse_config: vi.fn(),
      run_network: vi.fn(),
      save_config: vi.fn(async () => undefined),
      update_network_instance_state: vi.fn(),
      validate_config: vi.fn(),
    }

    const wrapper = mount(RemoteManagement, {
      props: {
        api,
        instanceId: INSTANCE_ID,
      },
      global: {
        stubs: {
          Config: true,
          ConfigEditDialog: true,
          Status: true,
        },
      },
    })

    try {
      await settleRemoteManagement()

      const saveButton = wrapper.find('button[data-label="web.device_management.save_config"]')
      expect(saveButton.exists()).toBe(true)
      expect(saveButton.attributes('disabled')).toBeUndefined()

      await saveButton.trigger('click')
      await flushPromises()

      expect(api.save_config).toHaveBeenCalledOnce()
      const savedConfig = api.save_config.mock.calls[0][0] as NetworkConfig

      for (const field of BOOLEAN_CONFIG_FIELDS) {
        expect(savedConfig[field], `${field} should be saved`).toBe(expectedFlags[field])
      }
    } finally {
      wrapper.unmount()
    }
  })
})

const RevisionConfigForm = defineComponent({
  name: 'Config', props: ['curNetwork', 'actionLabel', 'configInvalid'], emits: ['runNetwork'], template: '<div class="revision-config-form" />',
})
function revisionApi() {
  const config = { ...DEFAULT_NETWORK_CONFIG(), instance_id: INSTANCE_ID, hostname: 'original-host' }
  const entry = { entry_key: 'mesh.toml', inst_id: INSTANCE_ID, revision: 'revision-a', config,
    config_permission: 0, enabled: false, running: false, persisted_raw_hash: 'a', pending_apply: false,
    status: 'ready', network_name: 'mesh', source: 1 }
  const snapshot = { catalog_epoch: 'boot-a', catalog_generation: 1, entries: [entry], online: true,
    capabilities: ['management:persisted-config-revision-v1'] }
  return {
    entry, snapshot,
    api: {
      scope: 'connection-a',
      list_network_instance_ids: vi.fn(async () => ({ disabled_inst_ids: [INSTANCE_UUID], running_inst_ids: [],
        runtime_capabilities: snapshot.capabilities })),
      get_network_metas: vi.fn(async () => ({ metas: { [INSTANCE_ID]: { network_name: 'mesh', config_permission: 0 } } })),
      get_network_config: vi.fn(async () => config), get_network_info: vi.fn(), get_vpn_portal_info: vi.fn(),
      observe_local_configs: vi.fn(async () => snapshot),
      patch_local_config: vi.fn(async () => ({ status: 0, entry: { ...entry, revision: 'revision-b' } })),
      save_config: vi.fn(), run_network: vi.fn(), delete_network: vi.fn(), update_network_instance_state: vi.fn(),
      validate_config: vi.fn(), generate_config: vi.fn(), parse_config: vi.fn(),
    },
  }
}
async function openRevision(api: any, instanceId: string | null = INSTANCE_ID, newConfigGenerator?: () => NetworkConfig) {
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
  const wrapper = mount(RemoteManagement, { props: { api, instanceId: instanceId ?? undefined, newConfigGenerator }, global: {
    stubs: { Config: RevisionConfigForm, ConfigEditDialog: true, Status: true },
  } })
  await vi.advanceTimersByTimeAsync(1)
  await flushPromises()
  return wrapper
}

function runningRevision(pendingApply = false, applySupported = true) {
  const state = revisionApi()
  Object.assign(state.entry, { enabled: true, running: true, pending_apply: pendingApply })
  if (applySupported) state.snapshot.capabilities.push(LOCAL_CONFIG_APPLY_CAPABILITY)
  state.api.list_network_instance_ids.mockResolvedValue({ running_inst_ids: [INSTANCE_UUID], disabled_inst_ids: [],
    runtime_capabilities: state.snapshot.capabilities })
  state.api.get_network_info.mockResolvedValue({ running: true })
  return state
}

async function editRunning(wrapper: Awaited<ReturnType<typeof openRevision>>) {
  const actions = wrapper.findComponent({ name: 'Menu' }).props('model')
  await actions[0].command()
  await flushPromises()
  return wrapper.findComponent(RevisionConfigForm)
}

function legacyApi(running: boolean) {
  const state = revisionApi()
  state.snapshot.capabilities.length = 0
  state.api.list_network_instance_ids.mockImplementation(async () => ({
    running_inst_ids: running ? [INSTANCE_UUID] : [],
    disabled_inst_ids: running ? [] : [INSTANCE_UUID],
    runtime_capabilities: [],
  }))
  state.api.get_network_info.mockResolvedValue({ running: true })
  state.api.run_network.mockImplementation(async () => { running = true })
  state.api.update_network_instance_state.mockImplementation(async () => { running = true })
  return state
}

describe('legacy RemoteManagement save and run', () => {
  it('creates, selects, and runs a network when no instance was initially selected', async () => {
    const { api, entry } = legacyApi(false)
    api.list_network_instance_ids.mockResolvedValue({ running_inst_ids: [], disabled_inst_ids: [], runtime_capabilities: [] })
    api.save_config.mockImplementation(async () => {
      api.list_network_instance_ids.mockResolvedValue({ running_inst_ids: [], disabled_inst_ids: [INSTANCE_UUID], runtime_capabilities: [] })
    })
    api.update_network_instance_state.mockImplementation(async () => {
      api.list_network_instance_ids.mockResolvedValue({ running_inst_ids: [INSTANCE_UUID], disabled_inst_ids: [], runtime_capabilities: [] })
    })
    const wrapper = await openRevision(api, null, () => entry.config)
    try {
      await wrapper.find('button[data-label="web.device_management.create_network"]').trigger('click')
      await flushPromises()
      expect(wrapper.emitted('update:instanceId')).toEqual([[INSTANCE_ID]])
      const form = wrapper.findComponent(RevisionConfigForm)
      form.props('curNetwork').hostname = 'new-network-host'
      form.vm.$emit('runNetwork', form.props('curNetwork'))
      await flushPromises()

      expect(api.save_config).toHaveBeenCalledTimes(2)
      expect(api.update_network_instance_state).toHaveBeenCalledWith(INSTANCE_ID, false)
      expect(wrapper.find('.network-status-container').exists()).toBe(true)
      expect(wrapper.findComponent(RevisionConfigForm).exists()).toBe(false)
      expect(wrapper.emitted('update')).toHaveLength(1)
    } finally { wrapper.unmount(); vi.useRealTimers() }
  })

  it.each([false, true])('returns to network status after applying edits (initially running: %s)', async running => {
    const { api } = legacyApi(running)
    const wrapper = await openRevision(api)
    try {
      const form = running ? await editRunning(wrapper) : wrapper.findComponent(RevisionConfigForm)
      form.props('curNetwork').hostname = 'applied-host'
      await nextTick()
      expect(wrapper.text()).toContain('web.local_configs.dirty_preserved')

      form.vm.$emit('runNetwork', form.props('curNetwork'))
      await flushPromises()

      if (running) {
        expect(api.run_network).toHaveBeenCalledWith(expect.objectContaining({ hostname: 'applied-host' }), true)
        expect(api.save_config).not.toHaveBeenCalled()
      } else {
        expect(api.save_config).toHaveBeenCalledWith(expect.objectContaining({ hostname: 'applied-host' }))
        expect(api.update_network_instance_state).toHaveBeenCalledWith(INSTANCE_ID, false)
        expect(api.run_network).not.toHaveBeenCalled()
      }
      expect(wrapper.find('.network-status-container').exists()).toBe(true)
      expect(wrapper.findComponent(RevisionConfigForm).exists()).toBe(false)
      expect(wrapper.text()).not.toContain('web.local_configs.dirty_preserved')
      expect(wrapper.emitted('update')).toHaveLength(1)
    } finally { wrapper.unmount(); vi.useRealTimers() }
  })

  it.each(['save_config', 'update_network_instance_state', 'run_network'] as const)(
    'keeps the dirty form when %s fails', async operation => {
      const running = operation === 'run_network'
      const { api } = legacyApi(running)
      api[operation].mockRejectedValue(new Error('request failed'))
      const wrapper = await openRevision(api)
      try {
        const form = running ? await editRunning(wrapper) : wrapper.findComponent(RevisionConfigForm)
        form.props('curNetwork').hostname = 'unsaved-host'
        form.vm.$emit('runNetwork', form.props('curNetwork'))
        await flushPromises()
        await vi.advanceTimersByTimeAsync(1_000)

        expect(wrapper.findComponent(RevisionConfigForm).props('curNetwork').hostname).toBe('unsaved-host')
        expect(wrapper.text()).toContain('web.local_configs.dirty_preserved')
        expect(wrapper.find('.network-status-container').exists()).toBe(false)
        expect(wrapper.emitted('update')).toBeUndefined()
        if (operation === 'save_config') expect(api.update_network_instance_state).not.toHaveBeenCalled()
      } finally { wrapper.unmount(); vi.useRealTimers() }
    },
  )

  it.each([false, true])('retains edits made while the run request is pending (initially running: %s)', async running => {
    const { api } = legacyApi(running)
    let finish!: () => void
    const operation = running ? api.run_network : api.update_network_instance_state
    operation.mockImplementation(() => new Promise<void>(resolve => { finish = resolve }))
    const wrapper = await openRevision(api)
    try {
      const form = running ? await editRunning(wrapper) : wrapper.findComponent(RevisionConfigForm)
      form.props('curNetwork').hostname = 'submitted-host'
      form.vm.$emit('runNetwork', form.props('curNetwork'))
      await flushPromises()
      form.props('curNetwork').hostname = 'newer-edit'
      api.list_network_instance_ids.mockResolvedValue({ running_inst_ids: [INSTANCE_UUID], disabled_inst_ids: [], runtime_capabilities: [] })
      finish()
      await flushPromises()

      expect(wrapper.findComponent(RevisionConfigForm).props('curNetwork').hostname).toBe('newer-edit')
      expect(wrapper.text()).toContain('web.local_configs.dirty_preserved')
      expect(wrapper.find('.network-status-container').exists()).toBe(false)
      const submitted = (running ? api.run_network : api.save_config).mock.calls[0][0]
      expect(submitted.hostname).toBe('submitted-host')
    } finally { wrapper.unmount(); vi.useRealTimers() }
  })

  it.each(['save_config', 'run_network', 'get_network_metas'] as const)(
    'does not restore the previous selection after a delayed %s response', async operation => {
      const running = operation !== 'save_config'
      const { api, entry } = legacyApi(running)
      const nextId = '00000000-0000-0000-0000-000000000002'
      const nextUuid = { ...INSTANCE_UUID, part4: 2 }
      api.list_network_instance_ids.mockResolvedValue({
        running_inst_ids: running ? [INSTANCE_UUID] : [],
        disabled_inst_ids: running ? [nextUuid] : [INSTANCE_UUID, nextUuid],
        runtime_capabilities: [],
      })
      api.get_network_config.mockImplementation(async (...args: string[]) => ({ ...entry.config, instance_id: args[0] }))
      const wrapper = await openRevision(api)
      try {
        const form = running ? await editRunning(wrapper) : wrapper.findComponent(RevisionConfigForm)
        form.props('curNetwork').hostname = 'submitted-host'
        let finish!: () => void
        api[operation].mockImplementationOnce(() => new Promise<any>(resolve => {
          finish = () => resolve(operation === 'get_network_metas' ? { metas: {} } : undefined)
        }))
        form.vm.$emit('runNetwork', form.props('curNetwork'))
        await flushPromises()

        await wrapper.setProps({ instanceId: nextId })
        await flushPromises()
        const nextForm = wrapper.findComponent(RevisionConfigForm)
        nextForm.props('curNetwork').hostname = 'new-network-edit'
        finish()
        await flushPromises()

        expect(wrapper.findComponent({ name: 'Select' }).props('modelValue').uuid).toBe(nextId)
        expect(nextForm.props('curNetwork').hostname).toBe('new-network-edit')
        expect(nextForm.props('configInvalid')).toBe(false)
        expect(wrapper.text()).toContain('web.local_configs.dirty_preserved')
        expect(wrapper.emitted('update:instanceId')).toBeUndefined()
        expect(wrapper.emitted('update')).toBeUndefined()
        if (operation === 'save_config') expect(api.update_network_instance_state).not.toHaveBeenCalled()
      } finally { wrapper.unmount(); vi.useRealTimers() }
    },
  )
})

describe('revision-aware RemoteManagement forms', () => {
  it.each(['success', 'failure'] as const)('spins during reread, prevents duplicate requests, and resets after %s', async outcome => {
    const { api, entry, snapshot } = revisionApi()
    const wrapper = await openRevision(api)
    try {
      let resolve!: (value: typeof snapshot) => void
      let reject!: (error: unknown) => void
      api.observe_local_configs.mockClear().mockImplementationOnce(() => new Promise((done, fail) => {
        resolve = done
        reject = fail
      }))
      toastSpy.add.mockClear()
      const button = wrapper.findAllComponents({ name: 'Button' })
        .find(component => component.props('label') === 'web.local_configs.reread')!
      const saveButton = wrapper.findAllComponents({ name: 'Button' })
        .find(component => component.props('label') === 'web.device_management.save_config')!
      const form = wrapper.findComponent(RevisionConfigForm)
      form.props('curNetwork').hostname = 'unsaved-host'
      expect(button.props('loading')).toBe(false)
      button.vm.$emit('click')
      button.vm.$emit('click')
      await nextTick()
      expect(api.observe_local_configs).toHaveBeenCalledOnce()
      expect(button.props('loading')).toBe(true)
      expect(button.props('disabled')).toBe(true)
      expect(button.props('loadingIcon')).toBe('pi pi-refresh pi-spin')
      expect(saveButton.props('disabled')).toBe(true)
      expect(form.props('configInvalid')).toBe(true)
      saveButton.vm.$emit('click')
      form.vm.$emit('runNetwork')
      await flushPromises()
      expect(api.patch_local_config).not.toHaveBeenCalled()
      expect(api.save_config).not.toHaveBeenCalled()
      expect(api.update_network_instance_state).not.toHaveBeenCalled()
      expect(api.run_network).not.toHaveBeenCalled()
      if (outcome === 'success') {
        entry.config.hostname = 'reread-host'
        resolve(snapshot)
      } else {
        reject({ message: 'reread failed' })
      }
      await flushPromises()
      expect(button.props('loading')).toBe(false)
      expect(button.props('disabled')).toBe(false)
      expect(saveButton.props('disabled')).toBe(false)
      expect(form.props('configInvalid')).toBe(false)
      if (outcome === 'success') {
        expect(form.props('curNetwork').hostname).toBe('unsaved-host')
        expect(toastSpy.add).not.toHaveBeenCalled()
      } else {
        expect(toastSpy.add).toHaveBeenCalledWith(expect.objectContaining({ severity: 'warn', detail: 'reread failed' }))
      }
    } finally { wrapper.unmount(); vi.useRealTimers() }
  })

  it('ignores a stale reread failure after changing the selected configuration', async () => {
    const { api, snapshot } = revisionApi()
    const wrapper = await openRevision(api)
    try {
      let rejectOld!: (error: unknown) => void
      api.observe_local_configs.mockImplementationOnce(() => new Promise((_, reject) => { rejectOld = reject }))
      await wrapper.find('button[data-label="web.local_configs.reread"]').trigger('click')
      await wrapper.setProps({ instanceId: undefined })
      await wrapper.setProps({ instanceId: INSTANCE_ID })
      await flushPromises()

      let resolveCurrent!: (value: typeof snapshot) => void
      api.observe_local_configs.mockImplementationOnce(() => new Promise(resolve => { resolveCurrent = resolve }))
      const button = wrapper.findAllComponents({ name: 'Button' })
        .find(component => component.props('label') === 'web.local_configs.reread')!
      await button.trigger('click')
      expect(button.props('loading')).toBe(true)
      toastSpy.add.mockClear()
      rejectOld({ message: 'stale reread failed' })
      await flushPromises()
      expect(toastSpy.add).not.toHaveBeenCalled()
      expect(button.props('loading')).toBe(true)
      resolveCurrent(snapshot)
      await flushPromises()
      expect(button.props('loading')).toBe(false)
    } finally { wrapper.unmount(); vi.useRealTimers() }
  })

  it('applies persisted changes after a running configuration is saved without further edits', async () => {
    const { api, entry } = runningRevision()
    api.patch_local_config.mockImplementation(async (...args: any[]) => {
      const request = args[0]
      Object.assign(entry.config, request.config)
      entry.pending_apply = request.apply_mode === 1
      entry.revision = entry.pending_apply ? 'saved-revision' : 'applied-revision'
      return { status: 0, entry: { ...entry } }
    })
    const wrapper = await openRevision(api)
    try {
      const form = await editRunning(wrapper)
      form.props('curNetwork').hostname = 'persisted-host'
      await wrapper.find('button[data-label="web.device_management.save_config"]').trigger('click')
      await flushPromises()
      expect(entry.pending_apply).toBe(true)
      expect(form.props('actionLabel')).toBe('web.local_configs.apply_saved')
      expect(wrapper.text()).toContain('web.local_configs.pending_apply')
      form.vm.$emit('runNetwork')
      await flushPromises()
      expect(api.patch_local_config.mock.calls[1][0]).toEqual({ inst_id: INSTANCE_ID, expected_revision: 'saved-revision',
        config: {}, field_mask: [], apply_mode: 0 })
      expect(entry.pending_apply).toBe(false)
      expect(api.update_network_instance_state).not.toHaveBeenCalled()
      expect(api.run_network).not.toHaveBeenCalled()
    } finally { wrapper.unmount(); vi.useRealTimers() }
  })

  it('applies the persisted revision after reopening the editor and observing a manual disk edit', async () => {
    const { api, entry } = runningRevision(true)
    const first = await openRevision(api)
    await editRunning(first)
    first.unmount()
    vi.useRealTimers()
    entry.config.hostname = 'manually-edited-on-disk'
    entry.revision = 'disk-revision'
    const wrapper = await openRevision(api)
    try {
      const form = await editRunning(wrapper)
      expect(form.props('curNetwork').hostname).toBe('manually-edited-on-disk')
      form.vm.$emit('runNetwork')
      await flushPromises()
      expect(api.patch_local_config.mock.calls[0][0]).toEqual({ inst_id: INSTANCE_ID, expected_revision: 'disk-revision',
        config: {}, field_mask: [], apply_mode: 0 })
    } finally { wrapper.unmount(); vi.useRealTimers() }
  })

  it('keeps pending changes visible and asks to upgrade an older revision-capable node', async () => {
    const { api } = runningRevision(true, false)
    const wrapper = await openRevision(api)
    try {
      const form = await editRunning(wrapper)
      toastSpy.add.mockClear()
      form.vm.$emit('runNetwork')
      await flushPromises()
      expect(api.patch_local_config).not.toHaveBeenCalled()
      expect(wrapper.findComponent(RevisionConfigForm).exists()).toBe(true)
      expect(wrapper.text()).toContain('web.local_configs.apply_unsupported')
      expect(toastSpy.add).toHaveBeenCalledWith(expect.objectContaining({ severity: 'error',
        detail: expect.stringContaining('web.local_configs.apply_unsupported') }))
      expect(wrapper.emitted('update')).toBeUndefined()
    } finally { wrapper.unmount(); vi.useRealTimers() }
  })

  it.each([{ status: 1, message: 'revision conflict' }, { status: 5, message: 'apply failed' }])(
    'retains the pending revision and form after an apply failure: $message', async failure => {
      const { api } = runningRevision(true)
      api.patch_local_config.mockResolvedValue(failure)
      const wrapper = await openRevision(api)
      try {
        const form = await editRunning(wrapper)
        form.vm.$emit('runNetwork')
        await flushPromises()
        await vi.advanceTimersByTimeAsync(1_000)
        expect(api.patch_local_config).toHaveBeenCalledTimes(1)
        expect(api.patch_local_config.mock.calls[0][0].expected_revision).toBe('revision-a')
        expect(wrapper.findComponent(RevisionConfigForm).exists()).toBe(true)
        expect(wrapper.text()).toContain('web.local_configs.pending_apply')
        expect(wrapper.emitted('update')).toBeUndefined()
      } finally { wrapper.unmount(); vi.useRealTimers() }
    },
  )

  it('ignores an outstanding apply response after the management connection changes', async () => {
    const { api, entry } = runningRevision(true)
    let resolve!: (result: any) => void
    api.patch_local_config.mockImplementation(() => new Promise(done => { resolve = done }))
    const wrapper = await openRevision(api)
    try {
      const form = await editRunning(wrapper)
      form.vm.$emit('runNetwork')
      await flushPromises()
      expect(form.props('configInvalid')).toBe(true)
      const next = runningRevision()
      next.api.scope = 'connection-b'
      await wrapper.setProps({ api: next.api })
      resolve({ status: 0, entry: { ...entry, pending_apply: false, revision: 'late-revision' } })
      await flushPromises()
      expect(wrapper.findComponent(RevisionConfigForm).exists()).toBe(false)
      expect(wrapper.text()).not.toContain('web.local_configs.pending_apply')
      expect(wrapper.emitted('update')).toBeUndefined()
      expect(next.api.patch_local_config).not.toHaveBeenCalled()
    } finally { wrapper.unmount(); vi.useRealTimers() }
  })

  it('starts an unchanged disabled configuration through the existing lifecycle CAS', async () => {
    const { api } = revisionApi()
    const wrapper = await openRevision(api)
    try {
      wrapper.findComponent(RevisionConfigForm).vm.$emit('runNetwork')
      await flushPromises()
      expect(api.patch_local_config).not.toHaveBeenCalled()
      expect(api.update_network_instance_state).toHaveBeenCalledWith(INSTANCE_ID, false, 'revision-a')
    } finally { wrapper.unmount(); vi.useRealTimers() }
  })

  it('shows GUI conflict messages on save-and-start and retains the dirty form and revision', async () => {
    const { api } = revisionApi()
    const wrapper = await openRevision(api)
    try {
      toastSpy.add.mockClear()
      api.patch_local_config.mockResolvedValue({ status: 1, message: 'Configuration changed; reread its revision' })
      const form = wrapper.findComponent(RevisionConfigForm)
      form.props('curNetwork').hostname = 'unsaved-host'
      form.vm.$emit('runNetwork')
      await flushPromises()
      expect(toastSpy.add).toHaveBeenCalledWith(expect.objectContaining({
        severity: 'error', detail: 'Failed to run network, error: Configuration changed; reread its revision',
      }))
      expect(form.props('curNetwork').hostname).toBe('unsaved-host')
      form.vm.$emit('runNetwork')
      await flushPromises()
      expect(api.patch_local_config.mock.calls[1][0]).toMatchObject({
        expected_revision: 'revision-a', field_mask: ['hostname'], config: { hostname: 'unsaved-host' },
      })
      expect(api.update_network_instance_state).not.toHaveBeenCalled()
      expect(api.save_config).not.toHaveBeenCalled()
      expect(api.run_network).not.toHaveBeenCalled()
    } finally { wrapper.unmount(); vi.useRealTimers() }
  })

  it('saves dirty fields only and keeps edits/revision on conflict and status refresh', async () => {
    const { api, entry } = revisionApi()
    const wrapper = await openRevision(api)
    try {
      wrapper.findComponent(RevisionConfigForm).props('curNetwork').hostname = 'unsaved-host'
      await nextTick()
      api.patch_local_config.mockRejectedValue({ response: { status: 409, data: { message: 'revision conflict' } } })
      await wrapper.find('button[data-label="web.device_management.save_config"]').trigger('click')
      await flushPromises()
      expect(api.patch_local_config.mock.calls[0][0]).toMatchObject({ expected_revision: 'revision-a', field_mask: ['hostname'], config: { hostname: 'unsaved-host' }, apply_mode: 1 })
      api.list_network_instance_ids.mockResolvedValue({ running_inst_ids: [INSTANCE_UUID], disabled_inst_ids: [], runtime_capabilities: ['management:persisted-config-revision-v1'] })
      await vi.advanceTimersByTimeAsync(1000)
      entry.revision = 'remote-revision'
      entry.config.hostname = 'remote-host'
      api.list_network_instance_ids.mockResolvedValue({ running_inst_ids: [], disabled_inst_ids: [INSTANCE_UUID], runtime_capabilities: ['management:persisted-config-revision-v1'] })
      await vi.advanceTimersByTimeAsync(1000)
      expect(wrapper.findComponent(RevisionConfigForm).props('curNetwork').hostname).toBe('unsaved-host')
      await wrapper.find('button[data-label="web.device_management.save_config"]').trigger('click')
      await flushPromises()
      expect(api.patch_local_config.mock.calls[1][0].expected_revision).toBe('revision-a')
      expect(api.save_config).not.toHaveBeenCalled()
    } finally { wrapper.unmount(); vi.useRealTimers() }
  })

  it('starts a disabled configuration only after patch success with the returned revision', async () => {
    const { api } = revisionApi()
    const wrapper = await openRevision(api)
    try {
      const form = wrapper.findComponent(RevisionConfigForm)
      form.props('curNetwork').hostname = 'saved-host'
      form.vm.$emit('runNetwork')
      await flushPromises()
      expect(api.patch_local_config.mock.calls[0][0].apply_mode).toBe(0)
      expect(api.update_network_instance_state).toHaveBeenCalledWith(INSTANCE_ID, false, 'revision-b')
      expect(api.save_config).not.toHaveBeenCalled()
      expect(api.run_network).not.toHaveBeenCalled()
    } finally { wrapper.unmount(); vi.useRealTimers() }
  })

  it('creates revision-capable configurations through an empty-revision CAS and leaves them disabled', async () => {
    const { api, entry } = revisionApi()
    api.list_network_instance_ids.mockResolvedValue({ disabled_inst_ids: [], running_inst_ids: [], runtime_capabilities: ['management:persisted-config-revision-v1'] })
    api.patch_local_config.mockImplementation(async () => {
      api.list_network_instance_ids.mockResolvedValue({ disabled_inst_ids: [INSTANCE_UUID], running_inst_ids: [], runtime_capabilities: ['management:persisted-config-revision-v1'] })
      return { status: 0, entry }
    })
    const wrapper = await openRevision(api, null, () => entry.config)
    try {
      await wrapper.find('button[data-label="web.device_management.create_network"]').trigger('click')
      await flushPromises()
      expect(api.patch_local_config.mock.calls[0][0]).toMatchObject({ expected_revision: '', apply_mode: 1 })
      expect(api.patch_local_config.mock.calls[0][0].field_mask).not.toContain('instance_id')
      expect(api.patch_local_config.mock.calls[0][0].config).not.toHaveProperty('enable_bbr')
      expect(api.save_config).not.toHaveBeenCalled()
      expect(api.update_network_instance_state).not.toHaveBeenCalled()
    } finally { wrapper.unmount(); vi.useRealTimers() }
  })
})
