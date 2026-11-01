// A trusted watchdog outside the job's process group survives the signal sweep.
// It inherits only the owner's read pipe, never the job's stdout or stderr.
import { spawnSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { setTimeout as delay } from 'node:timers/promises';
import { fenceOwnedGroup } from './owner-fence.mjs';

const group = Number(process.argv[2]);
const status = process.argv[3];
if (!Number.isSafeInteger(group) || group <= 1 || group === process.pid || !status) throw new Error('Invalid owned process group');
let code;
let released = false;
const record = state => {
  try { writeFileSync(status, JSON.stringify({ state, group, watchdog: process.pid, code }), { mode: 0o600 }); } catch {}
};
const signal = name => {
  if (released || !process.connected) throw new Error('Owned process group lost its helper anchor');
  // Revoke authority before the syscall that releases our identity anchor.
  if (name === 'SIGKILL') released = true;
  try { process.kill(-group, name); return true; }
  catch (error) { if (error.code === 'ESRCH') return false; throw error; }
};
function members() {
  // Fixed executable and arguments; inspect only group identity and state.
  // Zombies cannot fork or retain pipes and may await the system reaper.
  const result = spawnSync('/bin/ps', ['-axo', 'pgid=,stat='], {
    encoding: 'utf8', timeout: 500, maxBuffer: 4 * 1024 * 1024,
    stdio: ['ignore', 'pipe', 'ignore'], env: { PATH: '/usr/bin:/bin', LC_ALL: 'C' },
  });
  if (result.status !== 0) throw new Error('Could not inspect the owned process group');
  return result.stdout.split('\n').map(line => line.trim().split(/\s+/))
    .filter(([id, state]) => Number(id) === group && state && !state.startsWith('Z'))
    .map(([, state]) => state);
}
let stopping = false;
async function stop() {
  if (stopping) return;
  stopping = true;
  process.stdin.destroy();
  try {
    await fenceOwnedGroup({ signal, members, anchored: () => process.connected && !released, delay, now: () => performance.now() });
    record('done');
  } catch {
    if (!released && process.connected) { try { signal('SIGKILL'); } catch {} }
    record('failed');
    process.exitCode = 1;
  }
}
process.on('message', message => {
  if (message?.finish === true && Number.isInteger(message.code)) {
    code = message.code;
    void stop();
  }
});
// Unexpected anchor loss is a failure, never permission to signal its old ID.
process.on('disconnect', () => { if (!stopping) void stop(); });
process.stdin.on('end', stop);
process.stdin.on('error', stop);
process.stdin.resume();
// The helper cannot dispatch a command before this durable handshake.
writeFileSync(status, JSON.stringify({ state: 'ready', group, watchdog: process.pid }), { mode: 0o600 });
process.send?.({ ready: true }, () => {});
