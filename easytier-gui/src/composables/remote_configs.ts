import type { NetworkTypes } from 'easytier-frontend-lib'

export interface RemoteConfigSnapshot {
  rpc_url: string
  configs: Array<{ config: NetworkTypes.NetworkConfig, source?: unknown }>
}

export function remoteConfigKey(rpcUrl: string): string {
  return `remote-network-configs:${new URL(rpcUrl.trim()).href}`
}

export function readRemoteConfigs(rpcUrl: string): string | null {
  return localStorage.getItem(remoteConfigKey(rpcUrl))
}

export function storeRemoteConfigs(snapshot: RemoteConfigSnapshot): void {
  const key = remoteConfigKey(snapshot.rpc_url)
  if (snapshot.configs.length === 0) {
    localStorage.removeItem(key)
  } else {
    localStorage.setItem(key, JSON.stringify(snapshot.configs))
  }
}
