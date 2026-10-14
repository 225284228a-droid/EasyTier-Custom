import { addPluginListener } from '@tauri-apps/api/core'
import { Utils } from 'easytier-frontend-lib'
import {
  complete_vpn_reconfigure,
  consume_vpn_tile_action,
  get_underlying_dns_servers,
  get_vpn_status,
  prepare_vpn,
  reconfigure_vpn,
  start_vpn,
  stop_vpn,
  type VpnTileAction,
} from 'tauri-plugin-vpnservice-api'
import {
  collectNetworkInfo,
  getConfig,
  getVpnStatus,
  listNetworkInstanceIds,
  setTunFd,
} from './backend'

interface vpnStatus {
  running: boolean
  ipv4Addr: string | null | undefined
  ipv4Cidr: number | null | undefined
  ipv6Addr: string | null | undefined
  routes: string[]
  dnsServers: string[]
  generation: string | undefined
  generationHighWater: string | undefined
  policyGeneration: string | undefined
}

let vpnReconcileTimer: ReturnType<typeof setTimeout> | null = null
const VPN_RECONCILE_INTERVAL_MS = 2000
const VPN_RECONCILE_MAX_ATTEMPTS = 60
const vpnTunRetryTimers = new Map<string, ReturnType<typeof setTimeout>>()
const VPN_GENERATION_STORAGE_KEY = 'easytier.vpn.configuration-generation'
const MAX_VPN_GENERATION = (1n << 63n) - 1n
let vpnConfigurationGeneration = 0n

function parseVpnGeneration(value: unknown) {
  if (typeof value !== 'string' || !/^\d+$/.test(value)) return 0n
  try {
    const parsed = BigInt(value)
    return parsed <= MAX_VPN_GENERATION ? parsed : 0n
  }
  catch {
    return 0n
  }
}

async function nextVpnConfigurationGeneration() {
  let highWater = vpnConfigurationGeneration
  try {
    const stored = parseVpnGeneration(localStorage.getItem(VPN_GENERATION_STORAGE_KEY))
    if (stored > highWater) highWater = stored
  }
  catch {
    // The in-memory counter remains available in restricted WebView contexts.
  }

  try {
    const status = await get_vpn_status()
    const nativeHighWater = parseVpnGeneration(status?.generationHighWater)
    if (nativeHighWater > highWater) highWater = nativeHighWater
    curVpnStatus.generationHighWater = status?.generationHighWater ?? curVpnStatus.generationHighWater
  }
  catch (error) {
    console.warn('vpn generation high-water query failed', error)
  }

  const next = highWater + 1n
  if (next > MAX_VPN_GENERATION) throw new Error('vpn generation counter exhausted')
  vpnConfigurationGeneration = next
  try {
    localStorage.setItem(VPN_GENERATION_STORAGE_KEY, next.toString())
  }
  catch {
    // The Android high-water mark is also persisted by the native service.
  }
  return next.toString()
}

let desiredVpnInstanceId: string | undefined
let activeVpnInstanceId: string | undefined
let vpnReconcileGeneration = 0
let vpnReconcileAttempts = 0
let vpnReconcileQueue: Promise<void> = Promise.resolve()
let vpnPermissionRequest: Promise<boolean> | null = null
let vpnTileActionHandler: ((action: VpnTileAction) => Promise<void>) | undefined
let vpnTileActionQueue: Promise<void> = Promise.resolve()

const curVpnStatus: vpnStatus = {
  running: false,
  ipv4Addr: undefined,
  ipv4Cidr: undefined,
  ipv6Addr: undefined,
  routes: [],
  dnsServers: [],
  generation: undefined,
  generationHighWater: undefined,
  policyGeneration: undefined,
}

export function setMobileVpnTileActionHandler(
  handler?: (action: VpnTileAction) => Promise<void>,
) {
  vpnTileActionHandler = handler
}

export async function consumePendingMobileVpnTileAction() {
  const handler = vpnTileActionHandler
  if (!handler) {
    return false
  }

  const action = (await consume_vpn_tile_action())?.action
  if (action !== 'start' && action !== 'stop') {
    return false
  }

  const run = vpnTileActionQueue
    .catch(error => console.error('previous VPN tile action failed', error))
    .then(() => handler(action))
  vpnTileActionQueue = run.catch(error => console.error('VPN tile action failed', error))
  await run
  return true
}

async function requestVpnPermissionOnce() {
  console.log('prepare vpn')
  const prepare_ret = await prepare_vpn()
  console.log('prepare vpn', JSON.stringify((prepare_ret)))
  if (prepare_ret?.errorMsg?.length) {
    throw new Error(prepare_ret.errorMsg)
  }

  const granted = prepare_ret?.granted ?? true
  if (!granted) {
    console.info('vpn permission request was denied or dismissed')
  }

  return granted
}

