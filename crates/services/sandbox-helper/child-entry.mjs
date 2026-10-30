// This entrypoint itself starts INSIDE the established OS sandbox. Untrusted
// component environment never reaches Node startup or the sandbox bootstrap.
import { spawn } from 'node:child_process';
const environment = JSON.parse(process.env.XYLO_SANDBOX_CHILD_ENV ?? '{}');
delete process.env.XYLO_SANDBOX_CHILD_ENV;
if (!environment || Array.isArray(environment) || typeof environment !== 'object') throw new Error('Invalid child environment');
for (const [name, value] of Object.entries(environment)) {
  if (!/^[A-Za-z0-9_]{1,128}$/.test(name) || typeof value !== 'string' || name === 'XYLO_SANDBOX_CHILD_ENV') throw new Error('Invalid child environment');
}
const child = spawn(process.argv[2], process.argv.slice(3), {
  stdio: 'inherit', env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1', ...environment },
});
child.once('error', () => { process.exitCode = 125; });
child.once('exit', (code, signal) => { process.exitCode = code ?? (signal ? 130 : 125); });
for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => child.kill(signal));
