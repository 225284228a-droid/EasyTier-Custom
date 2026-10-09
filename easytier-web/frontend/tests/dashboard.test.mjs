import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { spawn } from 'node:child_process';
import { mkdir } from 'node:fs/promises';
import { setTimeout as delay } from 'node:timers/promises';
import { chromium } from 'playwright';

const base = process.env.WEB_TEST_URL || 'http://127.0.0.1:5198';
const screenshotDir = process.env.WEB_SCREENSHOT_DIR;
let browser;
let server;

before(async () => {
    if (!process.env.WEB_TEST_URL) {
        server = spawn(process.execPath, ['node_modules/vite/bin/vite.js', 'preview', '--host', '127.0.0.1', '--port', '5198', '--strictPort'], { stdio: 'pipe' });
        for (let i = 0; i < 100; i++) {
            if (server.exitCode !== null) throw new Error('Vite exited before starting');
            if (await fetch(base).then(r => r.ok).catch(() => false)) break;
            await delay(100);
        }
    }
    browser = await chromium.launch({ headless: true });
    if (screenshotDir) await mkdir(screenshotDir, { recursive: true });
});

after(async () => {
    await browser?.close();
    server?.kill();
});

const uuid = n => ({ part1: 0, part2: 0, part3: 0, part4: n });
const id = n => `00000000-0000-0000-0000-${n.toString(16).padStart(12, '0')}`;

function fixture() {
    const machines = ['Amsterdam gateway', 'Build server', 'Office workstation', 'Storage node', 'Travel laptop', 'Windows desktop'].map((hostname, i) => ({
        client_url: `tcp://192.0.2.${i + 10}:11010`,
        public_ip: `192.0.2.${i + 10}`,
        online: false,
        info: { hostname, machine_id: uuid(i + 1), easytier_version: '2.4.5', report_time: '2026-09-13 12:00:00', running_network_instances: i === 0 ? [uuid(100)] : [] },
        location: { country: 'Netherlands', region: '', city: 'Amsterdam' },
        networks: [],
    }));
    const networks = ['Engineering', 'Home lab', 'Office network'].map((display_name, i) => ({
        network_id: `network-${i}`, display_name, network_name: `team-${i}`, networking_method: i === 0 ? 'Gateway' : 'Manual',
        online_member_count: 1, member_count: 2, network_secret: 'test-secret', peer_urls: i === 0 ? [] : ['tcp://192.0.2.1:11010'],
    }));
    const members = [{ member_id: id(101), device_id: id(1), hostname: 'Amsterdam gateway', hostname_override: null, virtual_ipv4: '10.126.126.1', online: true, running: true, version: '2.4.5', error_msg: null, runtime_virtual_ipv4: '10.126.126.1' }];
    return { machines, networks, members, memberConfig: {}, aclPolicy: { default_action: 'allow', rules: [] }, credentials: [], temporaryPeers: [], nodeRoutes: [], nodePeers: [], nodeAclStats: [], loggerConfig: { level: 'INFO' }, writes: [], requests: [], failures: new Set(), gatewayDelay: 0, gatewayEnabled: true };
}

async function open(t, route = '/h', options = {}, configure = () => {}) {
    const state = fixture();
    configure(state);
    const context = await browser.newContext({ viewport: { width: 1440, height: 960 }, ...options });
    t.after(() => context.close());
    await context.addInitScript(() => localStorage.setItem('lang', 'en'));
    if (state.noRandomUUID) {
        await context.addInitScript(() => {
            // Remote HTTP exposes getRandomValues but not randomUUID.
            Object.defineProperty(crypto, 'randomUUID', { value: undefined });
            const getRandomValues = crypto.getRandomValues.bind(crypto);
            window.secureRandomCalls = 0;
            crypto.getRandomValues = values => {
                window.secureRandomCalls++;
                return getRandomValues(values);
            };
        });
    }
    const page = await context.newPage();
    page.setDefaultTimeout(10000);
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    t.after(() => assert.deepEqual(errors, [], 'no uncaught browser errors'));
    await page.route('**/api_meta.js', route => route.fulfill({
        contentType: 'text/javascript',
        body: `window.apiMeta = ${JSON.stringify({ api_host: state.apiHost ?? '' })};`,
    }));
    await page.route('**/api/v1/**', async route => {
        const request = route.request();
        const path = new URL(request.url()).pathname.replace('/api/v1', '');
        const method = request.method();
        const payload = request.postDataJSON();
        state.requests.push({ path, method });
        if (method !== 'GET') state.writes.push({ path, method, payload });
        if (state.failures.has(path)) return route.fulfill({ status: 503, json: { message: 'Test unavailable' } });
        let result = {};
        if (path === '/summary') result = { device_count: state.machines.length };
        else if (path === '/console-info') result = { username: 'test-user', config_server_protocol: 'udp', config_server_port: 22020, webhook_auth: state.externalConsole ?? false };
        else if (path === '/machines') result = { machines: state.machines };
        else if (path === '/networks/gateway-info') {
            await delay(state.gatewayDelay);
            result = { enabled: state.gatewayEnabled, peer_url: 'tcp://192.0.2.1:11010', relay_data: true };
        } else if (path === '/networks') {
            if (method === 'POST') {
                result = { ...payload.settings, network_id: `created-${state.networks.length}`, network_secret: 'new-secret', member_count: 0, online_member_count: 0 };
                state.networks.push(result);
            } else result = { networks: state.networks };
        } else if (/^\/networks\/[^/]+$/.test(path)) {
            const network = state.networks.find(n => n.network_id === path.split('/')[2]);
            if (method === 'PATCH') Object.assign(network, payload.settings, payload.network_secret ? { network_secret: payload.network_secret } : {});
            if (method === 'DELETE') state.networks = state.networks.filter(n => n !== network);
            result = network ?? {};
        } else if (path.endsWith('/acl-policy')) {
            if (method === 'PUT') state.aclPolicy = payload;
            result = { policy: state.aclPolicy };
        } else if (/^\/networks\/[^/]+\/members$/.test(path)) {
            if (method === 'POST') payload.device_ids.forEach(device_id => state.members.push({ device_id, hostname: 'Added device', online: true }));
            result = { members: state.members, temporary_peers: state.temporaryPeers };
        } else if (/^\/networks\/[^/]+\/members\/[^/]+\/config$/.test(path)) {
            if (method === 'PUT') state.memberConfig = payload.config;
            result = method === 'GET' ? state.memberConfig : state.members[0];
        } else if (/^\/networks\/[^/]+\/members\//.test(path)) {
            const member = state.members.find(m => m.device_id === path.split('/').at(-1));
            if (method === 'PATCH') Object.assign(member, payload);
            if (method === 'DELETE') state.members = state.members.filter(m => m !== member);
            result = member ?? {};
        } else if (/^\/networks\/[^/]+\/credentials$/.test(path)) {
            result = { credentials: state.credentials };
        } else if (path.endsWith('/proxy-rpc')) {
            if (payload.method_name === 'list_route') result = { routes: state.nodeRoutes };
            else if (payload.method_name === 'list_peer') result = { peer_infos: state.nodePeers };
            else if (payload.method_name === 'get_acl_stats') result = { acl_stats: { rules: state.nodeAclStats } };
            else if (payload.method_name === 'get_logger_config') result = state.loggerConfig;
            else if (payload.method_name === 'set_logger_config') state.loggerConfig = { level: ['DISABLED', 'ERROR', 'WARNING', 'INFO', 'DEBUG', 'TRACE'][payload.payload.level] };
            else if (payload.method_name === 'show_node_info') result = { node_info: { config: '[instance]\nname = "Engineering"' } };
        } else if (/\/machines\/[^/]+\/networks$/.test(path)) result = { running_inst_ids: [uuid(100)], disabled_inst_ids: [] };
        else if (path.endsWith('/networks/info')) result = { info: { map: state.machineNetworkInfo ?? {} } };
        else if (path.endsWith('/networks/metas')) result = { metas: { [id(100)]: { network_name: 'Engineering', config_permission: 7 } } };
        else if (path.includes('/networks/info/')) result = { info: { map: { [id(100)]: { error_msg: 'Test device is reconnecting' } } } };
        else if (path.includes('/networks/config/')) result = { instance_id: id(100), network_name: 'Engineering', hostname: 'Amsterdam gateway', networking_method: 'Standalone' };
        await route.fulfill({ json: result });
    });
    await page.goto(`${base}/#${route}`);
    await page.locator('.console-page').waitFor();
    return { page, state, context };
}

