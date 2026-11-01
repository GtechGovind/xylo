// The helper remains alive as the group identity anchor until the final kill.
// After releasing it, inspection is read-only: a numeric PGID is no authority.
export async function fenceOwnedGroup({ signal, members, anchored, delay, now }) {
  const deadline = now() + 2000;
  for (;;) {
    if (!anchored()) throw new Error('Owned process group lost its helper anchor');
    if (!signal('SIGSTOP')) throw new Error('Owned process group lost its helper anchor');
    const live = members();
    if (live.length === 0) throw new Error('Owned process group lost its helper anchor');
    if (live.every(state => state.startsWith('T'))) {
      if (!anchored()) throw new Error('Owned process group lost its helper anchor');
      signal('SIGKILL');
      break;
    }
    if (now() >= deadline) throw new Error('Owned process group did not stop');
    await delay(10);
  }
  while (members().length !== 0) {
    if (now() >= deadline) throw new Error('Owned process group did not retire');
    await delay(10);
  }
}
