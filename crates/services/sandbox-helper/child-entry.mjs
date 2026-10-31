// This entrypoint itself starts INSIDE the established OS sandbox. Untrusted
// component environment never reaches Node startup or the sandbox bootstrap.
//
// fd 3 is a private pipe to run.mjs. If the program cannot be started, one
// line {"kind":"spawn_failed","reason":"..."} is written there and the exit
// status is 127. The program never inherits fd 3 (it gets /dev/null in that
// slot), so it cannot forge or suppress this report.
import { spawn } from 'node:child_process';
import { createWriteStream } from 'node:fs';

const SPAWN_FAILED_EXIT = 127;
const singleLine = value => String(value?.message ?? value).replace(/\s+/g, ' ').trim();
let control;
try {
  control = createWriteStream(null, { fd: 3 });
  control.on('error', () => {});
} catch {}
let started = false;
let failed = false;

function spawnFailed(reason) {
  if (failed || started) return;
  failed = true;
  control?.end(`${JSON.stringify({ kind: 'spawn_failed', reason: singleLine(reason) })}\n`);
  process.exitCode = SPAWN_FAILED_EXIT;
}

function childEnvironment() {
  const environment = JSON.parse(process.env.XYLO_SANDBOX_CHILD_ENV ?? '{}');
  delete process.env.XYLO_SANDBOX_CHILD_ENV;
  if (!environment || Array.isArray(environment) || typeof environment !== 'object') throw new Error('Invalid child environment');
  for (const [name, value] of Object.entries(environment)) {
    if (!/^[A-Za-z0-9_]{1,128}$/.test(name) || typeof value !== 'string' || name === 'XYLO_SANDBOX_CHILD_ENV') throw new Error('Invalid child environment');
  }
  return environment;
}

try {
  const environment = childEnvironment();
  const child = spawn(process.argv[2], process.argv.slice(3), {
    stdio: ['inherit', 'inherit', 'inherit', 'ignore'],
    env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1', ...environment },
  });
  child.once('spawn', () => { started = true; control?.end(); });
  child.once('error', error => spawnFailed(error));
  child.once('exit', (code, signal) => {
    if (!failed) process.exitCode = code ?? (signal ? 130 : 1);
  });
  for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => child.kill(signal));
} catch (error) {
  spawnFailed(error);
}
