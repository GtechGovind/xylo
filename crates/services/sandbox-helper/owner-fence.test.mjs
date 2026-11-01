import assert from 'node:assert/strict';
import test from 'node:test';
import { fenceOwnedGroup } from './owner-fence.mjs';

test('releasing the anchor ends all signal authority before a group identity can be reused', async () => {
  let owned = true;
  const signals = [];
  await fenceOwnedGroup({
    anchored: () => owned,
    signal: name => {
      assert.ok(owned, 'signaled a recycled group identity');
      signals.push(name);
      if (name === 'SIGKILL') owned = false;
      return true;
    },
    members: () => owned ? ['T', 'T'] : [],
    delay: async () => {},
    now: () => 0,
  });
  assert.deepEqual(signals, ['SIGSTOP', 'SIGKILL']);
});

test('loss of the helper anchor before cleanup grants no signal authority', async () => {
  const signals = [];
  await assert.rejects(fenceOwnedGroup({
    anchored: () => false,
    signal: name => { signals.push(name); return false; },
    members: () => [],
    delay: async () => {},
    now: () => 0,
  }), /anchor/);
  assert.deepEqual(signals, []);
});

test('loss of the anchor during freezing prevents the final destructive signal', async () => {
  let owned = true;
  const signals = [];
  await assert.rejects(fenceOwnedGroup({
    anchored: () => owned,
    signal: name => { signals.push(name); return true; },
    members: () => { owned = false; return ['T']; },
    delay: async () => {},
    now: () => 0,
  }), /anchor/);
  assert.deepEqual(signals, ['SIGSTOP']);
});

test('a live replacement group after release can only fail read-only retirement', async () => {
  let released = false;
  let clock = 0;
  const signals = [];
  await assert.rejects(fenceOwnedGroup({
    anchored: () => !released,
    signal: name => {
      assert.equal(released, false);
      signals.push(name);
      if (name === 'SIGKILL') released = true;
      return true;
    },
    members: () => released ? ['R'] : ['T'],
    delay: async () => { clock += 1000; },
    now: () => clock,
  }), /retire/);
  assert.deepEqual(signals, ['SIGSTOP', 'SIGKILL']);
});
