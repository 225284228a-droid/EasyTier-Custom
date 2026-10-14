import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { UnderlyingDnsServersResponse } from 'tauri-plugin-vpnservice-api'

const mocks = vi.hoisted(() => {
  const listeners = new Map<string, (payload: unknown) => Promise<void>>()
  const configs = new Map<string, Record<string, unknown>>()
  const networkInfo = new Map<string, unknown>()
  const vpnPolicies = new Map<string, Record<string, unknown>>()

  return {
    listeners,
    configs,
    networkInfo,
    vpnPolicies,
    addPluginListener: vi.fn(async (_plugin: string, event: string, listener: (payload: unknown) => Promise<void>) => {
      listeners.set(event, listener)
    }),
    collectNetworkInfo: vi.fn(async (instanceId: string) => ({
      info: { map: { [instanceId]: networkInfo.get(instanceId) } },
    })),
    consumeVpnTileAction: vi.fn(async () => ({})),
    getConfig: vi.fn(async (instanceId: string) => configs.get(instanceId)),
    getNativeVpnStatus: vi.fn<() => Promise<Record<string, unknown>>>(async () => ({ running: false })),
    getUnderlyingDnsServers: vi.fn<() => Promise<UnderlyingDnsServersResponse>>(async () => ({
      dnsServers: ['1.1.1.1'],
      systemDnsServers: ['1.1.1.1'],
    })),
    getCoreVpnStatus: vi.fn(async (instanceId: string) => vpnPolicies.get(instanceId)),
    listNetworkInstanceIds: vi.fn<() => Promise<{ running_inst_ids: unknown[] }>>(async () => ({ running_inst_ids: [] })),
    prepareVpn: vi.fn(async () => ({ granted: true })),
    setTunFd: vi.fn(async () => undefined),
    completeVpnReconfigure: vi.fn(async () => ({ accepted: true })),
    startVpn: vi.fn(async (request: Record<string, unknown>) => {
      await listeners.get('vpn_service_start')?.({ fd: 1, generation: request.generation, instanceId: request.instanceId })
      return {}
    }),
    reconfigureVpn: vi.fn(async (request: Record<string, unknown>) => {
      await listeners.get('vpn_service_start')?.({ fd: 2, generation: request.generation, instanceId: request.instanceId })
      return {}
    }),
    stopVpn: vi.fn(async () => {
      await listeners.get('vpn_service_stop')?.({})
      return {}
    }),
  }
})

vi.mock('@tauri-apps/api/core', () => ({
  addPluginListener: mocks.addPluginListener,
}))

vi.mock('easytier-frontend-lib', () => ({
  Utils: {
    UuidToStr: (value: unknown) => String(value),
    ipv4ToString: (address: { addr: string }) => address.addr,
  },
}))

vi.mock('tauri-plugin-vpnservice-api', () => ({
  complete_vpn_reconfigure: mocks.completeVpnReconfigure,
  consume_vpn_tile_action: mocks.consumeVpnTileAction,
  get_vpn_status: mocks.getNativeVpnStatus,
  get_underlying_dns_servers: mocks.getUnderlyingDnsServers,
  prepare_vpn: mocks.prepareVpn,
  reconfigure_vpn: mocks.reconfigureVpn,
  start_vpn: mocks.startVpn,
  stop_vpn: mocks.stopVpn,
}))

vi.mock('./backend', () => ({
  collectNetworkInfo: mocks.collectNetworkInfo,
  getConfig: mocks.getConfig,
  getVpnStatus: mocks.getCoreVpnStatus,
  listNetworkInstanceIds: mocks.listNetworkInstanceIds,
  setTunFd: mocks.setTunFd,
}))

function setConfig(instanceId: string, noTun = false) {
  mocks.configs.set(instanceId, {
    no_tun: noTun,
    dhcp: false,
    routes: [],
  })
}

function setReady(instanceId: string, ipv4: string) {
  mocks.networkInfo.set(instanceId, {
    my_node_info: {
      virtual_ipv4: {
        address: { addr: ipv4 },
        network_length: 24,
      },
    },
    routes: [],
  })
  setVpnPolicy(instanceId, [])
}

function setVpnPolicy(instanceId: string, routes: string[], generation = 0n, ipv6Addr = '') {
  mocks.vpnPolicies.set(instanceId, {
    capture_routes: routes,
    policy_generation: generation,
    ipv6_addr: ipv6Addr,
  })
}

async function loadVpnModule() {
  const mobileVpn = await import('./mobile_vpn')
  await mobileVpn.initMobileVpnService()
  return mobileVpn
}

