import { Sandbox } from 'e2b'

const reachable = async (sandbox, url) => {
  const result = await sandbox.commands.run(`curl -s -m 2 -o /dev/null -w "%{http_code}" ${url} || true`)
  return result.stdout.trim() !== '000'
}

const a = await Sandbox.create('base')
const b = await Sandbox.create('base')
const checks = {
  'internet is reachable': await reachable(a, 'https://example.com'),
  'host API is blocked': !(await reachable(a, 'http://10.12.0.1:3000/health')),
  'other sandboxes are blocked': !(await Promise.all(
    Array.from({ length: 12 }, (_, i) => reachable(a, `http://10.11.0.${i + 1}:49983/health`)),
  )).some(Boolean),
}
await Promise.all([a.kill(), b.kill()])
console.log(checks)
if (Object.values(checks).includes(false)) throw new Error('isolation check failed')
console.log('isolation test passed')