async function refresh(page) {
    await page.getByRole('button', { name: 'Refresh', exact: true }).click();
}

test('overview uses real counts and recovers from partial refresh failures', async t => {
    const { page, state } = await open(t);
    await page.waitForFunction(() => document.querySelectorAll('.summary-value')[2]?.textContent.trim() === '3');
    assert.deepEqual(await page.locator('.summary-value').allTextContents().then(values => values.map(s => s.trim())), ['6', '0', '3', '1']);
    state.machines[0].online = true;
    await refresh(page);
    await page.waitForFunction(() => document.querySelectorAll('.summary-value')[1]?.textContent.trim() === '1');
    assert.equal(await page.locator('.dashboard-device-card').count(), 6);
    state.failures.add('/networks');
    state.machines.pop();
    // Exercise a partial failure during the next automatic refresh.
    await page.getByText('Unable to refresh data.', { exact: false }).waitFor();
    assert.equal((await page.locator('.summary-value').nth(2).textContent()).trim(), '3');
    await page.waitForFunction(() => document.querySelector('.summary-value')?.textContent.trim() === '5');
    state.failures.clear();
    state.networks.pop();
    await page.waitForFunction(() => document.querySelectorAll('.summary-value')[2]?.textContent.trim() === '2');
    assert.equal(await page.getByText('Unable to refresh data.', { exact: false }).count(), 0);
});

test('dashboard devices default to compact cards, retain list choice and open the selected device', async t => {
    const { page, state } = await open(t);
    const devicePanel = page.locator('.overview-columns > section').first();
    const cards = devicePanel.locator('.dashboard-device-card');
    const rows = devicePanel.locator('.preview-row');
    const view = devicePanel.locator('.dashboard-device-view');
    await cards.first().waitFor();
    assert.equal(await cards.count(), 6);
    const names = state.machines.map(machine => machine.info.hostname);
    assert.deepEqual(await cards.locator('.preview-name').allTextContents(), names);
    for (let index = 0; index < names.length; index++) {
        assert.equal((await cards.nth(index).innerText()).replace(/\s+/g, ' ').trim(),
            `${names[index]} ${state.machines[index].public_ip}`,
            'dashboard cards show only the device name and public IP');
    }
    await view.getByRole('button', { name: 'List view', exact: true }).click();
    await rows.first().waitFor();
    assert.equal(await cards.count(), 0);
    assert.deepEqual(await rows.locator('.preview-name').allTextContents(), names);
    assert.equal(await rows.locator('.preview-number').count(), 6, 'list mode retains instance counts');
    await page.reload();
    await rows.first().waitFor();
    assert.deepEqual(await rows.locator('.preview-name').allTextContents(), names, 'list choice survives reload');
    await view.getByRole('button', { name: 'Card view', exact: true }).click();
    await cards.first().waitFor();
    assert.equal(await rows.count(), 0);
    await cards.first().click();
    await page.waitForURL(`**/device/${id(1)}/${id(100)}`);
    await page.locator('.console-device-drawer h2').filter({ hasText: names[0] }).waitFor();
    await page.goBack();
    await cards.first().waitFor();
    assert.deepEqual(await cards.locator('.preview-name').allTextContents(), names, 'returning to the dashboard retains cards');
});

test('dashboard limits device previews to six list entries or five responsive card rows', async t => {
    const { page, state } = await open(t, '/h', {}, state => {
        const machine = state.machines[0];
        state.machines = Array.from({ length: 100 }, (_, index) => ({
            ...machine,
            public_ip: `192.0.2.${index + 1}`,
            info: { ...machine.info, hostname: `Device ${String(index + 1).padStart(3, '0')}`,
                machine_id: uuid(index + 1), running_network_instances: [] },
        }));
    });
    const devicePanel = page.locator('.overview-columns > section').first();
    const view = devicePanel.locator('.dashboard-device-view');
    const rows = devicePanel.locator('.preview-row');
    const expectedNames = state.machines.map(machine => machine.info.hostname);
    const assertCards = async width => {
        await page.waitForFunction(total => {
            const grid = document.querySelector('.dashboard-device-grid');
            if (!grid) return false;
            const columns = getComputedStyle(grid).gridTemplateColumns.trim().split(/\s+/).length;
            return grid.children.length === Math.min(total, columns * 5);
        }, state.machines.length);
        const layout = await devicePanel.evaluate(panel => {
            const grid = panel.querySelector('.dashboard-device-grid');
            const cards = [...grid.children];
            const panelBounds = panel.getBoundingClientRect();
            return {
                columns: getComputedStyle(grid).gridTemplateColumns.trim().split(/\s+/).length,
                rows: new Set(cards.map(card => Math.round(card.getBoundingClientRect().top))).size,
                names: cards.map(card => card.querySelector('.preview-name').textContent),
                contained: cards.every(card => {
                    const bounds = card.getBoundingClientRect();
                    return bounds.left >= panelBounds.left && bounds.right <= panelBounds.right
                        && bounds.bottom <= panelBounds.bottom;
                }),
            };
        });
        assert.equal(layout.rows, 5, `${width}: card preview fills exactly five rows`);
        assert.deepEqual(layout.names, expectedNames.slice(0, layout.columns * 5), `${width}: cards retain the sorted prefix`);
        assert.ok(layout.contained, `${width}: every card is inside its panel`);
    };
    for (const width of [1440, 1101, 1100, 1024, 390, 280, 1440]) {
        await page.setViewportSize({ width, height: 960 });
        await assertCards(width);
        const [devices, networks] = await page.locator('.overview-columns > section').evaluateAll(panels => panels.map(panel => {
            const bounds = panel.getBoundingClientRect();
            return { top: bounds.top, bottom: bounds.bottom, left: bounds.left, width: bounds.width };
        }));
        if (width > 1100) {
            assert.ok(Math.abs(devices.bottom - networks.bottom) <= 1, `${width}: panel bottoms align`);
            assert.ok(Math.abs(devices.width - networks.width * 1.5) <= 1, `${width}: device panel is three fifths of the available width`);
        } else {
            assert.ok(networks.top > devices.bottom, `${width}: panels stack vertically`);
            assert.ok(Math.abs(devices.left - networks.left) <= 1 && Math.abs(devices.width - networks.width) <= 1,
                `${width}: stacked panels use the same width`);
        }
    }
    await view.getByRole('button', { name: 'List view', exact: true }).click();
    await rows.first().waitFor();
    assert.deepEqual(await rows.locator('.preview-name').allTextContents(), expectedNames.slice(0, 6));
    const bottoms = await page.locator('.overview-columns > section').evaluateAll(panels => panels.map(panel => panel.getBoundingClientRect().bottom));
    assert.ok(Math.abs(bottoms[0] - bottoms[1]) <= 1, 'list mode keeps panel bottoms aligned');
    await page.setViewportSize({ width: 390, height: 960 });
    assert.equal(await rows.count(), 6, 'list limit is independent of the available width');
    await view.getByRole('button', { name: 'Card view', exact: true }).click();
    await assertCards(390);
    await page.setViewportSize({ width: 1440, height: 960 });
    await assertCards(1440);
    assert.equal(await devicePanel.getByRole('link', { name: 'View all', exact: true }).count(), 1);
});

test('dashboard panels stay aligned with empty or short device and network previews', async t => {
    const { page, state } = await open(t);
    await page.locator('.dashboard-device-card').first().waitFor();
    const machines = state.machines;
    const networks = state.networks;
    for (const [deviceCount, networkCount] of [[0, 0], [2, 0], [0, 3]]) {
        state.machines = machines.slice(0, deviceCount);
        state.networks = networks.slice(0, networkCount);
        await refresh(page);
        await page.waitForFunction(({ deviceCount, networkCount }) => {
            const panels = document.querySelectorAll('.overview-columns > section');
            return panels.length === 2
                && panels[0].querySelectorAll('.dashboard-device-card').length === deviceCount
                && panels[1].querySelectorAll('.preview-row').length === networkCount
                && panels[0].querySelectorAll('.console-empty-state').length === Number(deviceCount === 0)
                && panels[1].querySelectorAll('.console-empty-state').length === Number(networkCount === 0);
        }, { deviceCount, networkCount });
        const layout = await page.locator('.overview-columns > section').evaluateAll(panels => panels.map(panel => {
            const bounds = panel.getBoundingClientRect();
            return { bottom: bounds.bottom, contained: [...panel.children].every(child => child.getBoundingClientRect().bottom <= bounds.bottom) };
        }));
        assert.ok(Math.abs(layout[0].bottom - layout[1].bottom) <= 1, `${deviceCount}/${networkCount}: panel bottoms align`);
        assert.ok(layout.every(panel => panel.contained), `${deviceCount}/${networkCount}: panel content is not clipped`);
    }
});