beforeEach(() => {
  vi.useFakeTimers()
  vi.clearAllTimers()
  vi.resetModules()
  mocks.listeners.clear()
  mocks.configs.clear()
  mocks.networkInfo.clear()
  mocks.vpnPolicies.clear()
  mocks.addPluginListener.mockClear()
  mocks.collectNetworkInfo.mockClear()
  mocks.consumeVpnTileAction.mockReset()
  mocks.consumeVpnTileAction.mockResolvedValue({})
  mocks.getConfig.mockClear()
  mocks.getNativeVpnStatus.mockReset()
  mocks.getNativeVpnStatus.mockResolvedValue({ running: false })
  mocks.getUnderlyingDnsServers.mockReset()
  mocks.getUnderlyingDnsServers.mockResolvedValue({
    dnsServers: ['1.1.1.1'],
    systemDnsServers: ['1.1.1.1'],
  })
  mocks.getCoreVpnStatus.mockClear()
  mocks.listNetworkInstanceIds.mockReset()
  mocks.listNetworkInstanceIds.mockResolvedValue({ running_inst_ids: [] })
  mocks.prepareVpn.mockClear()
  mocks.setTunFd.mockClear()
  mocks.completeVpnReconfigure.mockClear()
  mocks.startVpn.mockClear()
  mocks.reconfigureVpn.mockClear()
  mocks.stopVpn.mockClear()
})

