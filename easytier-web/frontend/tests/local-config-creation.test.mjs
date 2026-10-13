import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdtemp, mkdir, readFile, rm } from 'node:fs/promises';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { setTimeout as delay } from 'node:timers/promises';
import { test } from 'node:test';
import { chromium } from 'playwright';
import { NetworkTypes, LocalConfigs } from '../../frontend-lib/dist/easytier-frontend-lib.js';

const repo = resolve(import.meta.dirname, '../../..');
const binaries = resolve(process.env.EASYTIER_SMOKE_BIN_DIR ?? join(repo, 'target/debug'));
const suffix = process.platform === 'win32' ? '.exe' : '';
const coreBinary = resolve(process.env.EASYTIER_SMOKE_CORE_BINARY ?? join(binaries, `easytier-core${suffix}`));
const embeddedWeb = join(binaries, `easytier-web-embed${suffix}`);
const webBinary = existsSync(embeddedWeb) ? embeddedWeb : join(binaries, `easytier-web${suffix}`);
const username = 'creation-smoke';
const password = 'e2e-password';
// Test-only account, matching the existing central E2E login fixture.
const passwordHash = '$argon2id$v=19$m=4096,t=3,p=1$ZWFzeXRpZXItZTJlLXNhbHQ$FHbDjVElaTXuArgEEQrQrO4AdsnqNOcXGA5PFY8cG/s';
const machine = '00000000-0000-0000-0000-000000000002';