test('dashboard topology shares the panel heading inset and documentation links open this fork', async t => {
    const { page } = await open(t, '/h', {}, state => { state.machines = []; });
    const repository = 'https://github.com/225284228a-droid/EasyTier-Custom';
    await page.locator('.overview-columns .console-empty-state a.entity-link').waitFor();
    assert.equal(await page.locator('.console-docs').getAttribute('href'), repository);
    assert.equal(await page.locator('.overview-columns .console-empty-state a.entity-link').getAttribute('href'), repository);
    await page.locator('.dashboard .topology-header').waitFor();
    for (const width of [1440, 390]) {
        await page.setViewportSize({ width, height: 960 });
        const bounds = await page.locator('.dashboard').evaluate(panel => {
            const outer = panel.getBoundingClientRect();
            const heading = panel.querySelector('.section-heading h2').getBoundingClientRect();
            const summary = panel.querySelector('.dashboard-summary').getBoundingClientRect();
            const toolbar = panel.querySelector('.topology-header').getBoundingClientRect();
            return { outer: { left: outer.left, right: outer.right }, heading: heading.left,
                summary: { left: summary.left, right: summary.right }, toolbar: toolbar.left };
        });
        assert.ok(Math.abs(bounds.summary.left - bounds.heading) <= 1,
            `${width}: running-network summary is not aligned with the panel heading`);
        assert.ok(Math.abs(bounds.toolbar - bounds.heading) <= 1,
            `${width}: topology title and controls are not aligned with the panel heading`);
        assert.ok(bounds.summary.left > bounds.outer.left + 8 && bounds.summary.right < bounds.outer.right - 8,
            `${width}: topology content touches the panel edges`);
    }
});

test('dashboard globe cards zoom without scrolling and load only complete rows', async t => {
    const { page, state } = await open(t, '/h', {}, state => {
        state.machines = state.machines.slice(0, 1);
        state.machines[0].online = true;
        state.machines[0].info.running_network_instances = Array.from({ length: 24 }, (_, index) => uuid(100 + index));
        state.machineNetworkInfo = Object.fromEntries(Array.from({ length: 24 }, (_, index) => [id(100 + index), {
            running: true, network_name: 'wheel-test',
            my_node_info: { peer_id: index + 1, hostname: `Globe node ${String(index + 1).padStart(2, '0')}` },
            node_location: { country: 'China', city: 'Hong Kong', latitude: 22.3, longitude: 114.17 },
            peers: [], routes: [],
        }]));
    });
    page.setDefaultTimeout(60_000);
    const canvas = page.locator('.globe-stage canvas');
    await canvas.waitFor();
    await page.waitForFunction(() => Number(document.querySelector('.globe-stage canvas')?.dataset.globeDistance) > 0);
    await page.getByRole('button', { name: 'Pause Rotation', exact: true }).click();
    await page.locator('.node-row').first().click();
    await page.evaluate(() => {
        const stage = document.querySelector('.globe-stage').getBoundingClientRect();
        window.scrollTo(0, stage.top + window.scrollY - 400);
    });
    const settleWheel = () => page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    const distance = () => canvas.evaluate(element => Number(element.dataset.globeDistance));
    const scrollY = () => page.evaluate(() => window.scrollY);
    const hoverCanvas = () => canvas.hover({ position: { x: 20, y: 20 } });
    const initialScroll = await scrollY();
    const initialDistance = await distance();
    const [minimum, maximum] = await canvas.evaluate(element => [Number(element.dataset.globeMinDistance), Number(element.dataset.globeMaxDistance)]);
    assert.ok(initialScroll > 100, 'the page can scroll upward while the globe is visible');
    assert.ok(await page.evaluate(() => document.documentElement.scrollHeight - innerHeight - scrollY > 20),
        'the page can also scroll downward while the globe is visible');
    await hoverCanvas();
    await page.mouse.wheel(0, -250);
    await page.waitForFunction(before => Number(document.querySelector('.globe-stage canvas')?.dataset.globeDistance) < before, initialDistance);
    const nearerDistance = await distance();
    assert.ok(nearerDistance > minimum, 'ordinary zoom stays within the zoom range');
    await page.mouse.wheel(0, 150);
    await page.waitForFunction(before => Number(document.querySelector('.globe-stage canvas')?.dataset.globeDistance) > before, nearerDistance);
    assert.equal(await scrollY(), initialScroll, 'ordinary globe zoom does not scroll the page');
    await page.mouse.wheel(0, -5000);
    await page.waitForFunction(min => Math.abs(Number(document.querySelector('.globe-stage canvas')?.dataset.globeDistance) - min) < 1e-6, minimum);
    await page.mouse.wheel(0, -300);
    await settleWheel();
    assert.equal(await scrollY(), initialScroll, 'further zoom-in at the canvas limit does not scroll the page');
    assert.ok(Math.abs(await distance() - minimum) < 1e-6);

    const stack = page.locator('.globe-node-stack:visible');
    await stack.waitFor();
    const assertCompleteCards = async () => {
        const cards = await page.locator('.globe-labels').evaluate(layer => {
            const stacks = [...layer.querySelectorAll('.globe-label-stack')];
            return { count: layer.querySelectorAll('.globe-node-label').length,
                diagnostics: stacks.map(element => {
                    const outer = element.getBoundingClientRect();
                    return { bounds: outer.toJSON(), scrollHeight: element.scrollHeight, clientHeight: element.clientHeight,
                        clipped: [...element.children].flatMap(child => {
                            const row = child.getBoundingClientRect();
                            return row.top < outer.top - 0.1 || row.bottom > outer.bottom + 0.1
                                || row.left < outer.left - 0.1 || row.right > outer.right + 0.1
                                ? [{ text: child.textContent, bounds: row.toJSON() }] : [];
                        }).slice(0, 3) };
                }),
                complete: [...layer.querySelectorAll('.globe-node-label, .globe-traffic-label')]
                    .every(card => getComputedStyle(card).display !== 'none' && card.closest('.globe-label-stack'))
                    && stacks.every(element => {
                    const outer = element.getBoundingClientRect();
                    return getComputedStyle(element).display !== 'none'
                        && element.scrollHeight <= element.clientHeight
                        && [...element.children].every(child => {
                            const row = child.getBoundingClientRect();
                            return getComputedStyle(child).display !== 'none' && row.height > 0
                                && row.top >= outer.top - 0.1 && row.bottom <= outer.bottom + 0.1
                                && row.left >= outer.left - 0.1 && row.right <= outer.right + 0.1;
                        });
                }) };
        });
        assert.ok(cards.complete, `every mounted card fits completely without internal scrolling or hidden cards: ${JSON.stringify(cards.diagnostics)}`);
        return cards.count;
    };
    await assertCompleteCards();
    const card = stack.locator('.globe-node-label').first();
    await card.hover();
    const cardLimitScroll = await scrollY();
    await page.mouse.wheel(0, -300);
    await settleWheel();
    assert.equal(await scrollY(), cardLimitScroll, 'card zoom-in at the minimum does not scroll the page');
    assert.ok(Math.abs(await distance() - minimum) < 1e-6);
    await card.hover();
    const cardOutScroll = await scrollY();
    await page.mouse.wheel(0, 120);
    await page.waitForFunction(min => Number(document.querySelector('.globe-stage canvas')?.dataset.globeDistance) > min, minimum);
    const cardDistance = await distance();
    assert.equal(await scrollY(), cardOutScroll, 'node card zoom-out does not scroll the page');
    await card.hover();
    const cardInScroll = await scrollY();
    await page.mouse.wheel(0, -120);
    await page.waitForFunction(before => Number(document.querySelector('.globe-stage canvas')?.dataset.globeDistance) < before, cardDistance);
    assert.equal(await scrollY(), cardInScroll, 'node card zoom-in does not scroll the page');

    await hoverCanvas();
    await page.mouse.wheel(0, -5000);
    await page.waitForFunction(min => Math.abs(Number(document.querySelector('.globe-stage canvas')?.dataset.globeDistance) - min) < 1e-6, minimum);
    const desktopCount = await assertCompleteCards();
    await page.setViewportSize({ width: 390, height: 960 });
    await page.waitForFunction(previous => document.querySelectorAll('.globe-node-label').length < previous, desktopCount);
    const mobileCount = await assertCompleteCards();
    assert.ok(mobileCount > 0 && mobileCount < 24, 'crowded mobile labels load a complete subset of the fixture');
    await page.setViewportSize({ width: 1440, height: 960 });
    await page.waitForFunction(previous => document.querySelectorAll('.globe-node-label').length > previous, mobileCount);
    await assertCompleteCards();
    await page.evaluate(() => {
        const stage = document.querySelector('.globe-stage').getBoundingClientRect();
        window.scrollTo(0, stage.top + window.scrollY - 400);
    });

    state.machines[0].info.running_network_instances = [uuid(100)];
    state.machineNetworkInfo = { [id(100)]: state.machineNetworkInfo[id(100)] };
    await page.waitForFunction(() => {
        const stack = document.querySelector('.globe-node-stack');
        return stack?.children.length === 1 && stack.scrollHeight === stack.clientHeight;
    });
    await assertCompleteCards();
    await stack.locator('.globe-node-label').hover();
    const singleDistance = await distance();
    const singleScroll = await scrollY();
    await page.mouse.wheel(0, 120);
    await page.waitForFunction(before => Number(document.querySelector('.globe-stage canvas')?.dataset.globeDistance) > before, singleDistance);
    assert.equal(await scrollY(), singleScroll, 'a single card zooms without scrolling the page');

    await hoverCanvas();
    const limitScroll = await scrollY();
    await page.mouse.wheel(0, 5000);
    await page.waitForFunction(max => Math.abs(Number(document.querySelector('.globe-stage canvas')?.dataset.globeDistance) - max) < 1e-6, maximum);
    await page.mouse.wheel(0, 300);
    await settleWheel();
    assert.equal(await scrollY(), limitScroll, 'further zoom-out at the canvas limit does not scroll the page');
    assert.ok(Math.abs(await distance() - maximum) < 1e-6);
    const canvasBounds = await canvas.boundingBox();
    await page.mouse.move(canvasBounds.x - 12, canvasBounds.y + 40);
    await page.mouse.wheel(0, -250);
    await page.waitForFunction(before => window.scrollY < before, limitScroll);
    assert.ok(Math.abs(await distance() - maximum) < 1e-6, 'scrolling outside the globe leaves the zoom unchanged');
});

