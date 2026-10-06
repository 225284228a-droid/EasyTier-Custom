# City Geolocation Cache

`easytier-web` maintains an independent SQLite database, `geoip-cache.db`,
for city-level geolocation of public IPs reported by actual mesh nodes.
The authentication/configuration database (`et.db`) is not used for this
cache. The cache refuses the primary database path and unrelated SQL schemas.

## Data And Privacy

Only canonical public IPv4/IPv6 addresses obtained from a running node's
STUN/public-IP information are queried. Web request addresses, config-server
transport URLs, CDN/FRP addresses, private addresses and documentation ranges
are not geolocation inputs. The database contains IPs and geolocation records,
not device IDs, network secrets, account credentials or management URLs.

Online mode sends each queried node public IP to GeoJS and, when needed,
IPWho.is over HTTPS. No client-side third-party script, tracking pixel or
bulk IP-range scan is used. API output is cached privately for mesh display;
this is not a mirror or public redistribution of a provider's database.
Use `--geoip-offline` to disable these external requests.

Cities and WGS84 coordinates are IP-based estimates, not verified physical
device locations or GPS fixes. NAT, VPNs, mobile networks and anycast can
reduce accuracy. GeoJS's reported accuracy radius is stored when available.
The map uses returned city coordinates and keeps country-only fallback
positions separate; a country reference point is never saved as a city.

## Lifecycle

- Newly observed IPs are queued; web responses do not wait for external HTTP.
- Active nodes are sampled in the background independently of an open browser.
  Only successful node snapshots verify an IP; failed attempts do not keep
  old addresses alive. Recent IP verification has a three-minute grace period.
- Default refresh age is 24 hours, while the IP is recently in use.
- Failed lookups retain the last valid result, with exponential retry delays
  from 15 minutes to six hours. City results older than seven days are not used.
- Provider quotas and `Retry-After` cooldowns survive restarts. Requests are
  limited to one per second and 900 per UTC day per provider by default.
- An hourly cleanup removes records unused for 30 days by default. LRU
  eviction bounds the cache to 10,000 IPs. In-flight leases are not deleted.
  SQLite WAL checkpointing and incremental vacuum reclaim unused space.
- A bounded memory cache avoids repeated SQL writes for the same IP within
  a minute. Request-path SQL waits are limited to 150 ms. Database/network
  failures fall back to the offline MMDB and do not stop the config server.
- Unix database files and sidecars use private permissions. Generated cache
  files are excluded from Git; keep them out of publicly served directories.

## Configuration

| Option | Environment | Default |
| --- | --- | --- |
| `--geoip-cache-db` | `ET_GEOIP_CACHE_DB` | `geoip-cache.db` |
| `--geoip-offline` | `ET_GEOIP_OFFLINE` | `false` |
| `--geoip-refresh-hours` | `ET_GEOIP_REFRESH_HOURS` | `24` |
| `--geoip-retention-days` | `ET_GEOIP_RETENTION_DAYS` | `30` |
| `--geoip-cache-max-entries` | `ET_GEOIP_CACHE_MAX_ENTRIES` | `10000` |
| `--geoip-daily-budget` | `ET_GEOIP_DAILY_BUDGET` | `900` |

For example, preserve the cache in the server's persistent data directory:

```sh
easytier-web --db /var/lib/easytier/et.db \
  --geoip-cache-db /var/lib/easytier/geoip-cache.db \
  --geoip-refresh-hours 24 --geoip-retention-days 30
```

Keep the directory containing the SQLite file persistent, including WAL/SHM
sidecars. The independent cache can be discarded and rebuilt without changing
users or network configurations. Stop the server before manually removing
the cache file and its sidecars.

The existing `--geoip-db` option still selects an offline MaxMind-format
database; by default the embedded DB-IP country database is the fallback.
Provider attribution is accessible through the console's About dialog.

## Inspecting The Cache

The `city_cache` table has one row per canonical IP. `location_json` holds
country, region, city, coordinates and optional `accuracy_radius_km`;
`source`, `updated_at`, `last_used`, `last_attempt`, `refresh_at`,
`lease_until` and `fail_count` record its lifecycle.
`provider_state` stores daily quotas and global provider cooldowns.
All timestamps are Unix seconds in UTC.

```sql
SELECT ip, json_extract(location_json, '$.city') AS city,
       json_extract(location_json, '$.latitude') AS latitude,
       json_extract(location_json, '$.longitude') AS longitude,
       source, datetime(updated_at, 'unixepoch') AS updated_utc
FROM city_cache
WHERE location_json IS NOT NULL;
```

## Sources

Provider API formats, limits and terms were checked on October 6, 2026:

- GeoJS API: https://www.geojs.io/docs/v1/endpoints/geo/
- GeoJS service terms: https://www.geojs.io/tos/
- GeoJS data source acknowledgement: https://www.geojs.io/
- IPWho.is API: https://ipwhois.io/documentation
- IPWho.is service terms: https://ipwhois.io/terms

GeoJS uses MaxMind GeoLite data. IPWho.is's free endpoint currently supports
commercial use and 1,000 daily requests; limits and terms can change.
Operator quotas may also be shared with other services using the same
outgoing public IP, so HTTP 429 handling remains necessary.