async function requestVpnPermission() {
  if (vpnPermissionRequest) {
    console.log('reuse pending vpn permission request')
    return await vpnPermissionRequest
  }

  const request = requestVpnPermissionOnce()
  vpnPermissionRequest = request
  try {
    return await request
  }
  finally {
    if (vpnPermissionRequest === request) {
      vpnPermissionRequest = null
    }
  }
}

function clearVpnReconcileTimer() {
  if (vpnReconcileTimer) {
    clearTimeout(vpnReconcileTimer)
    vpnReconcileTimer = null
  }
}

function beginVpnReconcile(instanceId?: string) {
  clearVpnReconcileTimer()
  desiredVpnInstanceId = instanceId
  vpnReconcileAttempts = 0
  vpnReconcileGeneration += 1
  return vpnReconcileGeneration
}

function isCurrentVpnReconcile(instanceId: string, generation: number) {
  return desiredVpnInstanceId === (instanceId || undefined) && vpnReconcileGeneration === generation
}

function scheduleVpnReconcile(instanceId: string, generation: number, reason: string) {
  if (!isCurrentVpnReconcile(instanceId, generation))
    return

  if (vpnReconcileAttempts >= VPN_RECONCILE_MAX_ATTEMPTS) {
    console.error(
      'vpn service reconcile stopped after maximum attempts',
      instanceId,
      VPN_RECONCILE_MAX_ATTEMPTS,
      reason,
    )
    return
  }

  clearVpnReconcileTimer()
  vpnReconcileAttempts += 1
  console.log(
    'vpn service is not ready, retrying',
    JSON.stringify({
      instanceId,
      attempt: vpnReconcileAttempts,
      maxAttempts: VPN_RECONCILE_MAX_ATTEMPTS,
      delayMs: VPN_RECONCILE_INTERVAL_MS,
      reason,
    }),
  )
  vpnReconcileTimer = setTimeout(() => {
    vpnReconcileTimer = null
    void enqueueVpnReconcile(instanceId, generation)
  }, VPN_RECONCILE_INTERVAL_MS)
}

function resetVpnConfigStatus() {
  curVpnStatus.ipv4Addr = undefined
  curVpnStatus.ipv4Cidr = undefined
  curVpnStatus.ipv6Addr = undefined
  curVpnStatus.routes = []
  curVpnStatus.dnsServers = []
  curVpnStatus.generation = undefined
  curVpnStatus.generationHighWater = undefined
  curVpnStatus.policyGeneration = undefined
}

function syncVpnStatusFromNative(status: Awaited<ReturnType<typeof get_vpn_status>>) {
  curVpnStatus.running = status?.running ?? false
  curVpnStatus.generationHighWater = status?.generationHighWater ?? undefined
  if (!curVpnStatus.running) {
    activeVpnInstanceId = undefined
    resetVpnConfigStatus()
    return
  }

  const ipv4WithCidr = status?.ipv4Addr
  if (ipv4WithCidr?.length) {
    const [ipv4Addr, cidr] = ipv4WithCidr.split('/')
    curVpnStatus.ipv4Addr = ipv4Addr

    const parsedCidr = Number(cidr)
    curVpnStatus.ipv4Cidr = Number.isInteger(parsedCidr) ? parsedCidr : undefined
  }
  else {
    curVpnStatus.ipv4Addr = undefined
    curVpnStatus.ipv4Cidr = undefined
  }

  curVpnStatus.ipv6Addr = status?.ipv6Addr ?? undefined
  curVpnStatus.routes = [...(status?.routes ?? [])]
  curVpnStatus.dnsServers = [...(status?.dnsServers ?? [])]
  curVpnStatus.generation = status?.generation ?? undefined
}

async function waitVpnStatus(target_status: boolean, timeout_sec: number) {
  const start_time = Date.now()
  while (curVpnStatus.running !== target_status) {
    if (Date.now() - start_time > timeout_sec * 1000) {
      throw new Error('wait vpn status timeout')
    }
    await new Promise(r => setTimeout(r, 50))
  }
}

async function doStopVpn(force = false) {
  const wasRunning = curVpnStatus.running
  if (!force && !wasRunning) {
    activeVpnInstanceId = undefined
    return
  }
  console.log('stop vpn')
  const stop_ret = await stop_vpn()
  console.log('stop vpn', JSON.stringify((stop_ret)))
  if (wasRunning) {
    await waitVpnStatus(false, 3)
  }

  activeVpnInstanceId = undefined
  resetVpnConfigStatus()
}

