# World Map Data

`world-countries.json` contains Natural Earth 1:110m country geometry, with
only English/Chinese names and label coordinates retained. This geometry
is intended for the globe's low-resolution map rendering.

Source: https://github.com/nvkelso/natural-earth-vector/blob/master/geojson/ne_110m_admin_0_countries.geojson

`world-country-labels.json` contains country/region reference points from
Natural Earth 1:10m admin-0 countries, including small regions such as
Singapore, Hong Kong, and Macau that are absent from the 1:110m geometry.
Each array entry has `{ iso, en, zh, latitude, longitude }`, using
`ISO_A2` (or the valid two-letter `ISO_A2_EH` fallback), `NAME_EN`,
`NAME_ZH`, `LABEL_Y`, and `LABEL_X`, respectively. Entries with invalid
coordinates or no valid two-letter code are omitted. Codes are unique;
primary `ISO_A2` records take precedence over territories that share a
fallback code.

Source (pinned for reproducible generation):
https://github.com/nvkelso/natural-earth-vector/blob/9380cca83db5f9aef52d5e762765100745f84b27/geojson/ne_10m_admin_0_countries.geojson

Regenerate from the repository root with Node.js 18 or later:

```sh
node easytier-web/frontend/scripts/generate-globe-labels.mjs
```

The generator downloads and parses the upstream GeoJSON in memory,
retains only the label fields, and writes the JSON sorted by code.
The source geometry is not stored in the repository.

Natural Earth data is public domain: https://www.naturalearthdata.com/about/terms-of-use/

Country/region label coordinates are cartographic reference points, not
IP geolocation coordinates. They are used only as explicitly approximate
locations when GeoIP provides a country name or code but no coordinates.
They do not identify a node's city or physical location. Unresolved nodes
are not assigned fabricated geographic positions.
