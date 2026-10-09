#!/usr/bin/env node
// Runner-version parity witness for the gm-mcp release bridge (gm-mcp/src/release-bridge.js).
//
// Runs the bridge's real reconcile code inside an isolated AGENTPLUG_HOME and GM_TOOLS_DIR,
// against a stubbed GitHub that serves a release runner and a release gm guest. A daemon this
// script starts from the installed runner copy reports runner_version_parity, which must agree.
//
//   Phase A: the installed runner and gm guest are older; the bridge installs both.
//   Phase B: the installed bytes already equal the release, but the records were left stale;
//            the bridge must repair them on its current-build path.
//
// Both phases must leave plugins/gm.build.json naming the installed gm build with its source
// sha, the runner swap record and version file naming the installed runner bytes, and the
// daemon's parity agreeing. A bridge that does not record its installs keeps the stale
// gm.build.json seeded here, and the witness prints RESULT: FAIL.
//
//   node scripts/runner-version-parity-witness.mjs --old-runner <exe> --new-runner <exe>
//     --gm-wasm <wasm> --gm-version <x.y.z> --gm-source <40-hex sha>
//     --src <gm-mcp/src> --node-modules <gm-mcp/node_modules> [--scratch <dir>] [--keep]
//
// Exit 0 on PASS, 1 on FAIL, 2 on a usage or setup error. The live AGENTPLUG_HOME and
// GM_TOOLS_DIR are only read (inputs are copied out of them), never written.