test('removed local configuration page falls back to the dashboard without a navigation entry', async t => {
    const { page, state } = await open(t, '/h/local-configs');
    await page.waitForURL(url => url.hash === '#/h');
    await page.locator('.summary-value').first().waitFor();
    assert.equal(await page.getByRole('link', { name: 'Local configurations', exact: true }).count(), 0);
    assert.equal(state.requests.some(request => request.path === '/local-configs'), false);
});

test('central node detail uses live directional bandwidth and combined traffic totals', async t => {
    const { page } = await open(t, `/h/networks/${id(100)}`, {}, state => {
        state.networks[0].network_id = id(100);
        state.nodePeers = [{ peer_id: 2, conns: [{ conn_id: 'live', tunnel: { tunnel_type: 'wss' }, stats: { latency_us: '1000', bandwidth_estimate_version: 1, estimated_tx_bps: '1000000', tx_bytes: '2048', rx_bytes: '1024' } },
            { conn_id: 'old', is_closed: true, stats: { bandwidth_estimate_version: 1, estimated_rx_bps: '9000000' } }] }];
    });
    await page.getByRole('button', { name: 'Node Detail', exact: true }).click();
    const peers = page.locator('.node-drawer-desktop-peers');
    await peers.getByText('Upload: 1.00 Mbit/s', { exact: true }).waitFor();
    await peers.getByText('Download: --', { exact: true }).waitFor();
    await peers.getByText('Upload: 2.0 KiB', { exact: true }).waitFor();
    await peers.getByText('Download: 1.0 KiB', { exact: true }).waitFor();
});

test('enrollment command uses the console info response', async t => {
    const { page } = await open(t);
    await page.getByRole('button', { name: 'Device Enrollment', exact: true }).click();
    await page.getByText('easytier-core --config-server udp://127.0.0.1:22020/test-user').waitFor();
});

for (const [apiHost, hostname] of [
    ['https://api.example.test:8443/', 'api.example.test'],
    ['http://[2001:db8::1]:8848/', '[2001:db8::1]'],
    ['.', '127.0.0.1'],
]) {
    test(`enrollment command uses the configured API hostname for ${apiHost}`, async t => {
        const { page } = await open(t, '/h', {}, state => { state.apiHost = apiHost; });
        await page.getByRole('button', { name: 'Device Enrollment', exact: true }).click();
        await page.getByText(`easytier-core --config-server udp://${hostname}:22020/test-user`).waitFor();
    });
}

test('device search, sort, expansion and routed drawer survive reload and history', async t => {
    const { page } = await open(t, '/h/deviceList');
    const table = page.locator('.desktop-list');
    await table.getByRole('button', { name: 'Amsterdam gateway', exact: true }).waitFor();
    await page.getByRole('textbox', { name: 'Search name or address' }).fill('192.0.2.10');
    assert.equal(await table.locator('tbody > tr').count(), 1);
    await page.getByRole('textbox', { name: 'Search name or address' }).fill('');
    // 无静态 sortField：离线排最后的预排序是默认视图；点击 hostname 列头一次升序、再点一次降序
    await page.getByRole('columnheader', { name: 'Hostname' }).click(); // 升序
    await page.getByRole('columnheader', { name: 'Hostname' }).click(); // 降序
    assert.match(await table.locator('tbody > tr').first().textContent(), /Windows desktop/);
    await table.locator('tbody > tr').first().getByRole('button').first().click();
    await table.locator('.device-details').waitFor();
    await page.getByRole('textbox', { name: 'Search name or address' }).fill('192.0.2.');
    const tableElement = await table.elementHandle();
    await table.getByRole('button', { name: 'Amsterdam gateway', exact: true }).click();
    await page.locator('.console-device-drawer').waitFor();
    assert.ok(page.url().endsWith(`/device/${id(1)}/${id(100)}`));
    assert.ok(await tableElement.evaluate(element => element === document.querySelector('.desktop-list')), 'opening a device preserves the list');
    assert.equal(await page.getByRole('textbox', { name: 'Search name or address' }).inputValue(), '192.0.2.');
    assert.match(await table.locator('tbody > tr').first().textContent(), /Windows desktop/);
    assert.equal(await table.locator('.device-details').count(), 1);
    await page.reload();
    await page.locator('.console-device-drawer h2').filter({ hasText: 'Amsterdam gateway' }).waitFor();
    await page.keyboard.press('Escape');
    await page.waitForURL('**/#/h/deviceList');
    await page.goBack();
    await page.locator('.console-device-drawer').waitFor();
});

