import { Sandbox } from 'e2b'

const sandbox = await Sandbox.create()
console.log('sandbox:', sandbox.sandboxId)

const hello = await sandbox.commands.run('echo "hello from envd-rs as $(whoami) in $(pwd)"')
console.log('1) commands.run →', hello.stdout.trim())

await sandbox.files.write('/home/user/notes/todo.txt', 'buy milk\nwrite rust\n')
console.log('2) files.read  →', JSON.stringify(await sandbox.files.read('/home/user/notes/todo.txt')))
console.log('3) files.list  →', (await sandbox.files.list('/home/user/notes')).map((entry) => entry.name))

const streamed = []
await sandbox.commands.run('for i in 1 2 3; do echo tick $i; sleep 0.3; done', {
  onStdout: (line) => streamed.push(line.trim()),
})
console.log('4) streaming   →', streamed)

const background = await sandbox.commands.run('sleep 30', { background: true })
console.log('5) background  → pid', background.pid, 'running:', (await sandbox.commands.list()).map((p) => p.cmd))
await background.kill()

const terminal = await sandbox.pty.create({ cols: 80, rows: 24, onData: (data) => process.stdout.write(new TextDecoder().decode(data)) })
await sandbox.pty.sendInput(terminal.pid, new TextEncoder().encode('echo "6) pty    → $TERM $(tty)"; exit\n'))
await terminal.wait()
console.log('\ndone')
