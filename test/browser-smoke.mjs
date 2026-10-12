import assert from 'node:assert/strict';
import { once } from 'node:events';
import { randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { mkdir, mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import http from 'node:http';
import { chromium } from 'playwright';
import { spawn, execFile } from 'node:child_process';
import { promisify } from 'node:util';
import path from 'node:path';

const root = fileURLToPath(new URL('../', import.meta.url));
const socketName = `codex-web-browser-test-${randomUUID()}`;
const target = path.resolve(root, process.env.CARGO_TARGET_DIR || 'target');
const historyHome = await mkdtemp(path.join(tmpdir(), 'codex-web-chat-test-'));
const daemon = spawn(path.join(target, 'debug/examples/browser_fixture'), [], {
  cwd: root, env: { ...process.env, WEB_TEST_SOCKET: socketName, WEB_TEST_HISTORY: historyHome, CLAUDE_CONFIG_DIR: path.join(historyHome, 'claude'), NODE_BIN: process.execPath },
  stdio: ['ignore', 'pipe', 'pipe'],
});
const exited = new Promise((resolve) => daemon.once('exit', resolve));
let startupError = '';
daemon.stderr.on('data', (chunk) => { startupError += chunk; });
const ready = new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error(`Rust fixture startup timed out: ${startupError}`)), 10000);
  let output = '';
  daemon.stdout.on('data', (chunk) => {
    output += chunk;
    const match = output.match(/Test server: (http:\/\/127\.0\.0\.1:\d+)/);
    if (match) { clearTimeout(timer); resolve(match[1]); }
  });
  daemon.once('error', (error) => { clearTimeout(timer); reject(error); });
  daemon.once('exit', () => { clearTimeout(timer); reject(new Error(startupError || 'Rust fixture exited')); });
});
let base;
let selectedName;
async function status(name = selectedName) { return (await fetch(`${base}/api/session${name ? `?session=${name}` : ''}`)).json(); }
async function capture() { return (await (await fetch(`${base}/api/screen?session=${selectedName}`)).json()).text; }
let browser;
let sourceServer;
try {
  base = await ready;
  browser = await chromium.launch({
    ...(process.env.PLAYWRIGHT_EXECUTABLE_PATH
      ? { executablePath: process.env.PLAYWRIGHT_EXECUTABLE_PATH }
      : { channel: process.env.PLAYWRIGHT_CHANNEL ?? 'chrome' }),
    headless: true,
  });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await mkdir('artifacts', { recursive: true });
  // Reproduce clicking the localhost link from chat or another website.
  sourceServer = http.createServer((_request, response) => {
    response.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
    response.end(`<a href="${base}">打开 Codex 网页</a>`);
  });
  sourceServer.listen(0, '127.0.0.1');
  await once(sourceServer, 'listening');
  const sourceUrl = `http://localhost:${sourceServer.address().port}/`;
  await page.goto(sourceUrl);
  const navigation = page.waitForResponse((response) => response.url() === `${base}/`);
  await page.getByRole('link', { name: '打开 Codex 网页' }).click();
  const response = await navigation;
  assert.equal(response.status(), 200, 'Cross-site top-level navigation must open the page');
  assert.equal((await response.request().allHeaders())['sec-fetch-site'], 'cross-site');
  await page.getByRole('button', { name: '启动并连接' }).waitFor();
  await page.screenshot({ path: 'artifacts/desktop.png', fullPage: true });
  await page.locator('#prompt').fill('中文第一行\nsecond line');
  await page.getByRole('button', { name: '发送消息', exact: true }).click();
  await page.locator('#connection-status[data-state="online"]').waitFor();
  selectedName = await page.locator('.session-row.is-active').getAttribute('data-session');
  const originalName = selectedName;
  const originalPid = (await status()).pid;
  const searchActivity = page.locator('.activity-record').filter({ hasText: '搜索网页' });
  await page.locator('.activity-record[data-status="running"]').waitFor();
  await searchActivity.locator('summary').click();
  assert.equal(await searchActivity.getAttribute('open'), '');
  await page.locator('.message[data-role="assistant"] .markdown').filter({ hasText: '已经收到你的消息' }).waitFor();
  assert.equal(await page.locator('.user-bubble').count(), 1, 'Optimistic and confirmed user messages must merge');
  assert.equal(await page.locator('.user-bubble').innerText(), '中文第一行\nsecond line');
  assert.equal(await page.locator('.markdown pre code').count(), 1, 'Render Markdown code blocks');
  assert.equal(await page.getByRole('button', { name: '复制代码' }).count(), 1);
  assert.equal((await page.locator('body').innerText()).includes('PRIVATE_REASONING_MUST_NOT_RENDER'), false);
  assert.equal((await page.locator('body').innerText()).includes('PRIVATE_TOOL_RESULT_MUST_NOT_RENDER'), false);
  assert.equal(await page.locator('.activity-record').count(), 3, 'Tool events must be shown once in chronological order');
  assert.equal(await searchActivity.getAttribute('data-status'), 'completed');
  assert.equal(await searchActivity.getAttribute('open'), '', 'Activity details must remain open after a status update');
  const readActivity = page.locator('.activity-record').filter({ hasText: '读取文件' });
  await readActivity.locator('summary').click();
  assert.ok((await readActivity.locator('.activity-details').innerText()).includes('/skills/openai-docs/SKILL.md'));
  const openActivity = page.locator('.activity-record').filter({ hasText: '打开网页' });
  await openActivity.locator('summary').click();
  assert.equal(await openActivity.getByRole('link').getAttribute('href'), 'https://developers.openai.com/api/docs/guides/code-generation');
  const roles = await page.locator('#messages > article').evaluateAll((entries) => entries.map((entry) => entry.dataset.role));
  assert.deepEqual(roles, ['user', 'assistant', 'activity', 'activity', 'activity', 'assistant']);
  await page.screenshot({ path: 'artifacts/chat-desktop.png', fullPage: true });
  await page.getByRole('button', { name: '打开终端', exact: true }).click();
  const pasteDeadline = Date.now() + 7000;
  let text;
  do {
    text = await capture();
    if (text.includes('中文第一行\\nsecond line')) break;
    await page.waitForTimeout(50);
  } while (Date.now() < pasteDeadline);
  assert.ok(text.includes('中文第一行\\nsecond line'), 'Multiline Unicode prompt must reach the CLI intact');
  await page.getByRole('button', { name: '断开连接', exact: true }).click();
  await page.locator('#connection-status[data-state="offline"]').waitFor();
  assert.equal((await status()).pid, originalPid);
  await page.getByRole('button', { name: '关闭终端', exact: true }).click();
  await page.getByRole('button', { name: '连接终端', exact: false }).click();
  await page.locator('#connection-status[data-state="online"]').waitFor();
  await page.reload();
  await page.getByRole('button', { name: '连接终端', exact: false }).click();
  await page.locator('#connection-status[data-state="online"]').waitFor();
  assert.equal((await status()).pid, originalPid);
  await page.locator('.message[data-role="assistant"] .markdown').filter({ hasText: '已经收到你的消息' }).waitFor();
  assert.equal(await page.locator('.user-bubble').count(), 1, 'Refresh must restore the real conversation');
  assert.equal(await page.locator('.activity-record').count(), 3, 'Refresh must restore tool activities');

  // New sessions preserve earlier processes and bind each transcript to its own CLI.
  await page.locator('#prompt').fill('第一会话的未发送草稿');
  await page.getByRole('button', { name: '新建会话', exact: true }).click();
  await page.waitForFunction((name) => document.querySelector('.session-row.is-active')?.dataset.session !== name, originalName);
  await page.locator('#connection-status[data-state="online"]').waitFor();
  const secondName = await page.locator('.session-row.is-active').getAttribute('data-session');
  assert.notEqual(secondName, originalName);
  assert.equal((await status(originalName)).pid, originalPid);
  assert.equal((await status(originalName)).alive, true);
  assert.equal(await page.locator('.session-row').count(), 2);
  assert.equal(await page.locator('.user-bubble').count(), 0);
  assert.equal(await page.locator('#prompt').inputValue(), '');
  selectedName = secondName;
  await page.locator('#prompt').fill('第二个独立会话');
  await page.getByRole('button', { name: '发送消息', exact: true }).click();
  await page.locator('.message[data-role="assistant"] .markdown').filter({ hasText: '已经收到你的消息' }).waitFor();
  const firstChat = await (await fetch(`${base}/api/chat?session=${originalName}`)).json();
  const secondChat = await (await fetch(`${base}/api/chat?session=${secondName}`)).json();
  assert.notEqual(firstChat.sessionId, secondChat.sessionId);
  assert.equal(firstChat.messages.filter(message => message.role === 'user')[0].text, '中文第一行\nsecond line');
  assert.equal(secondChat.messages.filter(message => message.role === 'user')[0].text, '第二个独立会话');
  assert.equal(secondChat.messages.filter(message => message.role === 'user').length, 1);
  await page.locator(`[data-select-session="${originalName}"]`).click();
  selectedName = originalName;
  await page.locator('#connection-status[data-state="online"]').waitFor();
  await page.locator('.user-bubble').filter({ hasText: '中文第一行' }).waitFor();
  assert.equal(await page.locator('#prompt').inputValue(), '第一会话的未发送草稿');
  assert.equal(await page.locator('.user-bubble').count(), 1);
  await page.reload();
  await page.locator('.user-bubble').filter({ hasText: '中文第一行' }).waitFor();
  assert.equal(await page.locator('.session-row.is-active').getAttribute('data-session'), originalName);
  await page.getByRole('button', { name: '连接终端', exact: false }).click();
  await page.locator('#connection-status[data-state="online"]').waitFor();
  await page.screenshot({ path: 'artifacts/multi-session-desktop.png', fullPage: true });
  // Dismissing close leaves both sessions alive; closing the background session keeps this connection.
  page.once('dialog', dialog => dialog.dismiss());
  await page.locator(`[data-close-session="${secondName}"]`).click();
  assert.equal((await status(secondName)).alive, true);
  page.once('dialog', dialog => dialog.accept());
  await page.locator(`[data-close-session="${secondName}"]`).click();
  await page.locator(`[data-close-session="${secondName}"]`).waitFor({ state: 'detached' });
  assert.equal((await status(secondName)).exists, false);
  assert.equal((await status(originalName)).pid, originalPid);
  assert.equal(await page.locator('#connection-status').getAttribute('data-state'), 'online');

  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(200);
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth), false, 'Mobile layout must fit the viewport');
  await page.screenshot({ path: 'artifacts/mobile.png', fullPage: true });
  page.once('dialog', (dialog) => dialog.accept());
  // The sidebar is intentionally hidden on mobile; return to desktop to end the session.
  await page.setViewportSize({ width: 1440, height: 1000 });
  await page.getByRole('button', { name: '关闭会话', exact: true }).click();
  await page.getByRole('button', { name: '启动并连接' }).waitFor();
  assert.equal((await status()).exists, false);
  // Claude uses a different native transcript adapter, and custom CLIs use the raw terminal.
  await page.locator('#agent-picker').selectOption('claude');
  await page.getByRole('button', {name:'新建会话',exact:true}).click();
  await page.locator('#connection-status[data-state="online"]').waitFor();
  selectedName = await page.locator('.session-row.is-active').getAttribute('data-session');
  await page.locator('#prompt').fill('Claude 的独立消息');
  await page.getByRole('button',{name:'发送消息',exact:true}).click();
  await page.locator('.markdown').filter({hasText:'Claude Code 已收到你的消息'}).waitFor();
  const claudeChat = await (await fetch(`${base}/api/chat?session=${selectedName}`)).json();
  assert.equal(claudeChat.source,'claude');
  assert.equal(claudeChat.messages.filter(message=>message.role==='user')[0].text,'Claude 的独立消息');
  assert.equal(await page.locator('.assistant-name').first().innerText(),'Claude Code');
  assert.equal((await page.locator('body').innerText()).includes('PRIVATE_'),false);
  await page.reload();
  await page.locator('.markdown').filter({hasText:'Claude Code 已收到你的消息'}).waitFor();
  await page.screenshot({path:'artifacts/claude-agent.png',fullPage:true});
  page.once('dialog',dialog=>dialog.accept());
  await page.getByRole('button',{name:'关闭会话',exact:true}).click();
  await page.locator('.session-row').waitFor({state:'detached'});
  await page.locator('#agent-picker').selectOption('custom');
  await page.getByRole('button',{name:'新建会话',exact:true}).click();
  await page.locator('#connection-status[data-state="online"]').waitFor();
  await page.locator('#terminal-drawer.is-open').waitFor();
  assert.equal(await page.locator('#prompt').isDisabled(),true);
  selectedName = await page.locator('.session-row.is-active').getAttribute('data-session');
  assert.ok((await capture()).includes('CUSTOM_AGENT_READY'));
  page.once('dialog',dialog=>dialog.accept());
  await page.getByRole('button',{name:'关闭当前会话'}).click();
  await page.locator('.session-row').waitFor({state:'detached'});

  assert.deepEqual(errors, []);
  console.log('Browser smoke passed: independent sessions, transcript isolation, switch/refresh, draft preservation, cancel close, close background/current, activity records, mobile layout and access guards.');
} finally {
  await browser?.close();
  if (sourceServer) await new Promise((resolve) => { sourceServer.close(resolve); sourceServer.closeAllConnections(); });
  daemon.kill('SIGTERM');
  const shutdownTimeout = setTimeout(() => daemon.kill('SIGKILL'), 8000);
  await exited;
  clearTimeout(shutdownTimeout);
  await promisify(execFile)('tmux', ['-L', socketName, 'kill-server']).catch(() => {});
  await rm(historyHome, { recursive: true, force: true });
}
