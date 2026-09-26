import { beforeEach, describe, expect, it, vi } from 'vitest'
import { initRpcConnection, syncConfigsFromCore } from './backend'
import { readRemoteConfigs, remoteConfigKey, storeRemoteConfigs } from './remote_configs'

const invoke = vi.hoisted(() => vi.fn(async () => undefined))
vi.mock('@tauri-apps/api/core', () => ({ invoke }))
vi.mock('easytier-frontend-lib', () => ({
  NetworkTypes: {
    normalizeNetworkConfig: (config: unknown) => config,
    toBackendNetworkConfig: (config: unknown) => config,
  },
}))

beforeEach(() => {
  const values = new Map<string, string>()
  vi.stubGlobal('localStorage', {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => values.set(key, value),
    removeItem: (key: string) => values.delete(key),
  })
  invoke.mockClear()
})

describe('remote GUI fallback configs', () => {
  const first = 'tcp://127.0.0.1:15999'
  const second = 'tcp://127.0.0.2:15999'
  const config = { instance_id: 'one', network_name: 'local-draft' } as any

  it('isolates endpoints and accepts late events for the original endpoint', () => {
    storeRemoteConfigs({ rpc_url: second, configs: [{ config, source: 'user' }] })
    storeRemoteConfigs({ rpc_url: first, configs: [{ config: { ...config, instance_id: 'old' } }] })
    expect(JSON.parse(readRemoteConfigs(first)!)[0].config.instance_id).toBe('old')
    expect(JSON.parse(readRemoteConfigs(second)!)[0].config.instance_id).toBe('one')
    expect(remoteConfigKey(` ${first} `)).toBe(remoteConfigKey(first))
  })

  it('loads drafts without asking the core to autostart any instance', async () => {
    storeRemoteConfigs({ rpc_url: first, configs: [{ config, source: 'webhook' }] })
    await syncConfigsFromCore(first)
    expect(invoke).toHaveBeenCalledWith('load_configs', {
      configs: [{ config, source: 'web' }], enabledNetworks: [],
    })
    expect(invoke).not.toHaveBeenCalledWith('run_network_instance', expect.anything())
  })

  it('clears migrated drafts without deleting another endpoint cache', async () => {
    storeRemoteConfigs({ rpc_url: first, configs: [{ config }] })
    storeRemoteConfigs({ rpc_url: second, configs: [{ config }] })
    storeRemoteConfigs({ rpc_url: first, configs: [] })
    expect(readRemoteConfigs(first)).toBeNull()
    expect(readRemoteConfigs(second)).not.toBeNull()
    await syncConfigsFromCore()
    expect(invoke).toHaveBeenCalledWith('load_configs', { configs: [], enabledNetworks: [] })
  })

  it('enables endpoint cache writes only for an explicitly remote connection', async () => {
    await initRpcConnection(false, first)
    expect(invoke).toHaveBeenLastCalledWith('init_rpc_connection', {
      isNormalMode: false, url: first, configDir: undefined, remoteConfigCache: false,
    })
    await initRpcConnection(false, first, undefined, true)
    expect(invoke).toHaveBeenLastCalledWith('init_rpc_connection', {
      isNormalMode: false, url: first, configDir: undefined, remoteConfigCache: true,
    })
  })
})
