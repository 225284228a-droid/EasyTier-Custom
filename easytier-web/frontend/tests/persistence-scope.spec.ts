import { describe, expect, it } from 'vitest'
import { apiPersistenceScope } from '../src/modules/persistenceScope'

describe('API and account persistence scope', () => {
  it('resolves an injected same-origin root to its actual origin and path', () => {
    expect(apiPersistenceScope('/api/v1', 'alice', 'https://console.test/index.html#/h'))
      .toBe('https://console.test/api/v1#account=alice')
    expect(apiPersistenceScope('./api/v1', 'alice', 'https://console.test/console/'))
      .toBe('https://console.test/console/api/v1#account=alice')
  })
  it('separates accounts, API origins and paths, and disables storage before authentication', () => {
    const scope = (api: string, user: string | undefined) => apiPersistenceScope(api, user, 'https://console.test/')
    expect(scope('/api/v1', undefined)).toBe('')
    expect(scope('/api/v1', 'alice')).not.toBe(scope('/api/v1', 'bob'))
    expect(scope('https://first.test/api/v1', 'alice')).not.toBe(scope('https://second.test/api/v1', 'alice'))
    expect(scope('https://first.test/a/api/v1', 'alice')).not.toBe(scope('https://first.test/b/api/v1', 'alice'))
  })
})