test('switching the viewed network and device preserves the list and open drawer', async t => {
    const { page } = await open(t, '/h/deviceList');
    await page.route('**/machines/*/networks', route => route.fulfill({ json: {
        running_inst_ids: [uuid(100), uuid(101)], disabled_inst_ids: [],
    } }));
    await page.route('**/machines/*/networks/metas', route => route.fulfill({ json: { metas: {
        [id(100)]: { network_name: 'Engineering', config_permission: 7 },
        [id(101)]: { network_name: 'Home lab', config_permission: 7 },
    } } }));
    const table = page.locator('.desktop-list');
    await table.getByRole('button', { name: 'Amsterdam gateway', exact: true }).click();
    const drawer = page.locator('.console-device-drawer');
    await drawer.waitFor();
    const tableElement = await table.elementHandle();
    const drawerElement = await drawer.elementHandle();
    await drawer.locator('#dd-inst-id').click();
    await page.getByRole('option').filter({ hasText: id(101) }).click();
    await page.waitForURL(`**/device/${id(1)}/${id(101)}`);
    assert.ok(await tableElement.evaluate(element => element === document.querySelector('.desktop-list')), 'switching instances preserves the list');
    assert.ok(await drawerElement.evaluate(element => element === document.querySelector('.console-device-drawer')), 'switching instances preserves the open drawer');

    // Direct navigation also reuses the drawer while replacing its device content.
    await page.evaluate(hash => { location.hash = hash; }, `/h/deviceList/device/${id(2)}/${id(100)}`);
    await drawer.locator('h2').filter({ hasText: 'Build server' }).waitFor();
    assert.ok(await tableElement.evaluate(element => element === document.querySelector('.desktop-list')), 'switching devices preserves the list');
    assert.ok(await drawerElement.evaluate(element => element === document.querySelector('.console-device-drawer')), 'switching devices preserves the open drawer');
    await page.goBack();
    await drawer.locator('h2').filter({ hasText: 'Amsterdam gateway' }).waitFor();
    assert.ok(await drawerElement.evaluate(element => element === document.querySelector('.console-device-drawer')), 'history navigation preserves the open drawer');
});

test('device list card view toggle renders cards and persists across reload', async t => {
    const { page } = await open(t, '/h/deviceList');
    const table = page.locator('.desktop-list');
    await table.getByRole('button', { name: 'Amsterdam gateway', exact: true }).waitFor();
    assert.equal(await page.locator('.device-card').count(), 0);
    await page.locator('.view-toggle .pi-th-large').click();
    await page.locator('.device-card').first().waitFor();
    assert.equal(await page.locator('.device-card').count(), 6);
    assert.equal(await table.count(), 0);
    await page.reload();
    await page.locator('.device-card').first().waitFor();
    assert.equal(await page.locator('.device-card').count(), 6);
    await page.locator('.view-toggle .pi-table').click();
    await table.getByRole('button', { name: 'Amsterdam gateway', exact: true }).waitFor();
    assert.equal(await table.locator('tbody > tr').count(), 6);
});