async function doConfigureVpn(
  instanceId: string,
  ipv4Addr: string,
  ipv4Cidr: number,
  ipv6Addr: string | undefined,
  routes: string[],
  dnsServers: string[],
  policyGeneration: string,
) {
  const generation = await nextVpnConfigurationGeneration()
  console.log('configure vpn service', instanceId, ipv4Addr, ipv4Cidr, ipv6Addr, routes, dnsServers)
  const request = {
    ipv4Addr: `${ipv4Addr}/${ipv4Cidr}`,
    ipv6Addr,
    routes,
    dnsServers,
    disallowedApplications: [],
    mtu: 1300,
    instanceId,
    generation,
  }

  curVpnStatus.generation = generation
  curVpnStatus.generationHighWater = generation
  curVpnStatus.policyGeneration = policyGeneration
  curVpnStatus.ipv4Addr = ipv4Addr
  curVpnStatus.ipv4Cidr = ipv4Cidr
  curVpnStatus.ipv6Addr = ipv6Addr
  curVpnStatus.routes = [...routes]
  curVpnStatus.dnsServers = [...dnsServers]
  let configure = curVpnStatus.running ? reconfigure_vpn : start_vpn
  let start_ret = await configure(request)
  console.log('configure vpn response', JSON.stringify(start_ret))
  if (start_ret?.errorMsg === 'need_prepare') {
    const granted = await requestVpnPermission()
    if (!granted) {
      throw new Error('vpn_permission_denied')
    }
    configure = curVpnStatus.running ? reconfigure_vpn : start_vpn
    start_ret = await configure(request)
    console.log('configure vpn retry response', JSON.stringify(start_ret))
  }

  if (start_ret?.errorMsg?.length) {
    throw new Error(start_ret.errorMsg)
  }
  await waitVpnStatus(true, 3)

  activeVpnInstanceId = instanceId
}

function clearVpnTunRetry(generation: string) {
  const timer = vpnTunRetryTimers.get(generation)
  if (timer) clearTimeout(timer)
  vpnTunRetryTimers.delete(generation)
}

function scheduleVpnTunRetry(payload: any, attempt: number) {
  const generation = String(payload?.generation ?? '')
  if (!generation || generation !== curVpnStatus.generation || !curVpnStatus.running)
    return

  if (attempt >= VPN_RECONCILE_MAX_ATTEMPTS) {
    console.error('vpn TUN attach retry stopped after maximum attempts', generation)
    return
  }

  clearVpnTunRetry(generation)
  const timer = setTimeout(() => {
    vpnTunRetryTimers.delete(generation)
    void attachVpnTun(payload, attempt + 1)
  }, VPN_RECONCILE_INTERVAL_MS)
  vpnTunRetryTimers.set(generation, timer)
}

async function attachVpnTun(payload: any, attempt = 0) {
  const generation = String(payload?.generation ?? '')
  const instanceId = String(payload?.instanceId ?? '')
  if (!generation || !instanceId || generation !== curVpnStatus.generation || !payload?.fd)
    return

  try {
    await setTunFd(payload.fd, instanceId, generation)
    const completion = await complete_vpn_reconfigure(generation, true)
    clearVpnTunRetry(generation)
    if (!completion?.accepted) {
      console.info('vpn TUN attach acknowledgement was stale', generation)
    }
  }
  catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    console.error('set tun fd failed; retaining Android capture interface', generation, message)
    await complete_vpn_reconfigure(generation, false, message).catch((completionError) => {
      console.error('vpn install failure report failed', completionError)
    })
    scheduleVpnTunRetry(payload, attempt)
  }
}

async function onVpnServiceStart(payload: any) {
  console.log('vpn service start', JSON.stringify(payload))
  curVpnStatus.running = true
  const generation = String(payload?.generation ?? '')
  if (generation && !curVpnStatus.generation) curVpnStatus.generation = generation
  await attachVpnTun(payload)
}

async function onVpnServiceError(payload: any) {
  console.error('vpn interface configuration failed', JSON.stringify(payload))
  if (payload?.instanceId === desiredVpnInstanceId && payload?.generation === curVpnStatus.generation) {
    curVpnStatus.generation = undefined
    curVpnStatus.generationHighWater = payload.generation ?? curVpnStatus.generationHighWater
    curVpnStatus.policyGeneration = undefined
    scheduleVpnReconcile(payload.instanceId, vpnReconcileGeneration, 'vpn_interface_establish_failed')
  }
}

