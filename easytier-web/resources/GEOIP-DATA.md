# GeoIP Data

`dbip-country-lite-2026-10.mmdb` is the unmodified October 2026 DB-IP
IP to Country Lite database by [DB-IP](https://db-ip.com/).

- Source: https://db-ip.com/db/download/ip-to-country-lite
- Download: https://download.db-ip.com/free/dbip-country-lite-2026-10.mmdb.gz
- License: [Creative Commons Attribution 4.0 International](https://creativecommons.org/licenses/by/4.0/)
- Uncompressed SHA-256: `dbd70ccfa2a13627eaf4913d19920a1a229f1b3c5439ac01c509995b6364202a`

It provides country-level IPv4/IPv6 geolocation, not city coordinates. The
dashboard uses explicitly approximate Natural Earth country reference points
when no city-level location is available. The web console About dialog
attributes DB-IP. The offline database never sends IPs to DB-IP; the optional
online city cache sends node IPs to GeoJS/IPWho.is, as described in
`docs/city-geolocation.md`.

The web server embeds this offline database by default. `--geoip-db` overrides
it with an operator-provided MaxMind-format database. When distributing the
embedded database, preserve the DB-IP attribution and license notice.
