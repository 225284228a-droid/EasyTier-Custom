import { beforeEach, describe, expect, it, vi } from 'vitest'
import ApiClient from '../../frontend/src/modules/api'

const client = vi.hoisted(() => ({
  get: vi.fn(),
  post: vi.fn(),
  interceptors: {
    request: { use: vi.fn() },
    response: { use: vi.fn() },
  },
}))

vi.mock('axios', () => ({
  default: { create: vi.fn(() => client) },
  AxiosError: class extends Error {},
}))

describe('dashboard API request limits', () => {
  beforeEach(() => {
    client.get.mockReset()
    client.post.mockReset()
  })

  it('forwards timeout and cancellation to machine listing', async () => {
    const api = new ApiClient('http://localhost')
    const options = { timeout: 8_000, signal: new AbortController().signal }
    client.get.mockResolvedValue({ machines: [{ hostname: 'node-a' }] })
    expect(await api.list_machines(options)).toEqual([{ hostname: 'node-a' }])
    expect(client.get).toHaveBeenCalledWith('/machines', options)
  })

  it('forwards timeout and cancellation to topology collection', async () => {
    const api = new ApiClient('http://localhost')
    const options = { timeout: 8_000, signal: new AbortController().signal }
    client.post.mockResolvedValue({ info: { map: { mesh: { running: true } } } })
    expect(await api.collect_machine_network_info('node-a', options)).toEqual({ mesh: { running: true } })
    expect(client.post).toHaveBeenCalledWith('/machines/node-a/networks/info', {}, options)
  })
})