test('network creation retains gateway and advanced standalone modes', async t => {
    const { page, state } = await open(t, '/h/networks');
    await page.locator('.desktop-list').getByRole('button', { name: 'Engineering', exact: true }).waitFor();
    await page.getByRole('textbox', { name: 'Search networks' }).fill('no-match');
    await page.getByText('No matching results', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Clear search' }).click();
    await page.getByRole('button', { name: 'Create Network', exact: true }).click();
    await page.getByRole('dialog').getByText('Secure Mode', { exact: true }).waitFor();
    await page.getByRole('dialog').locator('label[for="create-secure-mode"] + .pi-question-circle').hover();
    await page.getByText('Noise encrypted handshakes with identity verification; enables temporary credentials. Member instances restart on change').waitFor();
    await page.locator('#network-display-name').fill('Research');
    await page.getByRole('dialog').getByRole('button', { name: 'Confirm', exact: true }).click();
    await page.waitForURL('**/networks/created-*');
    assert.equal(state.writes.find(w => w.path === '/networks')?.payload.settings.networking_method, 'Gateway');
    await page.getByRole('button', { name: 'Back to networks' }).click();
    await page.getByRole('button', { name: 'Create Network', exact: true }).click();
    await page.getByRole('dialog').getByRole('button', { name: /Advanced/ }).click();
    await page.getByRole('dialog').locator('.p-button-danger:visible').click();
    await page.locator('#network-display-name').fill('Isolated lab');
    await page.getByRole('dialog').getByRole('button', { name: 'Confirm', exact: true }).click();
    await page.waitForURL('**/networks/created-*');
    assert.equal(state.writes.filter(w => w.path === '/networks').at(-1).payload.settings.networking_method, 'Standalone');
});

test('PublicServer settings retain discovery mode when renamed or edited to the gateway URL', async t => {
    const { page, state } = await open(t, '/h/networks/network-0', {}, state => {
        Object.assign(state.networks[0], {
            networking_method: 'PublicServer',
            public_server_url: 'tcp://public.example:11010',
        });
    });
    await page.getByRole('tab', { name: 'Settings', exact: true }).click();
    await page.locator('#settings-display-name').fill('Public network');
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await page.getByRole('heading', { name: 'Public network', exact: true }).waitFor();
    let settings = state.writes.filter(w => w.method === 'PATCH').at(-1).payload.settings;
    assert.equal(settings.networking_method, 'PublicServer');
    assert.equal(settings.public_server_url, 'tcp://public.example:11010');
    assert.deepEqual(settings.peer_urls, []);

    await page.locator('#settings-initial-nodes .url-input-full input.grow').fill('192.0.2.1');
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await page.waitForResponse(response => response.request().method() === 'GET' && response.url().endsWith('/networks/network-0'));
    settings = state.writes.filter(w => w.method === 'PATCH').at(-1).payload.settings;
    assert.equal(settings.networking_method, 'PublicServer');
    assert.equal(settings.public_server_url, 'tcp://192.0.2.1:11010');
    assert.deepEqual(settings.peer_urls, []);
});

test('network tabs preserve gateway settings and unsaved input across polling', async t => {
    const { page, state } = await open(t, '/h/networks/network-0');
    await page.getByRole('tab', { name: 'Settings', exact: true }).click();
    await page.locator('#settings-display-name').fill('Engineering draft');
    const count = state.requests.filter(r => r.path === '/networks/network-0').length;
    for (let i = 0; i < 50 && state.requests.filter(r => r.path === '/networks/network-0').length <= count; i++) await delay(100);
    assert.ok(state.requests.filter(r => r.path === '/networks/network-0').length > count, 'polling continues');
    assert.equal(await page.locator('#settings-display-name').inputValue(), 'Engineering draft');
    await page.getByRole('tab', { name: 'Members', exact: true }).click();
    await page.locator('.desktop-list').getByText('Running', { exact: true }).waitFor();
    await page.getByRole('tab', { name: 'Settings', exact: true }).click();
    assert.equal(await page.locator('#settings-display-name').inputValue(), 'Engineering draft');
    await page.locator('#regenerate-secret').check();
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await page.getByRole('heading', { name: 'Engineering draft' }).waitFor();
    const saved = state.writes.find(w => w.method === 'PATCH');
    assert.equal(saved.payload.settings.networking_method, 'Gateway');
    assert.match(saved.payload.network_secret, /^[0-9a-f-]{36}$/);
    await page.getByRole('button', { name: 'Delete Network', exact: true }).click();
    await page.getByRole('alertdialog').getByRole('button', { name: 'Cancel', exact: true }).click();
    assert.equal(state.writes.some(w => w.method === 'DELETE'), false);
    await page.getByRole('button', { name: 'Delete Network', exact: true }).click();
    await page.getByRole('alertdialog').getByRole('button', { name: 'Confirm', exact: true }).click();
    await page.waitForURL('**/#/h/networks');
    assert.equal(state.networks.length, 2);
});

test('switching central networks resets the settings form to the selected network', async t => {
    const { page, state } = await open(t, '/h/networks/network-0');
    await page.getByRole('tab', { name: 'Settings', exact: true }).click();
    await page.locator('#settings-display-name').fill('Engineering draft');
    await page.locator('.console-sidebar a[href="#/h/networks/network-1"]').click();
    await page.getByRole('heading', { name: 'Home lab', exact: true }).waitFor();
    await page.getByRole('tab', { name: 'Settings', exact: true }).click();
    assert.equal(await page.locator('#settings-display-name').inputValue(), 'Home lab');
    assert.equal(state.writes.length, 0);
});

test('ACL rules and network secrets use secure randomness without randomUUID', async t => {
    const { page, state } = await open(t, '/h/networks/network-0', {}, state => { state.noRandomUUID = true; });
    assert.equal(await page.evaluate(() => typeof crypto.randomUUID), 'undefined');
    await page.getByRole('tab', { name: 'Access Control', exact: true }).click();
    await page.getByRole('button', { name: 'Add Rule', exact: true }).click();
    const editor = page.getByRole('complementary').filter({ has: page.locator('#acl-rule-name') });
    await editor.locator('#acl-rule-name').fill('Allow ping');
    for (const select of ['Search and choose the members that initiate access', 'Search and choose target members or subnets']) {
        await editor.getByText(select, { exact: true }).click();
        await page.getByRole('option', { name: 'All members', exact: true }).click();
        await page.keyboard.press('Escape');
    }
    await editor.getByRole('checkbox', { name: 'ICMP', exact: true }).check();
    await editor.getByRole('button', { name: 'Save Rule', exact: true }).click();
    await editor.waitFor({ state: 'detached' });
    await page.getByRole('button', { name: 'Save Policy', exact: true }).click();
    await page.getByText('Access policy saved', { exact: true }).waitFor();
    assert.match(state.aclPolicy.rules[0].id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);

    await page.getByRole('tab', { name: 'Settings', exact: true }).click();
    const randomCalls = await page.evaluate(() => window.secureRandomCalls);
    await page.locator('#regenerate-secret').check();
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await page.getByText('Config Saved', { exact: true }).waitFor();
    const saved = state.writes.find(write => write.method === 'PATCH');
    assert.match(saved.payload.network_secret, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    assert.ok(await page.evaluate(() => window.secureRandomCalls) > randomCalls);
});

test('a late member configuration response cannot overwrite the next member', async t => {
    const { page, state } = await open(t, '/h/networks/network-0', {}, state => {
        state.members.push({ ...state.members[0], member_id: id(102), device_id: id(2), hostname: 'Build server' });
    });
    let releaseFirst;
    const firstResponse = new Promise(resolve => { releaseFirst = resolve; });
    t.after(() => releaseFirst());
    const firstPath = `/networks/network-0/members/${id(1)}/config`;
    await page.route('**/networks/network-0/members/*/config', async route => {
        if (route.request().method() !== 'GET') return route.fallback();
        const first = route.request().url().endsWith(firstPath);
        if (first) await firstResponse;
        await route.fulfill({ json: { hostname: first ? 'configuration-a' : 'configuration-b' } });
    });
    const firstRequested = page.waitForRequest(request => request.url().endsWith(firstPath));
    await page.getByRole('row').filter({ hasText: 'Amsterdam gateway' }).getByRole('button', { name: 'Edit Member' }).click();
    await firstRequested;
    await page.getByRole('dialog').getByRole('button', { name: 'Cancel', exact: true }).click();
    await page.getByRole('dialog').waitFor({ state: 'detached' });
    await page.getByRole('row').filter({ hasText: 'Build server' }).getByRole('button', { name: 'Edit Member' }).click();
    await page.getByRole('dialog').getByRole('tab', { name: 'Advanced Config' }).click();
    await page.getByRole('dialog').getByRole('button', { name: 'Advanced Settings', exact: true }).click();
    await page.locator('#hostname').waitFor();
    assert.equal(await page.locator('#hostname').inputValue(), 'configuration-b');
    const lateResponse = page.waitForResponse(response => response.url().endsWith(firstPath));
    releaseFirst();
    await (await lateResponse).finished();
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    assert.equal(await page.locator('#hostname').inputValue(), 'configuration-b');
    await page.getByRole('dialog').getByRole('button', { name: 'Save', exact: true }).click();
    await page.getByRole('dialog').waitFor({ state: 'detached' });
    const saved = state.writes.find(write => write.method === 'PUT' && write.path.endsWith('/config'));
    assert.equal(saved.path, `/networks/network-0/members/${id(2)}/config`);
    assert.equal(saved.payload.config.hostname, 'configuration-b');
});

test('settings wait for gateway discovery before exposing the save action', async t => {
    const { page, state } = await open(t, '/h/networks/network-0', {}, state => { state.gatewayDelay = 1500; });
    await page.getByRole('tab', { name: 'Settings', exact: true }).click();
    assert.equal(await page.getByRole('button', { name: 'Save', exact: true }).count(), 0);
    await page.locator('#settings-display-name').waitFor();
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    for (let i = 0; i < 50 && !state.writes.some(w => w.method === 'PATCH'); i++) await delay(100);
    assert.equal(state.writes.find(w => w.method === 'PATCH').payload.settings.networking_method, 'Gateway');
});

test('editing an automatic member address keeps automatic assignment', async t => {
    const { page, state } = await open(t, '/h/networks/network-0', {}, state => {
        state.networks[0].virtual_cidr = '10.126.0.0/16';
        Object.assign(state.members[0], { virtual_ipv4: null, allocated_ipv4: '10.126.126.7', runtime_virtual_ipv4: null, online: false, running: false });
    });
    await page.locator('.desktop-list').getByText('10.126.126.7/16', { exact: true }).waitFor();
    await page.getByRole('row').filter({ hasText: 'Amsterdam gateway' }).getByRole('button', { name: 'Edit Member' }).click();
    assert.equal(await page.locator('#edit-virtual-ipv4').inputValue(), '');
    await page.locator('#edit-hostname-override').fill('automatic-member');
    await page.getByRole('dialog').getByRole('button', { name: 'Save', exact: true }).click();
    await page.getByRole('dialog').waitFor({ state: 'detached' });
    assert.equal(state.writes.find(write => write.method === 'PATCH').payload.virtual_ipv4, '');
});

test('member editing, adding and removal keep existing request semantics', async t => {
    const { page, state } = await open(t, '/h/networks/network-0');
    await page.getByRole('row').filter({ hasText: 'Amsterdam gateway' }).getByRole('button', { name: 'Edit Member' }).click();
    await page.locator('#edit-hostname-override').fill('gateway-west');
    await page.locator('#edit-virtual-ipv4').fill('10.126.126.20');
    await page.getByRole('dialog').getByRole('button', { name: 'Save', exact: true }).click();
    await page.getByRole('row').filter({ hasText: 'gateway-west' }).waitFor();
    assert.equal(state.members[0].virtual_ipv4, '10.126.126.20');
    await page.getByRole('button', { name: 'Add Devices' }).click();
    await page.getByRole('dialog').getByRole('row').filter({ hasText: 'Build server' }).getByRole('checkbox').check();
    await page.getByRole('dialog').getByRole('button', { name: 'Confirm', exact: true }).click();
    await page.getByRole('row').filter({ hasText: 'Added device' }).waitFor();
    assert.deepEqual(state.writes.find(w => w.method === 'POST' && w.path.endsWith('/members')).payload.device_ids, [id(2)]);
    await page.getByRole('row').filter({ hasText: 'Added device' }).getByRole('button', { name: 'Remove Device' }).click();
    await page.getByRole('alertdialog').getByRole('button', { name: 'Confirm', exact: true }).click();
    await page.getByRole('row').filter({ hasText: 'Added device' }).waitFor({ state: 'detached' });
    assert.equal(state.members.length, 1);
});

test('central member WireGuard clients stay editable through the member configuration', async t => {
    const { page, state } = await open(t, '/h/networks/network-0', {}, state => {
        state.memberConfig = {
            proxy_cidrs: ['192.168.20.0/24'],
            vpn_portal_config: {
                enabled: true,
                wireguard_listen: '0.0.0.0:22022',
                wireguard_private_key: 'KioqKioqKioqKioqKioqKioqKioqKioqKioqKioqKio=',
                clients: [{ name: 'phone', virtual_ip: '10.126.126.2/24', groups: [] }],
            },
        };
    });
    const key = state.memberConfig.vpn_portal_config.wireguard_private_key;
    const edit = async () => {
        await page.getByRole('button', { name: 'Edit Member' }).click();
        await page.getByRole('dialog').getByRole('tab', { name: 'Advanced Config' }).click();
        await page.getByRole('dialog').getByRole('button', { name: 'Advanced Settings', exact: true }).click();
        await page.locator('#vpn_portal_client_name_0').waitFor();
    };
    await edit();
    await page.getByRole('dialog').getByRole('button', { name: 'Add device', exact: true }).click();
    await page.locator('#vpn_portal_client_virtual_ip_1').fill('10.126.126.3/24');
    const generatedName = await page.locator('#vpn_portal_client_name_1').inputValue();
    assert.match(generatedName, /^device-[a-f0-9]{8}$/);
    await page.getByRole('dialog').getByRole('button', { name: 'Save', exact: true }).click();
    await page.getByRole('dialog').waitFor({ state: 'detached' });
    assert.equal(state.memberConfig.vpn_portal_config.clients.length, 2);
    assert.equal(state.memberConfig.vpn_portal_config.wireguard_private_key, key);
    assert.deepEqual(state.memberConfig.proxy_cidrs, ['192.168.20.0/24']);

    await edit();
    await page.getByRole('dialog').getByRole('button', { name: 'Delete device', exact: true }).first().click();
    await page.getByRole('dialog').getByRole('button', { name: 'Save', exact: true }).click();
    await page.getByRole('dialog').waitFor({ state: 'detached' });
    assert.deepEqual(state.memberConfig.vpn_portal_config.clients.map(client => client.name), [generatedName]);
    assert.equal(state.writes.filter(write => write.method === 'PUT' && write.path.endsWith('/config')).length, 2);
    assert.equal(state.writes.some(write => write.path.endsWith('/vpn-portal-clients') || write.payload?.method_name === 'patch_config'), false);
});

test('credentials tab and temporary devices render', async t => {
    const { page } = await open(t, '/h/networks/network-0', {}, state => {
        state.networks[0].secure_mode = true;
        const peers = [
            {
                peer_id: 42, credential_id: 'cred-1234567890abcdef', credential_expiry_unix: 1893456000,
                hostname: 'Visitor laptop', ipv4: '10.126.126.50', version: '2.4.5',
            },
            {
                peer_id: 99, credential_id: 'cred-1234567890abcdef', credential_expiry_unix: 1893456000,
                hostname: 'Visitor phone', ipv4: '10.126.126.51', version: '2.4.5',
            },
        ];
        state.credentials = [{
            credential_id: 'cred-1234567890abcdef', credential_secret: 'test-credential-secret',
            expiry_unix: 1893456000, reusable: true, online_peers: peers,
        }];
        state.temporaryPeers = peers;
    });
    await page.getByRole('tab', { name: 'Credentials', exact: true }).click();
    await page.getByRole('cell', { name: /cred-1234/ }).waitFor();
    await page.getByText('Visitor laptop').waitFor();
    await page.getByText('Visitor phone').waitFor();
    await page.getByRole('tab', { name: 'Members', exact: true }).click();
    await page.getByText('Temporary Devices').waitFor();
    await page.getByRole('cell', { name: /Visitor laptop/ }).waitFor();
    await page.getByRole('cell', { name: /Visitor phone/ }).waitFor();
});

test('PublicServer credential exports use the configured public server', async t => {
    const peer = 'tcp://public.example.test:11010';
    const { page } = await open(t, '/h/networks/network-0', {}, state => {
        Object.assign(state.networks[0], {
            secure_mode: true, networking_method: 'PublicServer', public_server_url: peer,
            peer_urls: [],
        });
        state.credentials = [{
            credential_id: 'cred-1234567890abcdef', credential_secret: 'test-credential-secret',
            expiry_unix: 1893456000, reusable: true, online_peers: [],
        }];
    });
    await page.getByRole('tab', { name: 'Credentials', exact: true }).click();
    await page.getByRole('button', { name: 'Show join command', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: 'Temporary device join command' });
    assert.equal(await dialog.locator('pre').textContent(),
        `easytier-core --network-name team-0 --secure-mode --credential test-credential-secret -p ${peer}`);
    await dialog.getByRole('button', { name: 'Config file', exact: true }).click();
    assert.equal(await dialog.locator('pre').textContent(), [
        '[network_identity]', 'network_name = "team-0"', '',
        '[[peer]]', `uri = "${peer}"`, '',
        '[secure_mode]', 'enabled = true', 'local_private_key = "test-credential-secret"',
    ].join('\n'));
});

test('node detail gives empty peers a clear home and keeps actions separate', async t => {
    const { page, state } = await open(t, `/h/networks/${id(100)}`, {}, state => { state.networks[0].network_id = id(100); });
    await page.getByRole('button', { name: 'Node Detail' }).click();
    const drawer = page.locator('.console-node-drawer');
    await drawer.getByText('No other nodes yet').waitFor();
    await page.waitForFunction(() => Math.abs(document.querySelector('.console-node-drawer').getBoundingClientRect().right - innerWidth) < 1);
    assert.equal(await drawer.getByRole('columnheader').count(), 0);
    assert.ok(await drawer.locator('.node-drawer-empty').evaluate(el => el.getBoundingClientRect().height < 60));
    assert.ok(await drawer.locator('.node-drawer-summary').evaluate(el => el.getBoundingClientRect().height < 60));
    assert.deepEqual(await drawer.locator('.node-drawer-metric strong').allTextContents().then(values => values.map(s => s.trim())), ['Running', '0', '0', '0']);
    assert.equal(await drawer.locator('.node-drawer-title').textContent(), 'Amsterdam gateway');
    assert.equal(await drawer.locator('.node-drawer-title').count(), 1);
    if (screenshotDir) await drawer.screenshot({ path: `${screenshotDir}/node-detail-empty.png`, animations: 'disabled' });
    await drawer.getByRole('tab', { name: 'Settings & actions' }).click();
    await drawer.getByText('Log Level', { exact: true }).waitFor();
    await drawer.getByRole('button', { name: 'Export Config' }).click();
    await page.getByText('[instance]', { exact: false }).waitFor();
    assert.ok(state.requests.some(r => r.path.endsWith('/proxy-rpc')));
    await page.emulateMedia({ colorScheme: 'dark' });
    await page.reload();
    await page.getByRole('button', { name: 'Switch language' }).click();
    await page.getByRole('button', { name: '节点详情' }).click();
    await drawer.getByText('暂无其他节点').waitFor();
    await page.waitForFunction(() => Math.abs(document.querySelector('.console-node-drawer').getBoundingClientRect().right - innerWidth) < 1);
    if (screenshotDir) await drawer.screenshot({ path: `${screenshotDir}/node-detail-empty-cn-dark.png`, animations: 'disabled' });
});

test('logger levels decode protobuf responses, persist selections and translate labels', async t => {
    const labels = ['Disabled', 'Error', 'Warning', 'Info', 'Debug', 'Trace'];
    const { page, state } = await open(t, `/h/networks/${id(100)}`, {}, state => { state.networks[0].network_id = id(100); });
    const drawer = page.locator('.console-node-drawer');
    const select = drawer.getByRole('combobox');
    const openSettings = async () => {
        await page.getByRole('button', { name: 'Node Detail', exact: true }).click();
        await drawer.getByRole('tab', { name: 'Settings & actions', exact: true }).click();
    };
    // Disabled is omitted by protobuf JSON; named and numeric values are valid.
    for (const [level, label] of [[undefined, 'Disabled'], ...labels.map(label => [label.toUpperCase(), label]), [3, 'Info']]) {
        state.loggerConfig = level === undefined ? {} : { level };
        await openSettings();
        await select.getByText(label, { exact: true }).waitFor();
        await page.keyboard.press('Escape');
        await drawer.waitFor({ state: 'hidden' });
    }
    await openSettings();
    for (const [level, label] of labels.entries()) {
        await select.click();
        assert.deepEqual(await page.getByRole('option').allTextContents(), labels);
        await page.getByRole('option', { name: label, exact: true }).click();
        await page.waitForFunction(() => !document.querySelector('.console-node-drawer .p-select').classList.contains('p-disabled'));
        assert.equal(state.writes.filter(write => write.payload?.method_name === 'set_logger_config').at(-1).payload.payload.level, level);
        await select.getByText(label, { exact: true }).waitFor();
        await page.reload();
        await openSettings();
        await select.getByText(label, { exact: true }).waitFor();
    }
    await page.keyboard.press('Escape');
    await drawer.waitFor({ state: 'hidden' });
    await page.getByRole('button', { name: 'Switch language', exact: true }).click();
    await page.getByRole('button', { name: '节点详情', exact: true }).click();
    await drawer.getByRole('tab').nth(1).click();
    await select.getByText('跟踪', { exact: true }).waitFor();
    await select.click();
    assert.deepEqual(await page.getByRole('option').allTextContents(), ['禁用', '错误', '警告', '信息', '调试', '跟踪']);
});

test('node detail groups populated peers, routes, connections and ACL stats', async t => {
    const { page } = await open(t, `/h/networks/${id(100)}`, { viewport: { width: 390, height: 844 }, colorScheme: 'dark' }, state => {
        state.networks[0].network_id = id(100);
        state.nodeRoutes = [{ peer_id: 2, hostname: 'Build server', ipv4_addr: { address: { addr: 175005442 }, network_length: 24 }, cost: 1, path_latency: 8, proxy_cidrs: [], version: '2.4.5', next_hop_peer_id: 2 }];
        state.nodePeers = [{ peer_id: 2, conns: [
            { conn_id: 'conn-1', tunnel: { tunnel_type: 'tcp', remote_addr: { url: 'tcp://192.0.2.11:11010' } }, stats: { latency_us: 8000, rx_bytes: 1024, tx_bytes: 2048 } },
            { conn_id: 'conn-2', tunnel: { tunnel_type: 'udp', remote_addr: { url: 'udp://192.0.2.11:11010' } }, stats: { latency_us: 9000 }, loss_rate: 0.125 },
        ] }];
        state.nodeAclStats = [{ rule: { name: 'Allow build server' }, stat: { packet_count: 42, byte_count: 4096 } }];
    });
    await page.locator('.mobile-list').getByRole('button', { name: 'Node Detail' }).click();
    const drawer = page.locator('.console-node-drawer');
    await drawer.locator('.node-drawer-mobile-peer').getByText('Build server').waitFor();
    await page.waitForFunction(() => Math.abs(document.querySelector('.console-node-drawer').getBoundingClientRect().right - innerWidth) < 1);
    assert.deepEqual(await drawer.locator('.node-drawer-metric strong').allTextContents().then(values => values.map(s => s.trim())), ['Running', '1', '1', '2']);
    assert.equal(await drawer.getByRole('tab').count(), 2);
    const drawerWidth = await drawer.evaluate(el => ({ drawer: el.getBoundingClientRect().width, viewport: innerWidth }));
    assert.ok(drawerWidth.drawer <= drawerWidth.viewport + 1, JSON.stringify(drawerWidth));
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1));
    if (screenshotDir) await drawer.screenshot({ path: `${screenshotDir}/node-detail-populated-mobile-dark.png`, animations: 'disabled' });
    await page.setViewportSize({ width: 1024, height: 844 });
    await drawer.getByRole('cell', { name: 'Build server' }).waitFor();
    await drawer.locator('details').filter({ hasText: 'Routes' }).locator('summary').click();
    await drawer.getByText('10.110.95.2/24').first().waitFor();
    await drawer.locator('details').filter({ hasText: 'Connections' }).locator('summary').click();
    await drawer.getByRole('cell', { name: '0.0%', exact: true }).waitFor();
    await drawer.getByRole('cell', { name: '12.5%', exact: true }).waitFor();
    await drawer.locator('details').filter({ hasText: 'ACL Stats' }).locator('summary').click();
    await drawer.getByRole('cell', { name: 'Allow build server' }).waitFor();
});

