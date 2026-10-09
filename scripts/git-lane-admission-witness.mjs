#!/usr/bin/env node
// Git-lane admission witness for the agentplug shared daemon.
//
// Builds the runner and runs two live self-checks against the real SharedPluginPool and
// GmFairnessGuard. RESULT: PASS is printed only when both exit 0 and print their own RESULT: PASS:
//   selfcheck-git-admission  with the general band of cheap dispatches full, a git-lane cheap
//                            dispatch is admitted within 10 s, a second one is refused within its
//                            bound, and a physical slot stays free for a short verb
//   selfcheck-lane-release   a project lane held by a dispatch whose requester has no live lease is
//                            released by the orphan sweep, and the abandoned holder cannot free the
//                            lane once a successor owns it
//
//   node scripts/git-lane-admission-witness.mjs [--root <agentplug dir>] [--mutant] [--scratch <dir>]
//     [--timeout-ms 900000]
//
// --mutant copies crates/ and the workspace manifest into a scratch tree, disables the git band
// there, builds it against the shared target dir (dependency artifacts are reused), and prints
// RESULT: FAIL when the self-check detects the starved git lane. Exit 0 on PASS, 1 on FAIL, 2 on a
// usage or setup error.

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const SELFCHECKS = ['selfcheck-git-admission', 'selfcheck-lane-release'];
const GIT_BAND_EXPRESSION = 'self.short_reserved_slots().saturating_sub(1)';
const DEFAULT_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const MAX_BUFFER = 64 * 1024 * 1024;

class UsageError extends Error {}

function parseArgs(argv) {
  const opts = { root: DEFAULT_ROOT, mutant: false, scratch: null, timeoutMs: 900000 };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    const value = () => {
      if (i + 1 >= argv.length) throw new UsageError(`${arg} needs a value`);
      i += 1;
      return argv[i];
    };
    if (arg === '--root') opts.root = path.resolve(value());
    else if (arg === '--mutant') opts.mutant = true;
    else if (arg === '--scratch') opts.scratch = path.resolve(value());
    else if (arg === '--timeout-ms') opts.timeoutMs = Number(value());
    else throw new UsageError(`unknown argument ${arg}`);
  }
  if (!Number.isFinite(opts.timeoutMs) || opts.timeoutMs <= 0) {
    throw new UsageError('--timeout-ms must be a positive number');
  }
  return opts;
}

function copyTree(from, to) {
  fs.cpSync(from, to, {
    recursive: true,
    filter: (src) => !/[\\/](target|\.git|node_modules)([\\/]|$)/.test(src),
  });
}

function prepareMutant(root, scratch) {
  fs.mkdirSync(scratch, { recursive: true });
  for (const name of ['Cargo.toml', 'Cargo.lock']) {
    const source = path.join(root, name);
    if (fs.existsSync(source)) fs.copyFileSync(source, path.join(scratch, name));
  }
  copyTree(path.join(root, 'crates'), path.join(scratch, 'crates'));
  const registry = path.join(scratch, 'crates', 'agentplug-host', 'src', 'registry.rs');
  const source = fs.readFileSync(registry, 'utf8');
  const hits = source.split(GIT_BAND_EXPRESSION).length - 1;
  if (hits !== 1) {
    throw new UsageError(`expected exactly one git band limit expression in ${registry}, found ${hits}`);
  }
  fs.writeFileSync(registry, source.replace(GIT_BAND_EXPRESSION, `0 * ${GIT_BAND_EXPRESSION}`));
}

