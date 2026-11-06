// `npm run dev`: starts a local LiveKit dev server and the Streamroom app together.
import { spawn } from 'node:child_process';

const procs = [];
function run(name, cmd, args) {
  const p = spawn(cmd, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  const tag = (d) => d.toString().split('\n').filter(Boolean).forEach((l) => console.log(`[${name}] ${l}`));
  p.stdout.on('data', tag);
  p.stderr.on('data', tag);
  p.on('error', (e) => {
    if (e.code === 'ENOENT' && name === 'livekit') {
      console.error('\nlivekit-server is not installed. See "Run it locally" in README.md.\n');
    } else console.error(`[${name}]`, e.message);
    shutdown(1);
  });
  p.on('exit', (code) => { console.log(`[${name}] exited (${code})`); shutdown(code || 0); });
  procs.push(p);
}
let down = false;
function shutdown(code) {
  if (down) return; down = true;
  procs.forEach((p) => p.kill());
  process.exit(code);
}
process.on('SIGINT', () => shutdown(0));
process.on('SIGTERM', () => shutdown(0));

// --dev = devkey/secret credentials; bind 0.0.0.0 so other machines on your network can watch.
run('livekit', 'livekit-server', ['--dev', '--bind', '0.0.0.0']);
run('app', process.execPath, ['server/index.js']);