test('empty and failed lists recover without presenting an empty result as success', async t => {
    const { page, state } = await open(t, '/h/deviceList');
    state.machines = [];
    await refresh(page);
    await page.getByRole('heading', { name: 'No devices yet' }).waitFor();
    state.failures.add('/machines');
    await page.reload();
    await page.getByRole('button', { name: 'Retry', exact: true }).waitFor();
    assert.equal(await page.getByRole('heading', { name: 'No devices yet' }).count(), 0);
    state.failures.clear();
    await page.getByRole('button', { name: 'Retry', exact: true }).click();
    await page.getByRole('heading', { name: 'No devices yet' }).waitFor();
});

test('responsive layouts, localization, dark mode and mobile drawer', async t => {
    const { page, state } = await open(t);
    const workspaceMenu = page.locator('.console-sidebar').getByRole('menu', { name: 'Navigation' });
    await workspaceMenu.focus();
    await page.keyboard.press('ArrowDown'); // Dashboard -> Device List
    await page.keyboard.press('Enter');
    await page.waitForURL('**/#/h/deviceList');
    await workspaceMenu.locator('[aria-current="page"]', { hasText: 'Device List' }).waitFor();
    assert.equal(await workspaceMenu.locator('[aria-current="page"]').textContent(), 'Device List');
    const networkMenu = page.locator('.console-sidebar').getByRole('menu', { name: 'Networks' });
    await networkMenu.focus();
    await page.keyboard.press('ArrowDown'); // Networks overview -> Engineering (first nav-child)
    await page.keyboard.press('Enter');
    await page.waitForURL('**/networks/network-0');
    await networkMenu.locator('[aria-current="page"]', { hasText: 'Engineering' }).waitFor();
    for (const colorScheme of ['light', 'dark']) {
        await page.emulateMedia({ colorScheme });
        for (const width of [1440, 1024, 390]) {
            await page.setViewportSize({ width, height: 960 });
            for (const language of ['en', 'cn']) {
                for (const [name, route] of [['overview', '/h'], ['devices', '/h/deviceList'], ['networks', '/h/networks'], ['members', '/h/networks/network-0']]) {
                    await page.goto(`${base}/#${route}`);
                    await page.locator('.console-page').waitFor();
                    if (await page.evaluate(() => localStorage.getItem('lang')) !== language) {
                        await page.getByRole('button', { name: /Switch language|切换语言/ }).click();
                    }
                    await page.locator('.p-skeleton').first().waitFor({ state: 'detached' });
                    assert.equal(await page.locator('.web-console').evaluate(el => getComputedStyle(el).backgroundColor), colorScheme === 'light' ? 'rgb(247, 248, 249)' : 'rgb(16, 23, 30)');
                    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), `${name} ${width} ${colorScheme} must not overflow`);
                    if (screenshotDir) await page.screenshot({ path: `${screenshotDir}/${name}-${language}-${colorScheme}-${width}.png`, fullPage: true, animations: 'disabled' });
                }
            }
        }
    }
    state.machines[0].info.hostname = 'A-very-long-device-hostname-that-must-wrap-without-breaking-the-layout.example.internal';
    await page.setViewportSize({ width: 390, height: 960 });
    await page.goto(`${base}/#/h/deviceList`);
    await page.locator('.mobile-list .entity-link').first().click();
    await page.locator('.console-device-drawer').waitFor();
    await page.waitForFunction(() => Math.abs(document.querySelector('.console-device-drawer').getBoundingClientRect().left) < 1);
    assert.ok(await page.locator('.console-device-drawer').evaluate(el => el.getBoundingClientRect().width <= innerWidth));
    if (screenshotDir) await page.screenshot({ path: `${screenshotDir}/device-drawer-mobile.png`, animations: 'disabled' });
    await page.keyboard.press('Escape');
    await page.locator('.mobile-nav-toggle').click();
    await page.locator('.console-mobile-nav').getByRole('menu', { name: '导航' }).waitFor();
    await page.locator('.console-mobile-nav').getByRole('link', { name: '设备列表', exact: true }).click();
    await page.locator('.console-mobile-nav').waitFor({ state: 'detached' });
    await page.locator('.mobile-nav-toggle').click();
    await page.locator('.console-mobile-nav').getByRole('link').first().click();
    await page.locator('.console-mobile-nav').waitFor({ state: 'detached' });
});


test('external Console keeps the device dashboard without central requests or navigation', async t => {
    const { page, state } = await open(t, '/h', {}, state => {
        state.externalConsole = true;
        state.failures.add('/networks');
    });
    await page.getByText('Amsterdam gateway', { exact: true }).waitFor();
    await delay(2500);
    assert.equal(state.requests.some(request => request.path.startsWith('/networks')), false);
    assert.equal(await page.locator('a[href*="/networks"]').count(), 0);
    assert.equal(await page.locator('.p-message-warn').count(), 0);
    for (const width of [1440, 1024, 390]) {
        await page.setViewportSize({ width, height: 960 });
        assert.equal(await page.locator('.overview-columns > section').count(), 1);
        const bounds = await page.locator('.overview-columns').evaluate(overview => ({
            outer: overview.getBoundingClientRect().width,
            panel: overview.firstElementChild.getBoundingClientRect().width,
        }));
        assert.ok(Math.abs(bounds.outer - bounds.panel) <= 1, `${width}: the device panel fills the single-panel overview`);
    }
});