async function onVpnServiceStop(payload: any) {
  console.log('vpn service stop', JSON.stringify(payload))
  curVpnStatus.running = false
  for (const generation of vpnTunRetryTimers.keys()) clearVpnTunRetry(generation)
  activeVpnInstanceId = undefined
  resetVpnConfigStatus()
}

async function registerVpnServiceListener() {
  console.log('register vpn service listener')
  await addPluginListener(
    'vpnservice',
    'vpn_service_start',
    onVpnServiceStart,
  )

  await addPluginListener(
    'vpnservice',
    'vpn_service_stop',
    onVpnServiceStop,
  )

  await addPluginListener(
    'vpnservice',
    'vpn_service_error',
    onVpnServiceError,
  )

  await addPluginListener(
    'vpnservice',
    'vpn_tile_action',
    () => {
      void consumePendingMobileVpnTileAction().catch((error) => {
        console.error('consume VPN tile action failed', error)
      })
    },
  )
}

async function stopVpnOwnedByOtherInstance(instanceId: string, generation: number) {
  if (!isCurrentVpnReconcile(instanceId, generation))
    return false

  if (curVpnStatus.running && activeVpnInstanceId !== instanceId) {
    console.info('vpn service will replace its active instance after the new TUN is installed', activeVpnInstanceId, instanceId)
  }

  return isCurrentVpnReconcile(instanceId, generation)
}

async function reconcileNetworkInstance(instanceId: string, generation: number) {
  if (!isCurrentVpnReconcile(instanceId, generation))
    return

  clearVpnReconcileTimer()

  if (!instanceId) {
    console.warn('vpn service skipped because instance id is empty')
    if (curVpnStatus.running) {
      await doStopVpn()
    }
    return
  }
  const config = await getConfig(instanceId)
  if (!isCurrentVpnReconcile(instanceId, generation))
    return

  console.log('vpn service loaded config', instanceId, JSON.stringify({
    no_tun: config.no_tun,
    dhcp: config.dhcp,
  }))
  if (config.no_tun) {
    console.log('vpn service skipped because no_tun is enabled', instanceId)
    if (activeVpnInstanceId === instanceId) {
      await doStopVpn()
    }
    return
  }

  if (!await stopVpnOwnedByOtherInstance(instanceId, generation))
    return

  let curNetworkInfo
  let vpnPolicy
  let underlyingDnsServers: string[]
  try {
    const [networkInfo, dnsResponse] = await Promise.all([
      collectNetworkInfo(instanceId).then(response => response?.info?.map?.[instanceId]),
      get_underlying_dns_servers(),
    ])
    if (dnsResponse?.errorMsg) throw new Error(dnsResponse.errorMsg)
    curNetworkInfo = networkInfo
    underlyingDnsServers = [...new Set(dnsResponse?.dnsServers ?? [])]
    if (!isCurrentVpnReconcile(instanceId, generation))
      return
    vpnPolicy = await getVpnStatus(instanceId)
  }
  catch (e) {
    console.warn('vpn service core state query failed', instanceId, e)
    scheduleVpnReconcile(instanceId, generation, 'core_state_query_failed')
    return
  }

  if (!isCurrentVpnReconcile(instanceId, generation))
    return

  if (!curNetworkInfo) {
    scheduleVpnReconcile(instanceId, generation, 'network_info_unavailable')
    return
  }

  if (!vpnPolicy) {
    scheduleVpnReconcile(instanceId, generation, 'vpn_policy_unavailable')
    return
  }

  if (!Array.isArray(vpnPolicy.capture_routes)) {
    scheduleVpnReconcile(instanceId, generation, 'vpn_capture_routes_unavailable')
    return
  }

  if (curNetworkInfo.error_msg?.length) {
    console.warn('vpn service skipped because network instance failed', instanceId, curNetworkInfo.error_msg)
    vpnReconcileAttempts = 0
    if (activeVpnInstanceId === instanceId) await doStopVpn()
    return
  }

  const virtualIpv4 = curNetworkInfo.my_node_info?.virtual_ipv4
  const virtual_ip = virtualIpv4?.address?.addr ? Utils.ipv4ToString(virtualIpv4.address) : undefined

  if (!virtual_ip || !virtual_ip.length) {
    scheduleVpnReconcile(
      instanceId,
      generation,
      config.dhcp ? 'dhcp_ipv4_unavailable' : 'static_ipv4_unavailable',
    )
    return
  }

  vpnReconcileAttempts = 0

  let network_length = virtualIpv4?.network_length
  if (!network_length) {
    network_length = 24
  }

  const routes = vpnPolicy.capture_routes
  const hasIpv6Route = routes.some((route: string) => route.split('/')[0].includes(':'))
  const ipv6Addr = hasIpv6Route ? vpnPolicy.ipv6_addr || undefined : undefined
  const policyGeneration = vpnPolicy.policy_generation.toString()

  const ipChanged = virtual_ip !== curVpnStatus.ipv4Addr
  const cidrChanged = network_length !== curVpnStatus.ipv4Cidr
  const ipv6Changed = ipv6Addr !== curVpnStatus.ipv6Addr
  const routesChanged = JSON.stringify(routes) !== JSON.stringify(curVpnStatus.routes)
  const dnsChanged = JSON.stringify(underlyingDnsServers) !== JSON.stringify(curVpnStatus.dnsServers)
  const policyChanged = policyGeneration !== curVpnStatus.policyGeneration
  const ownerChanged = activeVpnInstanceId !== instanceId
  const configChanged = ipChanged || cidrChanged || ipv6Changed || routesChanged || dnsChanged || policyChanged || ownerChanged
  const shouldStartVpn = !curVpnStatus.running

  if (shouldStartVpn || configChanged) {
    console.info('vpn service core policy changed', JSON.stringify(curVpnStatus), instanceId, policyGeneration)

    try {
      if (!isCurrentVpnReconcile(instanceId, generation))
        return

      await doConfigureVpn(instanceId, virtual_ip, network_length, ipv6Addr, routes, underlyingDnsServers, policyGeneration)
    }
    catch (e) {
      if (e instanceof Error && e.message === 'need_prepare') {
        console.info('vpn permission is required before starting the Android VPN service')
        return
      }
      if (e instanceof Error && e.message === 'vpn_permission_denied') {
        console.info('vpn permission request was denied or dismissed')
        return
      }
      console.error('start vpn service failed', e)
    }
  }
}

