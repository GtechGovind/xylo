// Xylo's managed process boundary. stdout belongs exclusively to the child
// (including MCP JSON-RPC); setup errors are diagnostic stderr only.
import { SandboxManager } from '@anthropic-ai/sandbox-runtime';
import { spawn } from 'node:child_process';
import { realpathSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const quote = value => `'${String(value).replaceAll("'", "'\\''")}'`;
let child;
try {
  const [major, minor] = process.versions.node.split('.').map(Number);
  if (major < 22 || (major === 22 && minor < 12)) throw new Error('Node.js 22.12 or later is required');
  const childEnvironment = JSON.parse(process.env.XYLO_SANDBOX_CHILD_ENV ?? '{}');
  delete process.env.XYLO_SANDBOX_CHILD_ENV;
  if (!childEnvironment || Array.isArray(childEnvironment) || typeof childEnvironment !== 'object') throw new Error('Invalid child environment');
  for (const [name, value] of Object.entries(childEnvironment)) {
    if (!/^[A-Za-z0-9_]{1,128}$/.test(name) || typeof value !== 'string' || name === 'XYLO_SANDBOX_CHILD_ENV') throw new Error('Reserved child environment variable');
  }
  const spec = JSON.parse(process.argv[2]);
  const workspace = realpathSync(spec.workspace);
  if (!Array.isArray(spec.args) || !spec.args.every(a => typeof a === 'string')) throw new Error('Invalid argument array');
  const allowed = [...(spec.read_roots ?? []), ...(spec.toolchain_read_roots ?? [])].map(p => realpathSync(p));
  const codingEnvironment = spec.coding_environment ?? {};
  const helperDirectory = dirname(fileURLToPath(import.meta.url));
  const childEntry = realpathSync(join(helperDirectory, 'child-entry.mjs'));
  await SandboxManager.initialize({
    network: { allowedDomains: spec.allowed_domains ?? [], deniedDomains: [] },
    filesystem: {
      denyRead: [homedir(), ...(spec.protected_paths ?? [])],
      allowRead: [workspace, ...allowed, helperDirectory, process.execPath],
      allowWrite: [workspace, spec.temporary],
      denyWrite: [join(workspace, '.git'), join(workspace, '.agents'), join(workspace, '.claude'), join(workspace, '.codex'), ...allowed, ...(spec.protected_paths ?? []), ...(spec.trusted_paths ?? [])],
    },
  });
  const wrapped = await SandboxManager.wrapWithSandbox([process.execPath, childEntry, spec.program, ...spec.args].map(quote).join(' '));
  child = spawn(wrapped, { shell: '/bin/sh', cwd: workspace, stdio: 'inherit', env: { ...process.env, XYLO_SANDBOX_CHILD_ENV: JSON.stringify({ TMPDIR: spec.temporary, ...codingEnvironment, ...childEnvironment }), TMPDIR: spec.temporary } });
  child.once('error', async () => { await SandboxManager.reset(); process.exitCode = 125; });
  child.once('exit', async (code, signal) => { await SandboxManager.reset(); process.exitCode = code ?? (signal ? 130 : 125); });
  for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => child?.kill(signal));
} catch (error) {
  console.error(`Xylo sandbox unavailable: ${error.message}`);
  await SandboxManager.reset().catch(() => {});
  process.exitCode = 125;
}
