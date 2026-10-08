/** Use the URL Axios actually calls; an empty injected host means same origin. */
export function apiPersistenceScope(apiRoot: string, account: string | undefined, pageUrl: string): string {
  if (!account?.trim()) return ''
  const url = new URL(apiRoot, pageUrl)
  url.hash = ''
  url.search = ''
  url.username = ''
  url.password = ''
  return `${url.href.replace(/\/+$/, '')}#account=${encodeURIComponent(account)}`
}
