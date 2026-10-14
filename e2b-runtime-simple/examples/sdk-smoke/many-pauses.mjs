import { Sandbox } from 'e2b'

const ROUNDS = Number(process.env.ROUNDS ?? 40)
const sandbox = await Sandbox.create('base')
let current = sandbox
for (let round = 1; round <= ROUNDS; round++) {
  await current.files.write(`/home/user/round-${round}.txt`, `round ${round}`)
  await current.pause()
  current = await Sandbox.connect(sandbox.sandboxId)
}
const listed = (await current.commands.run('ls /home/user | grep -c round-')).stdout.trim()
if (listed !== String(ROUNDS)) throw new Error(`expected ${ROUNDS} files, found ${listed}`)
for (const round of [1, Math.floor(ROUNDS / 2), ROUNDS]) {
  const content = await current.files.read(`/home/user/round-${round}.txt`)
  if (content !== `round ${round}`) throw new Error(`round ${round}: ${content}`)
}
await current.pause()
console.log(sandbox.sandboxId)
console.log(`many pauses test passed after ${ROUNDS} rounds`)