describe('mobile VPN reconciliation ownership', () => {
  it('keeps A active while B is unavailable, then replaces it without stopping the service', async () => {
    setConfig('A')
    setConfig('B')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')
    expect(mocks.startVpn).toHaveBeenCalledTimes(1)

    mocks.startVpn.mockClear()
    mocks.reconfigureVpn.mockClear()
    mocks.stopVpn.mockClear()
    await vpn.onNetworkInstanceChange('B')

    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.startVpn).not.toHaveBeenCalled()
    expect(mocks.reconfigureVpn).not.toHaveBeenCalled()

    setReady('B', '10.0.0.2')
    await vpn.onNetworkInstanceUpdate('B')

    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.reconfigureVpn).toHaveBeenCalledTimes(1)
    expect(mocks.reconfigureVpn).toHaveBeenCalledWith(expect.objectContaining({ ipv4Addr: '10.0.0.2/24' }))
    const generations = [
      mocks.reconfigureVpn.mock.calls[0][0].generation,
    ]
    expect(generations[0]).toMatch(/^\d+$/)
    expect(mocks.reconfigureVpn.mock.calls[0][0].dnsServers).toEqual(['1.1.1.1'])
  })

  it('starts generations above the Android persisted high-water mark', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    mocks.getNativeVpnStatus.mockResolvedValue({ running: false, generationHighWater: '900' })
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')

    expect(mocks.startVpn).toHaveBeenCalledWith(expect.objectContaining({ generation: '901' }))
  })

  it('preserves the active VPN during pre-run for a different instance', async () => {
    setConfig('A')
    setConfig('B')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')
    mocks.stopVpn.mockClear()

    await vpn.prepareVpnService('B')

    expect(mocks.stopVpn).not.toHaveBeenCalled()
  })

  it('preserves the VPN while retrying the same instance', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')
    mocks.stopVpn.mockClear()
    mocks.networkInfo.delete('A')

    await vpn.onNetworkInstanceUpdate('A')

    expect(mocks.stopVpn).not.toHaveBeenCalled()
  })

  it('ignores an update from an instance that no longer owns the VPN', async () => {
    setConfig('A')
    setConfig('B')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')
    await vpn.onNetworkInstanceChange('B')
    mocks.collectNetworkInfo.mockClear()

    await vpn.onNetworkInstanceUpdate('A')

    expect(mocks.collectNetworkInfo).not.toHaveBeenCalled()
  })

  it('does not stop the active VPN when the desired instance changes during an in-flight result', async () => {
    setConfig('A')
    setConfig('B')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')
    mocks.startVpn.mockClear()
    mocks.stopVpn.mockClear()

    interface NetworkInfoResponse { info: { map: Record<string, unknown> } }
    let resolveNetworkInfo: (value: NetworkInfoResponse) => void = () => undefined
    let markCollectStarted: () => void = () => undefined
    const collectStarted = new Promise<void>((resolve) => {
      markCollectStarted = resolve
    })
    mocks.collectNetworkInfo.mockImplementationOnce(async () => await new Promise<NetworkInfoResponse>((resolve) => {
      resolveNetworkInfo = resolve
      markCollectStarted()
    }))

    const staleUpdate = vpn.onNetworkInstanceUpdate('A')
    await collectStarted
    const switchToB = vpn.onNetworkInstanceChange('B')
    resolveNetworkInfo({
      info: {
        map: {
          A: {
            my_node_info: {
              virtual_ipv4: {
                address: { addr: '10.0.0.99' },
                network_length: 24,
              },
            },
            routes: [],
          },
        },
      },
    })

    await Promise.all([staleUpdate, switchToB])

    expect(mocks.startVpn).not.toHaveBeenCalled()
    expect(mocks.stopVpn).not.toHaveBeenCalled()
  })

  it('attaches the selected core instance to an already-running native VPN', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    mocks.getNativeVpnStatus.mockResolvedValue({
      running: true,
      ipv4Addr: '10.0.0.1/24',
      routes: [],
    })
    mocks.listNetworkInstanceIds.mockResolvedValue({ running_inst_ids: ['A'] })
    const vpn = await loadVpnModule()

    await vpn.syncMobileVpnService()

    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.startVpn).not.toHaveBeenCalled()
    expect(mocks.reconfigureVpn).toHaveBeenCalledTimes(1)
  })

  it('uses underlying DNS and the core capture set before completing replacement', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    mocks.configs.set('A', {
      no_tun: false,
      dhcp: false,
      routes: ['192.0.2.0/24'],
    })
    setVpnPolicy('A', ['100.64.0.0/10', '2001:db8:1::/64'], 7n, 'fd42::1/64')
    const vpn = await loadVpnModule()
    let releaseInstall: () => void = () => undefined
    const installStarted = new Promise<void>((resolve) => {
      releaseInstall = resolve
    })
    let finishInstall: () => void = () => undefined
    const installPending = new Promise<void>((resolve) => {
      finishInstall = resolve
    })
    mocks.setTunFd.mockImplementationOnce(async () => {
      releaseInstall()
      await installPending
    })

    const reconcile = vpn.onNetworkInstanceChange('A')
    await installStarted

    const request = mocks.startVpn.mock.calls[0][0]
    expect(request).toMatchObject({
      ipv6Addr: 'fd42::1/64',
      routes: ['100.64.0.0/10', '2001:db8:1::/64'],
      dnsServers: ['1.1.1.1'],
      instanceId: 'A',
    })
    expect(mocks.completeVpnReconfigure).not.toHaveBeenCalled()

    finishInstall()
    await reconcile

    expect(mocks.completeVpnReconfigure).toHaveBeenCalledWith(expect.any(String), true)
    expect(mocks.setTunFd.mock.invocationCallOrder[0]).toBeLessThan(
      mocks.completeVpnReconfigure.mock.invocationCallOrder[0],
    )
  })

  it('preserves scoped IPv6 DNS for the VPN Builder', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    mocks.getUnderlyingDnsServers.mockResolvedValueOnce({
      dnsServers: ['fe80::1%wlan0'],
      systemDnsServers: ['fe80::1'],
    })
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')

    expect(mocks.startVpn.mock.calls[0][0].dnsServers).toEqual(['fe80::1%wlan0'])
  })

  it('retains capture and retries TUN attachment after an install failure', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()
    mocks.setTunFd.mockRejectedValueOnce(new Error('install failed'))

    await vpn.onNetworkInstanceChange('A')

    expect(mocks.completeVpnReconfigure).toHaveBeenCalledWith(expect.any(String), false, 'install failed')
    expect(mocks.startVpn).toHaveBeenCalledOnce()

    mocks.setTunFd.mockResolvedValue(undefined)
    await vi.advanceTimersByTimeAsync(2000)

    expect(mocks.setTunFd).toHaveBeenCalledTimes(2)
    expect(mocks.completeVpnReconfigure).toHaveBeenLastCalledWith(expect.any(String), true)
  })
})

describe('mobile VPN tile action delivery', () => {
  it('does not consume a pending action before a handler is ready', async () => {
    const vpn = await loadVpnModule()

    expect(await vpn.consumePendingMobileVpnTileAction()).toBe(false)
    expect(mocks.consumeVpnTileAction).not.toHaveBeenCalled()
  })

  it('consumes and dispatches a pending action once a handler is registered', async () => {
    const vpn = await loadVpnModule()
    const handler = vi.fn(async () => undefined)
    mocks.consumeVpnTileAction.mockResolvedValue({ action: 'start' })
    vpn.setMobileVpnTileActionHandler(handler)

    expect(await vpn.consumePendingMobileVpnTileAction()).toBe(true)
    expect(handler).toHaveBeenCalledWith('start')
  })
})