import { execFileSync, spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const RUNNER_REPO = 'AnEntrypoint/agentplug-bin';
const GUEST_REPO = 'AnEntrypoint/plugkit-bin';
const GUEST_ASSET = 'plugkit-slim.wasm';
const SEEDED_GM_VERSION = '0.1.1441';
const LIVE_PLUGINS = ['libsql', 'bert', 'treesitter', 'crux', 'lightpanda'];
const PLUGIN_SUFFIXES = ['.wasm', '.version', '.wasm.sha256'];
const STALE_GM_BUILD = {
  plugin: 'gm',
  source_sha: '0a790d59ad296a75fc66c7b52c592ee9561306ed',
  origin_main_sha: '0a790d59ad296a75fc66c7b52c592ee9561306ed',
  wasm_sha256: 'f5eb111ac5780941de00f6b4e210d732c25fb305b72581cb13ccd6d45d89f6db',
  wasm_bytes: 5011332,
  built_at: 1791474360,
  source_root: '/c/dev/gm/rs-plugkit',
};
const DAEMON_CONFIG = {
  registry_poll_interval_secs: 5,
  heartbeat_interval_secs: 10,
  plugin_update_poll_interval_secs: 3600,
  plugin_update_poll_interval_secs_by_name: {},
  runner_update_poll_interval_secs: 3600,
  instruction_source_poll_interval_secs: 3600,
  require_runner_signature: false,
};
const PARITY_TIMEOUT_MS = 90_000;

class UsageError extends Error {}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');
const readJson = (file) => {
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch {
    return null;
  }
};
const readText = (file) => {
  try {
    return fs.readFileSync(file, 'utf8');
  } catch {
    return null;
  }
};
const LIVE_HOME = path.join(os.homedir(), '.agentplug');
const LIVE_TOOLS = path.join(os.homedir(), '.gm-tools');

function parseArgs(argv) {
  const opts = { keep: false, scratch: path.join(os.tmpdir(), 'agentplug-runner-version-parity-witness') };
  for (let i = 0; i < argv.length; i++) {
    const flag = argv[i];
    if (flag === '--keep') {
      opts.keep = true;
      continue;
    }
    const match = /^--([a-z-]+)$/.exec(flag ?? '');
    if (!match) throw new UsageError(`unknown option ${flag}`);
    const value = argv[++i];
    if (value === undefined) throw new UsageError(`${flag} needs a value`);
    opts[match[1].replace(/-([a-z])/g, (_, c) => c.toUpperCase())] = value;
  }
  for (const key of ['oldRunner', 'newRunner', 'gmWasm', 'gmVersion', 'gmSource', 'src', 'nodeModules']) {
    if (!opts[key]) throw new UsageError(`--${key.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`)} is required`);
  }
  if (!/^[0-9a-f]{40}$/.test(opts.gmSource)) throw new UsageError('--gm-source must be a 40-character lowercase sha');
  if (!/^\d+\.\d+\.\d+$/.test(opts.gmVersion)) throw new UsageError('--gm-version must be X.Y.Z');
  return opts;
}

function buildInfo(exe) {
  const info = JSON.parse(execFileSync(exe, ['--build-info'], { encoding: 'utf8', windowsHide: true, timeout: 20000 }));
  if (typeof info.version !== 'string' || info.release_build !== true) {
    throw new UsageError(`${exe} is not a release build: ${JSON.stringify(info)}`);
  }
  return info;
}

function semverGreater(a, b) {
  const x = a.split('.').map(Number);
  const y = b.split('.').map(Number);
  for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return x[i] > y[i];
  return false;
}

function prepareScratch(scratch, oldRunner) {
  for (const live of [LIVE_HOME, LIVE_TOOLS]) {
    if (scratch === live || scratch.startsWith(live + path.sep)) throw new UsageError(`--scratch must not be ${live} or inside it`);
  }
  fs.rmSync(scratch, { recursive: true, force: true });
  const dirs = {
    scratch,
    home: path.join(scratch, 'home'),
    tools: path.join(scratch, 'tools'),
    userHome: path.join(scratch, 'userhome'),
    logs: path.join(scratch, 'logs'),
  };
  for (const dir of [path.join(dirs.home, 'plugins'), dirs.tools, dirs.userHome, dirs.logs]) fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dirs.home, 'daemon-config.json'), JSON.stringify(DAEMON_CONFIG, null, 4));
  for (const name of LIVE_PLUGINS) {
    for (const suffix of PLUGIN_SUFFIXES) {
      const from = path.join(LIVE_HOME, 'plugins', `${name}${suffix}`);
      if (fs.existsSync(from)) fs.copyFileSync(from, path.join(dirs.home, 'plugins', `${name}${suffix}`));
    }
  }
  if (fs.existsSync(path.join(LIVE_HOME, 'precompiled'))) {
    fs.cpSync(path.join(LIVE_HOME, 'precompiled'), path.join(dirs.home, 'precompiled'), { recursive: true });
  }
  fs.copyFileSync(oldRunner, path.join(dirs.tools, 'agentplug-runner.exe'));
  fs.writeFileSync(path.join(dirs.home, 'plugins', 'gm.wasm'), Buffer.from([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]));
  fs.writeFileSync(path.join(dirs.home, 'plugins', 'gm.version'), SEEDED_GM_VERSION);
  writeStaleBuildRecord(dirs.home);
  return dirs;
}

function writeStaleBuildRecord(home) {
  fs.writeFileSync(path.join(home, 'plugins', 'gm.build.json'), `${JSON.stringify(STALE_GM_BUILD)}\n`);
}

function fakeGithub({ runnerNew, runnerVersion, runnerAsset, gmWasm, gmVersion, gmSource }) {
  const routes = new Map();
  const add = (url, body) => routes.set(url, body);
  const runnerBytes = fs.readFileSync(runnerNew);
  const runnerSha = sha256(runnerBytes);
  const wasmBytes = fs.readFileSync(gmWasm);
  const wasmSha = sha256(wasmBytes);
  const runnerBase = `https://github.com/${RUNNER_REPO}/releases/download/v${runnerVersion}`;
  const guestBase = `https://github.com/${GUEST_REPO}/releases/download/v${gmVersion}`;
  add(`https://api.github.com/repos/${RUNNER_REPO}/releases/latest`, Buffer.from(JSON.stringify({
    tag_name: `v${runnerVersion}`,
    body: '',
    assets: [
      { name: runnerAsset, browser_download_url: `${runnerBase}/${runnerAsset}`, digest: `sha256:${runnerSha}` },
      { name: `${runnerAsset}.sha256`, browser_download_url: `${runnerBase}/${runnerAsset}.sha256` },
    ],
  })));
  add(`${runnerBase}/${runnerAsset}`, runnerBytes);
  add(`${runnerBase}/${runnerAsset}.sha256`, Buffer.from(`${runnerSha}  ${runnerAsset}\n`));
  add(`https://api.github.com/repos/${GUEST_REPO}/releases/latest`, Buffer.from(JSON.stringify({
    tag_name: `v${gmVersion}`,
    body: `source-head: ${gmSource}\nsource-deps: AnEntrypoint/rs-search@0000000000000000000000000000000000000000\nAuto-published from CI\n`,
    assets: [
      { name: GUEST_ASSET, browser_download_url: `${guestBase}/${GUEST_ASSET}`, digest: `sha256:${wasmSha}` },
      { name: `${GUEST_ASSET}.sha256`, browser_download_url: `${guestBase}/${GUEST_ASSET}.sha256` },
    ],
  })));
  add(`${guestBase}/${GUEST_ASSET}`, wasmBytes);
  add(`${guestBase}/${GUEST_ASSET}.sha256`, Buffer.from(`${wasmSha}  ${GUEST_ASSET}\n`));
  globalThis.fetch = async (url) => {
    const key = String(url);
    if (!routes.has(key)) throw new Error(`witness: unexpected network request ${key}`);
    return new Response(routes.get(key), { status: 200 });
  };
}

async function importBridge(srcDir, nodeModules, scratch) {
  const bridgeDir = path.join(scratch, 'bridge');
  fs.cpSync(srcDir, bridgeDir, { recursive: true });
  fs.writeFileSync(path.join(bridgeDir, 'package.json'), '{"type":"module"}\n');
  fs.symlinkSync(nodeModules, path.join(bridgeDir, 'node_modules'), 'junction');
  return import(pathToFileURL(path.join(bridgeDir, 'release-bridge.js')).href);
}

function recordFailures(dirs, expected) {
  const failures = [];
  const build = readJson(path.join(dirs.home, 'plugins', 'gm.build.json'));
  if (!build) {
    failures.push('plugins/gm.build.json is missing or unreadable');
  } else {
    if (build.version !== expected.gmVersion) failures.push(`gm.build.json names version ${build.version}, installed ${expected.gmVersion}`);
    if (build.source_sha !== expected.gmSource) failures.push(`gm.build.json source_sha ${build.source_sha}, installed source ${expected.gmSource}`);
    if (build.wasm_sha256 !== expected.wasmSha) failures.push(`gm.build.json wasm_sha256 ${build.wasm_sha256}, installed ${expected.wasmSha}`);
  }
  const swap = readJson(path.join(dirs.home, 'last-completed-runner-swap.json'));
  if (!swap || swap.version !== expected.runnerVersion || swap.sha256 !== expected.runnerSha) {
    failures.push(`last-completed-runner-swap.json does not name the installed runner ${expected.runnerVersion} ${expected.runnerSha}`);
  }
  const version = readText(path.join(dirs.home, 'agentplug-runner.version'))?.trim();
  if (version !== expected.runnerVersion) failures.push(`agentplug-runner.version is ${version}, installed ${expected.runnerVersion}`);
  return failures;
}

async function stopChild(child, hasExited) {
  if (hasExited()) return;
  if (process.platform === 'win32') spawnSync('taskkill', ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true });
  else child.kill('SIGTERM');
  for (let i = 0; i < 100 && !hasExited(); i++) await sleep(100);
}

