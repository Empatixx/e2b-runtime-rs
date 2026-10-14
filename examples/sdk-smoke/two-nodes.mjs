import { Sandbox } from 'e2b'

const apiKey = process.env.E2B_API_KEY
const nodeA = { apiUrl: 'http://localhost:13000', sandboxUrl: 'http://localhost:13001', apiKey }
const nodeB = { apiUrl: 'http://localhost:13100', sandboxUrl: 'http://localhost:13101', apiKey }

const timed = async (label, action) => {
  const started = performance.now()
  const result = await action()
  console.log(`${label}: ${Math.round(performance.now() - started)} ms`)
  return result
}

const expectEqual = (actual, expected, what) => {
  if (actual !== expected) throw new Error(`${what}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`)
}

const onA = await timed('create on A', () => Sandbox.create('base', nodeA))
await onA.files.write('/home/user/notes.txt', 'written on A')
await onA.commands.run('sleep 3600', { background: true })
const sleeperOnA = (await onA.commands.run('pgrep -x sleep')).stdout.trim()
await timed('pause on A', () => onA.pause())

const onB = await timed('resume on B', () => Sandbox.connect(onA.sandboxId, nodeB))
expectEqual(await onB.files.read('/home/user/notes.txt'), 'written on A', 'file after moving to B')
expectEqual((await onB.commands.run('pgrep -x sleep')).stdout.trim(), sleeperOnA, 'process kept in memory')

const seenFromA = await Sandbox.getInfo(onA.sandboxId, nodeA)
console.log('A sees the sandbox on domain', seenFromA.sandboxDomain ?? seenFromA.domain)
await timed('pause on B through A', () => Sandbox.pause(onA.sandboxId, nodeA))
const back = await timed('resume on A', () => Sandbox.connect(onA.sandboxId, nodeA))
expectEqual(await back.files.read('/home/user/notes.txt'), 'written on A', 'file back on A')
await Sandbox.kill(onA.sandboxId, nodeB)
console.log('two node test passed')
