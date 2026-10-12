// Optional npm conveniences. The production backend is a standalone Rust binary.
import { spawn } from 'node:child_process';
import { access } from 'node:fs/promises';
import { homedir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
let cargo = process.env.CARGO_BIN;
if (!cargo) {
  const installed = path.join(process.env.CARGO_HOME || path.join(homedir(), '.cargo'), 'bin', process.platform === 'win32' ? 'cargo.exe' : 'cargo');
  try { await access(installed); cargo = installed; }
  catch { cargo = 'cargo'; }
}
let child;
let stopping = false;
for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => { stopping = true; child?.kill(signal); });
}
function run(program, args) {
  return new Promise((resolve, reject) => {
    child = spawn(program, args, { cwd: root, stdio: 'inherit' });
    child.on('error', reject);
    child.on('exit', (code, signal) => {
      if (signal || stopping) resolve(false);
      else if (code === 0) resolve(true);
      else reject(new Error(`${program} 退出，状态码 ${code}`));
    });
  });
}
try {
  const args = process.argv.slice(2);
  if (args[0] === 'start') {
    if (await run(cargo, ['build', '--quiet'])) {
      const target = path.resolve(root, process.env.CARGO_TARGET_DIR || 'target');
      await run(path.join(target, 'debug', process.platform === 'win32' ? 'agent-web-tmux.exe' : 'agent-web-tmux'), []);
    }
  } else { await run(cargo, args); }
} catch (error) {
  console.error(error.code === 'ENOENT' ? '找不到 Rust 工具链，请先安装 rustup / cargo。' : error.message);
  process.exitCode = 1;
}
