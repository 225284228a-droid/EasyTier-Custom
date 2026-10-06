import { writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

const sourceRevision = '9380cca83db5f9aef52d5e762765100745f84b27';

function geometryCoordinates(value) {
  if (!Array.isArray(value)) {
    throw new Error('Natural Earth geometry contains invalid coordinates');
  }
  if (typeof value[0] === 'number') {
    const [longitude, latitude] = value;
    if (
      !Number.isFinite(longitude) || Math.abs(longitude) > 180 ||
      !Number.isFinite(latitude) || Math.abs(latitude) > 90
    ) {
      throw new Error('Natural Earth geometry contains an invalid position');
    }
    return [Number(longitude.toFixed(6)), Number(latitude.toFixed(6))];
  }
  return value.map(geometryCoordinates);
}

for (const scale of ['50m', '10m']) {
  const name = `ne_${scale}_admin_0_countries`;
  const sourceUrl = `https://raw.githubusercontent.com/nvkelso/natural-earth-vector/${sourceRevision}/geojson/${name}.geojson`;
  const response = await fetch(sourceUrl, { signal: AbortSignal.timeout(120_000) });
  if (!response.ok) {
    throw new Error(`Natural Earth ${scale} download failed: HTTP ${response.status}`);
  }
  const source = await response.json();
  if (source.type !== 'FeatureCollection' || !Array.isArray(source.features)) {
    throw new Error(`Natural Earth ${scale} source is not a GeoJSON FeatureCollection`);
  }
  const features = source.features.map(feature => {
    const type = feature.geometry?.type;
    if (type !== 'Polygon' && type !== 'MultiPolygon') {
      throw new Error(`Natural Earth ${scale} source contains an unexpected geometry`);
    }
    return {
      type: 'Feature',
      properties: {},
      geometry: { type, coordinates: geometryCoordinates(feature.geometry.coordinates) },
    };
  });
  const outputUrl = new URL(`../src/assets/world-boundaries-${scale}.json`, import.meta.url);
  const output = `${JSON.stringify({ type: 'FeatureCollection', name, features })}\n`;
  await writeFile(outputUrl, output, 'utf8');
  console.log(`Generated ${features.length} ${scale} country boundaries (${Buffer.byteLength(output)} bytes): ${fileURLToPath(outputUrl)}`);
}
