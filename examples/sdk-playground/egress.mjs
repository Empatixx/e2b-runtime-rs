import { Sandbox, CommandExitError } from 'e2b'

const blocked = process.env.BLOCKED_URL
if (!blocked) throw new Error('BLOCKED_URL is not set')

const sandbox = await Sandbox.create()

const internet = await sandbox.commands.run("curl -s -m 5 -o /dev/null -w '%{http_code}' http://1.1.1.1")
if (internet.stdout.trim() === '000') {
  console.error('guest cannot reach the internet')
  process.exit(1)
}

const reachedPrivate = await sandbox.commands
  .run(`curl -s -m 3 -o /dev/null http://${blocked}`)
  .then(() => true)
  .catch((error) => {
    if (error instanceof CommandExitError && error.exitCode !== 0) return false
    throw error
  })
if (reachedPrivate) {
  console.error(`guest reached the private address ${blocked}`)
  process.exit(1)
}

console.log('egress ok')
