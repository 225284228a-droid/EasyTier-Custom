import { writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

const sourceRevision = '9380cca83db5f9aef52d5e762765100745f84b27';
const sourceUrl = `https://raw.githubusercontent.com/nvkelso/natural-earth-vector/${sourceRevision}/geojson/ne_10m_admin_0_countries.geojson`;
const outputUrl = new URL('../src/assets/world-country-labels.json', import.meta.url);
const response = await fetch(sourceUrl, { signal: AbortSignal.timeout(120_000) });
if (!response.ok) {
  throw new Error(`Natural Earth download failed: HTTP ${response.status}`);
}

const source = await response.json();
if (source.type !== 'FeatureCollection' || !Array.isArray(source.features)) {
  throw new Error('Natural Earth source is not a GeoJSON FeatureCollection');
}

const labels = new Map();
for (const feature of source.features) {
  const properties = feature.properties ?? {};
  const primaryIso = properties.ISO_A2;
  const alternateIso = properties.ISO_A2_EH;
  const isPrimary = typeof primaryIso === 'string' && /^[A-Z]{2}$/.test(primaryIso);
  const iso = isPrimary ? primaryIso : alternateIso;
  if (typeof iso !== 'string' || !/^[A-Z]{2}$/.test(iso)) continue;

  const latitude = properties.LABEL_Y;
  const longitude = properties.LABEL_X;
  if (
    !Number.isFinite(latitude) || Math.abs(latitude) > 90 ||
    !Number.isFinite(longitude) || Math.abs(longitude) > 180
  ) continue;

  const en = properties.NAME_EN;
  const zh = properties.NAME_ZH;
  if (typeof en !== 'string' || !en.trim() || typeof zh !== 'string' || !zh.trim()) {
    throw new Error(`Natural Earth label ${iso} is missing its English or Chinese name`);
  }

  // Prefer the main ISO record over a territory sharing its fallback ISO code.
  const existing = labels.get(iso);
  if (existing && (existing.isPrimary || !isPrimary)) continue;
  labels.set(iso, {
    isPrimary,
    label: { iso, en, zh, latitude, longitude },
  });
}

const entries = [...labels.values()]
  .map(({ label }) => label)
  .sort((a, b) => a.iso < b.iso ? -1 : a.iso > b.iso ? 1 : 0);
if (entries.length === 0) {
  throw new Error('Natural Earth source produced no valid country labels');
}

const output = `${JSON.stringify(entries)}\n`;
const size = Buffer.byteLength(output);
if (size > 100_000) {
  throw new Error(`Country labels exceed the 100 KB limit: ${size} bytes`);
}
await writeFile(outputUrl, output, 'utf8');
console.log(`Generated ${entries.length} country labels (${size} bytes): ${fileURLToPath(outputUrl)}`);