function enqueueVpnTask(task: () => Promise<void>) {
  const run = vpnReconcileQueue
    .catch((e) => {
      console.error('previous vpn service reconcile failed', e)
    })
    .then(task)
  vpnReconcileQueue = run.catch((e) => {
    console.error('vpn service reconcile failed', e)
  })
  return run
}

function enqueueVpnReconcile(instanceId: string, generation: number) {
  return enqueueVpnTask(() => reconcileNetworkInstance(instanceId, generation))
}

export async function onNetworkInstanceChange(instanceId: string) {
  const generation = beginVpnReconcile(instanceId || undefined)

  if (instanceId && await isNoTunEnabled(instanceId)) {
    if (vpnReconcileGeneration !== generation)
      return

    if (activeVpnInstanceId === instanceId) {
      desiredVpnInstanceId = undefined
      await enqueueVpnReconcile('', generation)
      return
    }

    desiredVpnInstanceId = activeVpnInstanceId
    if (activeVpnInstanceId) {
      await enqueueVpnReconcile(activeVpnInstanceId, generation)
    }
    return
  }

  if (vpnReconcileGeneration !== generation)
    return

  await enqueueVpnReconcile(instanceId, generation)
}

export async function onNetworkInstanceUpdate(instanceId: string) {
  if (!instanceId || instanceId !== desiredVpnInstanceId)
    return

  const generation = beginVpnReconcile(instanceId)
  await enqueueVpnReconcile(instanceId, generation)
}

async function isNoTunEnabled(instanceId: string | undefined) {
  if (!instanceId) {
    return false
  }
  return (await getConfig(instanceId)).no_tun ?? false
}

async function findRunningTunInstanceId() {
  const instanceIds = await listNetworkInstanceIds()
  const runningIds = (instanceIds.running_inst_ids ?? []).map(Utils.UuidToStr)
  console.log('vpn service sync running instances', JSON.stringify(runningIds))

  for (const instanceId of runningIds) {
    if (await isNoTunEnabled(instanceId)) {
      continue
    }

    return instanceId
  }

  return undefined
}

export async function initMobileVpnService() {
  await registerVpnServiceListener()
}

export async function prepareVpnService(instanceId: string) {
  if (await isNoTunEnabled(instanceId)) {
    return
  }

  const generation = beginVpnReconcile(instanceId)
  const stopPreviousOwner = enqueueVpnTask(async () => {
    await stopVpnOwnedByOtherInstance(instanceId, generation)
  })
  await Promise.all([requestVpnPermission(), stopPreviousOwner])
}

export async function syncMobileVpnService() {
  syncVpnStatusFromNative(await get_vpn_status())
  const instanceId = await findRunningTunInstanceId()
  if (instanceId) {
    console.log('vpn service sync selected instance', instanceId)
    await onNetworkInstanceChange(instanceId)
    return
  }

  await onNetworkInstanceChange('')
}
