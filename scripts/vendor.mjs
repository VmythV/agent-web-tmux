import { copyFile, mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('../', import.meta.url));
await mkdir(path.join(root, 'public/vendor'), { recursive: true });
for (const [source, destination] of [
  ['@xterm/xterm/lib/xterm.js', 'xterm.js'],
  ['@xterm/xterm/css/xterm.css', 'xterm.css'],
  ['@xterm/xterm/LICENSE', 'xterm.LICENSE'],
  ['@xterm/addon-fit/lib/addon-fit.js', 'addon-fit.js'],
  ['@xterm/addon-fit/LICENSE', 'addon-fit.LICENSE'],
  ['marked/lib/marked.umd.js', 'marked.js'],
  ['marked/LICENSE', 'marked.LICENSE'],
  ['dompurify/dist/purify.min.js', 'purify.js'],
  ['dompurify/LICENSE', 'purify.LICENSE'],
]) {
  await copyFile(path.join(root, 'node_modules', source), path.join(root, 'public/vendor', destination));
}
