import { inject, provide, type ComputedRef, type InjectionKey, type Ref } from 'vue'
import type ApiClient from './api'
import type { MeshNetworkView, MeshSyncResult, MeshJoinResult } from './api'
import type { DiscoveredNetwork, NetworkMember } from './networkDiscovery'

/** What the pages of one network share: the network and the console's record of it. */
export interface MeshNetworkContext {
  api: ApiClient
  networkName: ComputedRef<string>
  network: ComputedRef<DiscoveredNetwork | undefined>
  /** The console's view: core settings, preset, joining on their own. Central mode only. */
  view: Ref<MeshNetworkView | undefined>
  centralEnabled: ComputedRef<boolean>
  reload(): Promise<void>
}

const KEY: InjectionKey<MeshNetworkContext> = Symbol('meshNetwork')

export function provideMeshNetwork(context: MeshNetworkContext) {
  provide(KEY, context)
}

export function useMeshNetwork(): MeshNetworkContext {
  const context = inject(KEY, undefined)
  if (!context) throw new Error('Mesh network context is not provided')
  return context
}

export const deviceName = (member: Pick<NetworkMember, 'device'>) =>
  member.device.alias || member.device.hostname || member.device.machine_id

export type SyncResult = MeshSyncResult | MeshJoinResult

/** Results that did not reach their member as asked. */
export const failedResults = (results: SyncResult[]) =>
  results.filter(result => !['saved', 'unchanged', 'pending', 'locked'].includes(result.status))

export const resultSeverity = (status: SyncResult['status']) =>
  status === 'saved' || status === 'unchanged' ? 'success'
    : status === 'pending' || status === 'locked' ? 'info'
      : status === 'failed' ? 'danger' : 'warn'