async function port() {
    const server = createServer();
    await new Promise((done, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', done); });
    const selected = server.address().port;
    await new Promise((done, reject) => server.close(error => error ? reject(error) : done()));
    return selected;
}

async function waitFor(check, label, timeout = 30000) {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) {
        if (await check()) return;
        await delay(100);
    }
    throw new Error(`Timed out waiting for ${label}`);
}

function start(binary, args, cwd) {
    // Keep this isolated from any locally configured EasyTier account/options.
    const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !/^ET_/i.test(key)));
    const child = spawn(binary, args, { cwd, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    let logs = '';
    child.on('error', error => { logs += `${error}\n`; });
    for (const stream of [child.stdout, child.stderr]) stream.on('data', chunk => { logs = (logs + chunk).slice(-20000); });
    return { child, logs: () => logs };
}

async function stop(process) {
    if (!process || !process.child.pid || process.child.exitCode !== null) return;
    const exited = new Promise(done => process.child.once('exit', done));
    process.child.kill();
    await Promise.race([exited, delay(5000).then(() => { throw new Error('Smoke process did not stop'); })]);
}

function seedUser(database) {
    const db = new DatabaseSync(database);
    try {
        const user = db.prepare('INSERT INTO users(username, password) VALUES (?, ?)').run(username, passwordHash);
        const group = db.prepare("SELECT id FROM groups WHERE name = 'users'").get();
        db.prepare('INSERT INTO users_groups(user_id, group_id) VALUES (?, ?)').run(user.lastInsertRowid, group.id);
    } finally { db.close(); }
}

function wireRequest(request) {
    const hex = request.inst_id.replaceAll('-', '');
    return { ...request, inst_id: Object.fromEntries([0, 1, 2, 3].map(index =>
        [`part${index + 1}`, parseInt(hex.slice(index * 8, index * 8 + 8), 16)])) };
}

test('embedded console creates a default network and Core restores it as disabled', { timeout: 120000 }, async () => {
    assert.ok(existsSync(webBinary), `Missing embedded Web binary: ${webBinary}`);
    assert.ok(existsSync(coreBinary), `Missing Core binary: ${coreBinary}`);
    const fixtures = JSON.parse(await readFile(new URL('../../frontend-lib/tests/fixtures/network-create-patches.json', import.meta.url)));
    for (const fixture of fixtures) {
        assert.deepEqual(LocalConfigs.buildLocalConfigCreatePatch({ ...NetworkTypes.DEFAULT_NETWORK_CONFIG(), ...fixture.overrides }, fixture.capabilities), fixture.request);
    }
    const work = await mkdtemp(join(tmpdir(), 'easytier-create-'));
    const directory = join(work, 'configs');
    await mkdir(directory);
    const apiPort = await port();
    const configPort = await port();
    const base = `http://127.0.0.1:${apiPort}`;
    let web;
    let core;
    let official;
    let browser;
    try {
        web = start(webBinary, ['--db', join(work, 'web.db'), '--api-server-addr', '127.0.0.1',
            '--api-server-port', String(apiPort), '--config-server-protocol', 'tcp', '--config-server-port', String(configPort),
            '--console-log-level', 'error'], work);
        await waitFor(async () => {
            assert.equal(web.child.exitCode, null, web.logs());
            return fetch(`${base}/api_meta.js`).then(response => response.ok).catch(() => false);
        }, 'embedded Web startup');
        seedUser(join(work, 'web.db'));
        browser = await chromium.launch({ headless: true, executablePath: process.env.CHROMIUM_EXECUTABLE || undefined,
            args: ['--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader'] });
        const context = await browser.newContext({ baseURL: base });
        await context.addInitScript(() => localStorage.setItem('lang', 'en'));
        const page = await context.newPage();
        await page.goto('/');
        await page.locator('#username').fill(username);
        await page.locator('#password input').fill(password);
        await page.getByRole('button', { name: 'Login', exact: true }).click();
        await page.locator('.console-page').waitFor();
        const api = async path => {
            const response = await context.request.get(`/api/v1${path}`);
            assert.equal(response.status(), 200, await response.text());
            return response.json();
        };
        const startCore = () => start(coreBinary, ['--daemon', '--config-dir', directory,
            '--config-server', `tcp://127.0.0.1:${configPort}/${username}`, '--machine-id', machine,
            '--no-listener', '--no-tun', '--hostname', 'creation-smoke-node', '--console-log-level', 'error'], work);
        core = startCore();
        await waitFor(async () => {
            assert.equal(core.child.exitCode, null, core.logs());
            const response = await context.request.get(`/api/v1/machines/${machine}/local-configs`);
            if (!response.ok()) return false;
            const snapshot = await response.json();
            return snapshot.online && !snapshot.stale && snapshot.capabilities?.includes('management:persisted-config-revision-v1');
        }, 'Core revision-capable connection');
        const listed = page.waitForResponse(response => response.url().endsWith(`/machines/${machine}/networks`) && response.request().method() === 'GET');
        await page.goto(`/#/h/deviceList/device/${machine}`);
        assert.ok((await (await listed).json()).runtime_capabilities?.includes('management:persisted-config-revision-v1'));
        const createdResponse = page.waitForResponse(response => response.url().endsWith(`/machines/${machine}/local-configs/patch`));
        await page.getByRole('button', { name: 'Create New Network', exact: true }).click();
        const created = await createdResponse;
        assert.equal(created.status(), 200, await created.text());
        const submitted = created.request().postDataJSON();
        assert.equal(submitted.expected_revision, '');
        assert.equal(submitted.apply_mode, 1);
        assert.deepEqual(submitted.field_mask.filter(field => /^enable_vpn_portal$|^vpn_portal_(listen_port|client_network_addr|client_network_len)$|^quic_listen_port$/.test(field)), []);
        const snapshot = await created.json();
        const entry = snapshot.entries.find(entry => entry.config?.hostname === 'creation-smoke-node');
        assert.ok(entry, 'The default network is observable');
        assert.equal(entry.enabled ?? false, false);
        assert.equal(entry.running ?? false, false);
        assert.ok(entry.revision);
        const hex = [entry.inst_id.part1, entry.inst_id.part2, entry.inst_id.part3, entry.inst_id.part4].map(part => (part ?? 0).toString(16).padStart(8, '0')).join('');
        const id = `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
        const file = join(directory, `${id}.toml`);
        const saved = await readFile(file, 'utf8');
        assert.ok(saved.includes(id));
        const state = JSON.parse(await readFile(join(directory, 'easytier-state.json'), 'utf8'));
        assert.equal(state.instances[id].enabled, false);
        // Exercise every shared frontend request against the actual shipped Core.
        for (const fixture of fixtures) {
            const response = await context.request.post(`/api/v1/machines/${machine}/local-configs/patch`, { data: wireRequest(fixture.request) });
            assert.equal(response.status(), 200, `${fixture.name}: ${await response.text()}`);
        }
        const invalid = wireRequest({ ...fixtures[0].request, inst_id: '00000000-0000-0000-0000-000000000200', field_mask: ['enable_vpn_portal'] });
        const rejected = await context.request.post(`/api/v1/machines/${machine}/local-configs/patch`, { data: invalid });
        assert.equal(rejected.status(), 422);
        assert.match((await rejected.json()).message, /enable_vpn_portal/);
        const missing = await context.request.post(`/api/v1/machines/${machine}/local-configs/patch`, {
            data: { ...invalid, expected_revision: 'not-observed', field_mask: ['hostname'], config: { hostname: 'missing' } },
        });
        assert.equal(missing.status(), 404);
        await stop(core);
        core = startCore();
        await waitFor(async () => {
            const mirror = await api(`/machines/${machine}/local-configs`);
            return mirror.online && !mirror.stale && mirror.catalog_epoch !== snapshot.catalog_epoch && mirror.entries.length === fixtures.length + 1;
        }, 'Core catalog after restart');
        const restored = (await api(`/machines/${machine}/local-configs`)).entries;
        assert.ok(restored.every(entry => !entry.enabled && !entry.running));
        assert.equal(await readFile(file, 'utf8'), saved);

        if (process.env.OFFICIAL_EASYTIER_BIN) {
            const officialDirectory = join(work, 'official-configs');
            await mkdir(officialDirectory);
            const officialMachine = '00000000-0000-0000-0000-000000000003';
            official = start(resolve(process.env.OFFICIAL_EASYTIER_BIN), ['--daemon', '--config-dir', officialDirectory,
                '--config-server', `tcp://127.0.0.1:${configPort}/${username}`, '--machine-id', officialMachine,
                '--no-listener', '--no-tun', '--hostname', 'official-smoke-node', '--console-log-level', 'error'], work);
            await waitFor(async () => {
                assert.equal(official.child.exitCode, null, official.logs());
                return (await api('/machines')).machines.some(device => device.machine_id === officialMachine || device.info?.machine_id?.part4 === 3);
            }, 'official Core enrollment');
            const legacyList = page.waitForResponse(response => response.url().endsWith(`/machines/${officialMachine}/networks`) && response.request().method() === 'GET');
            await page.goto(`/#/h/deviceList/device/${officialMachine}`);
            assert.ok(!(await (await legacyList).json()).runtime_capabilities?.includes('management:persisted-config-revision-v1'));
            const savedResponse = page.waitForResponse(response => response.url().includes(`/machines/${officialMachine}/networks/config/`) && response.request().method() === 'PUT');
            await page.getByRole('button', { name: 'Create New Network', exact: true }).click();
            const response = await savedResponse;
            assert.equal(response.status(), 200, await response.text());
            const config = response.request().postDataJSON().config;
            assert.equal(config.network_name, 'easytier');
            assert.equal(config.vpn, undefined);
            assert.equal(config.close_redundant_conns_when_disguised, undefined);
            // Official Web owns this saved configuration in its database; the
            // local TOML catalog / SaveNetworkInstanceConfig RPC is fork-only.
            const saved = await api(`/machines/${officialMachine}/networks/config/${config.instance_id}`);
            assert.equal(saved.instance_id, config.instance_id);
            assert.equal(saved.hostname, config.hostname);
            const ids = await api(`/machines/${officialMachine}/networks`);
            assert.equal((ids.running_inst_ids ?? []).length, 0);
            assert.equal((ids.disabled_inst_ids ?? []).length, 1);
        }
    } catch (error) {
        error.message += `\nWeb logs: ${web?.logs()}\nCore logs: ${core?.logs()}\nOfficial logs: ${official?.logs()}`;
        throw error;
    } finally {
        await browser?.close();
        await stop(official);
        await stop(core);
        await stop(web);
        assert.ok(work.startsWith(join(tmpdir(), 'easytier-create-')));
        await rm(work, { recursive: true, force: true });
    }
});