async function daemonParity(dirs, phase) {
  const exe = path.join(dirs.tools, 'agentplug-runner.exe');
  const statusFile = path.join(dirs.home, 'daemon-status.json');
  fs.rmSync(statusFile, { force: true });
  const env = { ...process.env, AGENTPLUG_HOME: dirs.home, GM_TOOLS_DIR: dirs.tools, HOME: dirs.userHome, USERPROFILE: dirs.userHome };
  delete env.AGENTPLUG_NO_DAEMON;
  const log = fs.createWriteStream(path.join(dirs.logs, `daemon-${phase}.log`));
  const child = spawn(exe, ['daemon'], { env, stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
  child.stdout.pipe(log, { end: false });
  child.stderr.pipe(log, { end: false });
  let exited = false;
  child.on('exit', () => {
    exited = true;
  });
  try {
    const deadline = Date.now() + PARITY_TIMEOUT_MS;
    while (Date.now() < deadline && !exited) {
      const status = readJson(statusFile);
      if (status && status.pid === child.pid && status.runner_version_parity) {
        return { parity: status.runner_version_parity, exe };
      }
      await sleep(500);
    }
    return { parity: null, exe };
  } finally {
    await stopChild(child, () => exited);
    log.end();
  }
}

async function parityFailures(dirs, phase) {
  const { parity, exe } = await daemonParity(dirs, phase);
  if (!parity) return [`daemon did not report runner_version_parity within ${PARITY_TIMEOUT_MS / 1000} s (log: ${path.join(dirs.logs, `daemon-${phase}.log`)})`];
  const failures = [];
  if (String(parity.exe ?? '').toLowerCase() !== exe.toLowerCase()) failures.push(`daemon probed ${parity.exe}, expected the installed copy ${exe}`);
  if (parity.agrees !== true) failures.push(`runner_version_parity.agrees is ${parity.agrees}: ${(parity.disagreements ?? []).join(' | ')}`);
  return failures;
}

function summarize(label, result) {
  return `${label} bridge: outcome=${result.outcome} runner=${result.runner?.outcome} guest=${result.guest?.outcome}`;
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  const oldRunner = path.resolve(opts.oldRunner);
  const newRunner = path.resolve(opts.newRunner);
  const oldInfo = buildInfo(oldRunner);
  const newInfo = buildInfo(newRunner);
  if (!semverGreater(newInfo.version, oldInfo.version)) {
    throw new UsageError(`--new-runner ${newInfo.version} must be newer than --old-runner ${oldInfo.version}`);
  }
  const gmWasm = path.resolve(opts.gmWasm);
  const gmWasmBytes = fs.readFileSync(gmWasm);
  if (gmWasmBytes.subarray(0, 4).toString('hex') !== '0061736d') throw new UsageError('--gm-wasm is not a wasm module');

  const scratch = path.resolve(opts.scratch);
  const dirs = prepareScratch(scratch, oldRunner);
  const bridge = await importBridge(path.resolve(opts.src), path.resolve(opts.nodeModules), scratch);
  const runnerAsset = bridge.runnerAssetName();
  if (!runnerAsset) throw new UsageError(`no runner asset for ${process.platform}-${process.arch}`);
  fakeGithub({ runnerNew: newRunner, runnerVersion: newInfo.version, runnerAsset, gmWasm, gmVersion: opts.gmVersion, gmSource: opts.gmSource });

  process.env.AGENTPLUG_HOME = dirs.home;
  process.env.GM_TOOLS_DIR = dirs.tools;
  process.env.GM_MCP_RELEASE_BRIDGE_INTERVAL_MS = '1';
  for (const name of ['AGENTPLUG_NO_SELF_UPDATE', 'GM_MCP_NO_SELF_UPDATE', 'GM_MCP_RELEASE_BRIDGE']) delete process.env[name];

  const expected = {
    runnerVersion: newInfo.version,
    runnerSha: sha256(fs.readFileSync(newRunner)),
    gmVersion: opts.gmVersion,
    gmSource: opts.gmSource,
    wasmSha: sha256(gmWasmBytes),
  };
  const failures = [];
  const lines = [];

  let result = await bridge.runReleaseBridge();
  lines.push(summarize('phase A', result));
  if (result.runner?.outcome !== 'swapped') failures.push(`phase A: runner outcome is ${result.runner?.outcome}, expected swapped`);
  if (result.guest?.outcome !== 'swapped') failures.push(`phase A: guest outcome is ${result.guest?.outcome}, expected swapped`);
  failures.push(...recordFailures(dirs, expected).map((f) => `phase A: ${f}`));
  failures.push(...(await parityFailures(dirs, 'phase-a')).map((f) => `phase A: ${f}`));

  fs.rmSync(path.join(dirs.home, 'gm-mcp-release-bridge.json'), { force: true });
  fs.rmSync(path.join(dirs.home, 'last-completed-runner-swap.json'), { force: true });
  fs.rmSync(path.join(dirs.home, 'agentplug-runner.version'), { force: true });
  writeStaleBuildRecord(dirs.home);
  result = await bridge.runReleaseBridge();
  lines.push(summarize('phase B', result));
  if (result.runner?.outcome !== 'current') failures.push(`phase B: runner outcome is ${result.runner?.outcome}, expected current`);
  if (result.guest?.outcome !== 'current') failures.push(`phase B: guest outcome is ${result.guest?.outcome}, expected current`);
  failures.push(...recordFailures(dirs, expected).map((f) => `phase B: ${f}`));
  failures.push(...(await parityFailures(dirs, 'phase-b')).map((f) => `phase B: ${f}`));

  for (const line of lines) console.log(line);
  for (const failure of failures) console.log(`FAIL ${failure}`);
  console.log(`installed runner ${expected.runnerVersion} sha256 ${expected.runnerSha}; gm ${expected.gmVersion} source ${expected.gmSource} wasm ${expected.wasmSha}`);
  console.log(`RESULT: ${failures.length === 0 ? 'PASS' : 'FAIL'}`);
  if (failures.length === 0 && !opts.keep) fs.rmSync(scratch, { recursive: true, force: true });
  process.exit(failures.length === 0 ? 0 : 1);
}

main().catch((error) => {
  if (error instanceof UsageError) {
    console.error(`usage: ${error.message}`);
    process.exit(2);
  }
  console.error(error?.stack ?? String(error));
  console.log('RESULT: FAIL');
  process.exit(1);
});