function buildRunner(treeRoot, targetDir, timeoutMs) {
  const env = { ...process.env };
  if (targetDir) env.CARGO_TARGET_DIR = targetDir;
  // A scratch tree and the real tree share a target dir and the same workspace-relative package
  // hashes, so a previous build can leave the other tree's binary in place. Cleaning the two
  // agentplug packages first forces this tree's own objects to be built and uplifted.
  const cleaned = spawnSync('cargo', ['clean', '--quiet', '-p', 'agentplug-host', '-p', 'agentplug-runner'], {
    cwd: treeRoot,
    encoding: 'utf8',
    timeout: timeoutMs,
    maxBuffer: MAX_BUFFER,
    windowsHide: true,
    env,
  });
  if (cleaned.status !== 0) {
    const tail = `${cleaned.stderr ?? ''}${cleaned.stdout ?? ''}`.trim().split(/\r?\n/).slice(-4).join(' | ');
    return { ok: false, detail: `cargo clean exit ${cleaned.status ?? cleaned.signal}: ${tail}` };
  }
  const result = spawnSync('cargo', ['build', '--quiet', '-p', 'agentplug-runner'], {
    cwd: treeRoot,
    encoding: 'utf8',
    timeout: timeoutMs,
    maxBuffer: MAX_BUFFER,
    windowsHide: true,
    env,
  });
  if (result.status === 0) return { ok: true, detail: 'built' };
  const tail = `${result.stderr ?? ''}${result.stdout ?? ''}`.trim().split(/\r?\n/).slice(-6).join(' | ');
  return { ok: false, detail: `cargo exit ${result.status ?? result.signal}: ${tail}` };
}

function runnerPath(treeRoot, targetDir) {
  const exe = process.platform === 'win32' ? 'agentplug-runner.exe' : 'agentplug-runner';
  return path.join(targetDir ?? path.join(treeRoot, 'target'), 'debug', exe);
}

function runSelfcheck(binary, name, timeoutMs) {
  const started = Date.now();
  const result = spawnSync(binary, [name], {
    encoding: 'utf8',
    timeout: timeoutMs,
    maxBuffer: MAX_BUFFER,
    windowsHide: true,
  });
  const stdout = result.stdout ?? '';
  const lines = stdout.split(/\r?\n/);
  const passed = lines.includes('RESULT: PASS');
  const failLine = lines.find((line) => line.startsWith('RESULT: FAIL')) ?? null;
  return {
    name,
    ok: result.status === 0 && passed,
    status: result.status ?? result.signal,
    ms: Date.now() - started,
    failLine,
    lines: lines.filter((line) => line.startsWith(`[${name}]`)),
  };
}

function main() {
  let opts;
  try {
    opts = parseArgs(process.argv.slice(2));
  } catch (error) {
    console.error(`git-lane-admission-witness: ${error.message}`);
    process.exit(2);
  }

  let treeRoot = opts.root;
  let targetDir = null;
  try {
    if (opts.mutant) {
      const scratch = opts.scratch ?? fs.mkdtempSync(path.join(os.tmpdir(), 'agentplug-git-mutant-'));
      prepareMutant(opts.root, scratch);
      treeRoot = scratch;
      targetDir = path.join(opts.root, 'target');
    }
  } catch (error) {
    console.error(`git-lane-admission-witness: ${error.message}`);
    process.exit(2);
  }

  console.log(`tree ${treeRoot}${opts.mutant ? ' (mutant: git band disabled)' : ''}`);
  const build = buildRunner(treeRoot, targetDir, opts.timeoutMs);
  if (!build.ok) {
    console.log(`RESULT: FAIL build did not complete: ${build.detail}`);
    process.exit(1);
  }

  const binary = runnerPath(treeRoot, targetDir);
  const runs = SELFCHECKS.map((name) => runSelfcheck(binary, name, opts.timeoutMs));
  for (const run of runs) {
    for (const line of run.lines) console.log(line);
    const verdict = run.ok ? 'PASS' : 'FAIL';
    const reason = run.failLine ? ` -- ${run.failLine.slice('RESULT: FAIL '.length)}` : '';
    console.log(`witness ${run.name}: exit ${run.status} ${verdict} in ${run.ms}ms${reason}`);
  }

  const failed = runs.filter((run) => !run.ok).map((run) => run.name);
  if (failed.length === 0) {
    console.log('RESULT: PASS');
    process.exit(0);
  }
  console.log(`RESULT: FAIL ${failed.join(', ')}${opts.mutant ? ' (git-lane mutant)' : ''}`);
  process.exit(1);
}

main();
