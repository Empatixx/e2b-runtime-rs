import { Sandbox, Volume } from 'e2b'

const timed = async (label, action) => {
  const started = performance.now()
  const result = await action()
  console.log(`${label}: ${Math.round(performance.now() - started)} ms`)
  return result
}

const expectEqual = (actual, expected, what) => {
  if (actual !== expected) throw new Error(`${what}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`)
}

const sandbox = await timed('create', () => Sandbox.create('base'))
const hello = await sandbox.commands.run('echo hello from $(whoami)')
expectEqual(hello.stdout.trim(), 'hello from user', 'command output')

await sandbox.files.write('/home/user/notes.txt', 'before pause')
await sandbox.commands.run('sudo apt-get --version > /dev/null && pip3 --version || true')

await timed('pause', () => sandbox.pause())
const resumed = await timed('resume', () => Sandbox.connect(sandbox.sandboxId))
expectEqual(await resumed.files.read('/home/user/notes.txt'), 'before pause', 'file after resume')

const snapshot = await timed('snapshot', () => resumed.createSnapshot())
const fromSnapshot = await timed('create from snapshot', () => Sandbox.create(snapshot.snapshotId))
expectEqual(await fromSnapshot.files.read('/home/user/notes.txt'), 'before pause', 'file in snapshot copy')

const volume = await Volume.create(`home-${Date.now()}`)
const withVolume = await timed('create with volume', () => Sandbox.create('base', { volumeMounts: { '/home/user/data': volume } }))
await withVolume.files.write('/home/user/data/kept.txt', 'kept in volume')
await withVolume.kill()
const again = await Sandbox.create('base', { volumeMounts: { '/home/user/data': volume } })
expectEqual(await again.files.read('/home/user/data/kept.txt'), 'kept in volume', 'file in volume after kill')

const listed = await Sandbox.list().nextItems()
console.log(`listed ${listed.length} sandboxes`)
for (const box of [resumed, fromSnapshot, again]) await box.kill()
console.log('smoke test passed')
