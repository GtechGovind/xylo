// Xylo's managed process boundary. stdout and stderr belong exclusively to
// the sandboxed program (stdout may carry MCP JSON-RPC).
//
// Status contract with the Rust launcher (xylo_services::sandbox): the spec
// names a private `status_file` that the sandboxed program can neither read
// nor write. This process appends one JSON object per line to it:
//   {"kind":"unavailable","reason":"..."}    the sandbox could not be set up;
//                                            nothing ran; exit status 125
//   {"kind":"spawn_failed","reason":"..."}   the program could not be started
//                                            inside the sandbox; exit status 127
//   {"kind":"network_denied","host":"h:p"}   an outbound connection was blocked
// Only this file is evidence that nothing ran: anything the program prints,
// including look-alike lines, is the program's own output. Inside the sandbox,
// child-entry.mjs reports a spawn failure over a private pipe (fd 3) that the
// program never receives; this process copies it into the status file.
//
// Network: `spec.network` is "none" (default) or "unrestricted". Hosts in
// `spec.allowed_domains` are always allowed. Any other host is decided by the
// runtime's ask callback: "unrestricted" allows it (the runtime has no
// allow-all pattern for allowedDomains; its resolved-address check still
// refuses loopback, link-local and metadata addresses), "none" denies it and
// records it once per destination.
//
// Never call process.exit(): setting process.exitCode lets output drain.
import { spawn } from 'node:child_process';
import { appendFileSync, realpathSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const UNAVAILABLE_EXIT = 125;
const SPAWN_FAILED_EXIT = 127;
const quote = value => `'${String(value).replaceAll("'", "'\\''")}'`;
const singleLine = value => String(value?.message ?? value).replace(/\s+/g, ' ').trim();

let manager;
let child;
let statusFile;
let unavailableReported = false;
const reported = new Set();

function record(entry) {
  if (statusFile) appendFileSync(statusFile, `${JSON.stringify(entry)}\n`, { mode: 0o600 });
}

async function unavailable(reason) {
  if (unavailableReported) return;
  unavailableReported = true;
  record({ kind: 'unavailable', reason: singleLine(reason) });
  process.exitCode = UNAVAILABLE_EXIT;
  await manager?.reset().catch(() => {});
}

function networkDecision(unrestricted) {
  return async ({ host, port }) => {
    if (unrestricted) return true;
    const destination = singleLine(`${host}:${port}`);
    if (!reported.has(destination)) {
      reported.add(destination);
      record({ kind: 'network_denied', host: destination });
    }
    const reason = 'Network access is not approved for this command; request network access and run it again';
    return manager.askCallbackDenyReason ? { allow: false, reason } : false;
  };
}

// Reports from child-entry.mjs arrive on fd 3. Only a spawn failure is
// accepted, and only before the program started.
function readControl(stream, onSpawnFailed) {
  let buffer = '';
  stream.setEncoding('utf8');
  stream.on('data', data => {
    buffer += data;
    if (buffer.length > 65536) { stream.destroy(); return; }
    let newline;
    while ((newline = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, newline);
      buffer = buffer.slice(newline + 1);
      try {
        const message = JSON.parse(line);
        if (message?.kind === 'spawn_failed') onSpawnFailed(singleLine(message.reason ?? 'unknown reason'));
      } catch {}
    }
  });
  stream.on('error', () => {});
}

try {
  const spec = JSON.parse(process.argv[2]);
  if (typeof spec.status_file === 'string' && spec.status_file) statusFile = spec.status_file;
  const [major, minor] = process.versions.node.split('.').map(Number);
  if (major < 22 || (major === 22 && minor < 12)) throw new Error('Node.js 22.12 or later is required');
  // Imported here so a missing or broken runtime package is recorded as
  // unavailable instead of an uncaught module error.
  ({ SandboxManager: manager } = await import('@anthropic-ai/sandbox-runtime'));
  const childEnvironment = JSON.parse(process.env.XYLO_SANDBOX_CHILD_ENV ?? '{}');
  delete process.env.XYLO_SANDBOX_CHILD_ENV;
  if (!childEnvironment || Array.isArray(childEnvironment) || typeof childEnvironment !== 'object') throw new Error('Invalid child environment');
  for (const [name, value] of Object.entries(childEnvironment)) {
    if (!/^[A-Za-z0-9_]{1,128}$/.test(name) || typeof value !== 'string' || name === 'XYLO_SANDBOX_CHILD_ENV') throw new Error('Reserved child environment variable');
  }
  if (!statusFile) throw new Error('Missing status file');
  const control = dirname(statusFile);
  const workspace = realpathSync(spec.workspace);
  if (typeof spec.program !== 'string' || !spec.program) throw new Error('Invalid program');
  if (!Array.isArray(spec.args) || !spec.args.every(a => typeof a === 'string')) throw new Error('Invalid argument array');
  const allowed = [...(spec.read_roots ?? []), ...(spec.toolchain_read_roots ?? [])].map(p => realpathSync(p));
  const codingEnvironment = spec.coding_environment ?? {};
  const helperDirectory = dirname(fileURLToPath(import.meta.url));
  const childEntry = realpathSync(join(helperDirectory, 'child-entry.mjs'));
  const network = spec.network ?? 'none';
  if (network !== 'none' && network !== 'unrestricted') throw new Error('Invalid network policy');
  await manager.initialize({
    network: { allowedDomains: spec.allowed_domains ?? [], deniedDomains: [] },
    filesystem: {
      denyRead: [homedir(), control, ...(spec.protected_paths ?? [])],
      allowRead: [workspace, ...allowed, helperDirectory, process.execPath],
      allowWrite: [workspace, spec.temporary],
      denyWrite: [join(workspace, '.git'), join(workspace, '.agents'), join(workspace, '.claude'), join(workspace, '.codex'), control, ...allowed, ...(spec.protected_paths ?? []), ...(spec.trusted_paths ?? [])],
    },
  }, networkDecision(network === 'unrestricted'));
  const wrapped = await manager.wrapWithSandbox([process.execPath, childEntry, spec.program, ...spec.args].map(quote).join(' '));
  child = spawn(wrapped, { shell: '/bin/sh', cwd: workspace, stdio: ['inherit', 'inherit', 'inherit', 'pipe'], env: { ...process.env, XYLO_SANDBOX_CHILD_ENV: JSON.stringify({ TMPDIR: spec.temporary, ...codingEnvironment, ...childEnvironment }), TMPDIR: spec.temporary } });
  let failed = false;
  let spawnFailed = false;
  readControl(child.stdio[3], reason => {
    if (spawnFailed) return;
    spawnFailed = true;
    record({ kind: 'spawn_failed', reason });
  });
  child.once('error', async error => { failed = true; await unavailable(`could not start the sandbox wrapper: ${singleLine(error)}`); });
  child.once('exit', async (code, signal) => {
    await manager.reset();
    if (!failed) process.exitCode = spawnFailed ? SPAWN_FAILED_EXIT : code ?? (signal ? 130 : 1);
  });
  for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => child?.kill(signal));
} catch (error) {
  await unavailable(error);
}
