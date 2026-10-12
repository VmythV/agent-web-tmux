const $ = (id) => document.getElementById(id);
const icon = (name) => `<svg aria-hidden="true"><use href="#i-${name}"/></svg>`;
const terminal = new Terminal({
  cursorBlink: true, fontFamily: '"SFMono-Regular", Menlo, Consolas, monospace',
  fontSize: 11, lineHeight: 1.35, scrollback: 10000,
  theme: { background: '#ffffff', foreground: '#5c6650', cursor: '#83976a', selectionBackground: '#dfe8d4', black: '#4b5440', brightBlack: '#9da78f', green: '#718a51', brightGreen: '#8ca76a', blue: '#6f8bc0', brightBlue: '#89a3d2', white: '#626e51', brightWhite: '#74835f' },
});
const fit = new FitAddon.FitAddon();
terminal.loadAddon(fit);
terminal.open($('terminal'));
let socket = null;
let connected = false;
let shouldReconnect = false;
let reconnectAttempt = 0;
let reconnectTimer = null;
let noticeTimer = null;
let lastSize = '';
let attachCommand = '';
let workspacePath = '';
let sessionExists = false;
let selectedSession = null;
let sessions = [];
let sessionMutation = false;
let sessionsRequest = false;
let sessionsVersion = 0;
let agents = [{ id: 'codex', label: 'Codex', chatSupported: true }];
let agentPickerInitialized = false;
let bootstrapped = false;
const sessionViews = new Map();
let history = [];
let pending = null;
let queued = null;
let flushing = false;
let nativeBusy = false;
let chatRequest = false;
let nativeSessionId = null;
let sessionEpoch = 0;
const nodes = new Map();

function notice(text, error = false) {
  clearTimeout(noticeTimer);
  $('notice').textContent = text;
  $('notice').dataset.error = String(error);
  $('notice').hidden = false;
  if (!error) noticeTimer = setTimeout(() => { $('notice').hidden = true; }, 5000);
}

async function copy(text, success = '已复制') {
  try { await navigator.clipboard.writeText(text); notice(success); }
  catch { notice('复制失败，请允许浏览器访问剪贴板。', true); }
}

function currentAgent() {
  const kind = sessions.find(session => session.session === selectedSession)?.agent || $('agent-picker').value;
  return agents.find(agent => agent.id === kind) || { id: kind, label: kind, chatSupported: kind !== 'custom' };
}

function busy() { return nativeBusy || Boolean(queued) || Boolean(pending && pending.state !== 'failed'); }
function updateComposer() {
  const working = busy();
  $('send-prompt').disabled = !$('prompt').value.trim() || working || sessionMutation || !currentAgent().chatSupported;
  $('prompt').disabled = !currentAgent().chatSupported;
  $('prompt').placeholder = currentAgent().chatSupported ? `向 ${currentAgent().label} 描述你的任务…` : '请在终端中操作此 agent';
  $('agent-label').textContent = currentAgent().label;
  $('stop-generation').hidden = !working || !connected;
  $('thinking').hidden = !working;
  const runningActivity = history.findLast((message) => message.activity?.status === 'running');
  $('thinking-text').textContent = nativeBusy ? runningActivity ? `${currentAgent().label} 正在${runningActivity.text}…` : `${currentAgent().label} 正在处理…` : pending?.state === 'queued' ? '正在准备 agent 会话…' : '消息已发送，等待回复…';
  $('composer-status').textContent = working ? '正在处理你的任务' : connected ? '已连接到工作空间' : '发送消息即可开始';
}

function renderMessage(message) {
  let entry = nodes.get(message.id);
  if (!entry) {
    entry = document.createElement('article');
    entry.dataset.id = message.id;
    nodes.set(message.id, entry);
  }
  const signature = JSON.stringify([message.role, message.phase, message.state, message.text, message.activity]);
  if (entry.dataset.signature === signature) return entry;
  const expanded = entry.querySelector('.activity-record')?.open || false;
  entry.dataset.signature = signature;
  entry.className = `message ${message.role}${message.phase === 'commentary' ? ' commentary' : ''}`;
  entry.dataset.role = message.role;
  entry.replaceChildren();
  if (message.role === 'user') {
    const bubble = document.createElement('div');
    bubble.className = 'user-bubble';
    bubble.textContent = message.text;
    entry.append(bubble);
    if (message.state) {
      const meta = document.createElement('div');
      meta.className = 'message-meta';
      meta.textContent = message.state === 'failed' ? '未发送' : message.state === 'queued' ? '准备发送…' : '已发送';
      entry.append(meta);
    }
  } else if (message.role === 'activity') {
    const activity = message.activity || {};
    const record = document.createElement('details');
    record.className = 'activity-record';
    record.open = expanded;
    record.dataset.status = activity.status;
    const summary = document.createElement('summary');
    const activityIcon = { read: 'folder', files: 'folder', search: 'search', web: 'globe', command: 'terminal', edit: 'new', image: 'image' }[activity.kind] || 'spark';
    const graphic = document.createElement('span');
    graphic.className = 'activity-icon';
    graphic.innerHTML = icon(activityIcon);
    const title = document.createElement('span');
    title.className = 'activity-title';
    title.textContent = message.text;
    const preview = document.createElement('span');
    preview.className = 'activity-preview';
    preview.textContent = (activity.details || []).slice(0, 2).join(' · ');
    const status = document.createElement('span');
    status.className = 'activity-status';
    status.textContent = { running: '进行中', completed: '已完成', failed: '失败', interrupted: '已中断', finished: '已结束' }[activity.status] || '已记录';
    const chevron = document.createElement('span');
    chevron.className = 'activity-chevron';
    chevron.innerHTML = icon('chevron');
    summary.append(graphic, title, preview, status, chevron);
    const body = document.createElement('div');
    body.className = 'activity-details';
    for (const detail of activity.details || []) {
      let url;
      try { url = new URL(detail); } catch { /* File paths and commands stay plain text. */ }
      const line = document.createElement(url && ['http:', 'https:'].includes(url.protocol) ? 'a' : 'p');
      line.textContent = detail;
      if (line.tagName === 'A') { line.href = url.href; line.target = '_blank'; line.rel = 'noopener noreferrer'; }
      body.append(line);
    }
    if (!body.children.length) {
      const line = document.createElement('p');
      line.textContent = '这项操作没有附加详情。';
      body.append(line);
    }
    record.append(summary, body);
    entry.append(record);
  } else {
    const heading = document.createElement('div');
    heading.className = 'assistant-heading';
    heading.innerHTML = `<span class="assistant-avatar">${icon('code')}</span><span class="assistant-name"></span>`;
    heading.querySelector('.assistant-name').textContent = currentAgent().label;
    const body = document.createElement('div');
    body.className = 'markdown';
    body.innerHTML = DOMPurify.sanitize(marked.parse(message.text, { breaks: true }), {
      USE_PROFILES: { html: true }, FORBID_TAGS: ['style', 'form', 'input', 'textarea', 'button', 'img'], FORBID_ATTR: ['style'],
    });
    body.querySelectorAll('a').forEach((link) => { link.target = '_blank'; link.rel = 'noopener noreferrer'; });
    body.querySelectorAll('pre').forEach((pre) => {
      const code = pre.querySelector('code');
      if (!code) return;
      const bar = document.createElement('div');
      bar.className = 'code-bar';
      const language = document.createElement('span');
      language.textContent = code.className.match(/language-([\w-]+)/)?.[1] || '代码';
      const button = document.createElement('button');
      button.innerHTML = `${icon('copy')}<span>复制代码</span>`;
      button.addEventListener('click', () => copy(code.textContent, '代码已复制'));
      bar.append(language, button);
      pre.prepend(bar);
    });
    const actions = document.createElement('div');
    actions.className = 'message-actions';
    const button = document.createElement('button');
    button.title = '复制回复';
    button.setAttribute('aria-label', '复制回复');
    button.innerHTML = icon('copy');
    button.addEventListener('click', () => copy(message.text, '回复已复制'));
    actions.append(button);
    entry.append(heading, body, actions);
  }
  return entry;
}

function renderChat() {
  const scroll = $('chat-scroll');
  const follow = scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight < 100;
  const messages = pending ? [...history, pending] : history;
  const fragment = document.createDocumentFragment();
  const liveIds = new Set();
  for (const message of messages) { liveIds.add(message.id); fragment.append(renderMessage(message)); }
  for (const id of nodes.keys()) if (!liveIds.has(id)) nodes.delete(id);
  $('messages').replaceChildren(fragment);
  $('welcome').hidden = messages.length > 0;
  const first = messages.find((message) => message.role === 'user');
  const title = first ? Array.from(first.text.replace(/\s+/g, ' ')).slice(0, 22).join('') : '新对话';
  $('thread-title').textContent = title;
  const selected = sessions.find((session) => session.session === selectedSession);
  if (selected && first) selected.title = title;
  renderSessions();
  document.querySelector('.header-title').textContent = title;
  updateComposer();
  if (follow) requestAnimationFrame(() => { scroll.scrollTop = scroll.scrollHeight; });
}

async function refreshChat() {
  if (chatRequest || !currentAgent().chatSupported || (!sessionExists && !connected)) return;
  chatRequest = true;
  const requestEpoch = sessionEpoch;
  try {
    const response = await fetch(sessionUrl('/api/chat'));
    const data = await response.json();
    if (requestEpoch !== sessionEpoch) return;
    if (!response.ok) throw new Error(data.error);
    if (nativeSessionId && data.sessionId && nativeSessionId !== data.sessionId) { history = []; pending = null; }
    if (data.sessionId) nativeSessionId = data.sessionId;
    const messages = data.messages || [];
    if (pending) {
      const confirmed = messages.find((message) => message.role === 'user' && message.text.trim() === pending.text.trim() && !pending.knownIds.has(message.id));
      if (confirmed) pending = null;
    }
    history = messages;
    nativeBusy = Boolean(data.busy);
    renderChat();
  } catch { /* Keep the last confirmed conversation during a transient disconnect. */ }
  finally { chatRequest = false; }
}

function setConnection(state, label) {
  connected = state === 'online';
  $('connection-status').dataset.state = state;
  $('connection-status').querySelector('b').textContent = label;
  $('connect').disabled = connected || state === 'connecting';
  $('connect').textContent = connected ? '已连接' : state === 'connecting' ? '连接中…' : sessionExists ? '连接终端' : '启动并连接';
  $('disconnect').disabled = !connected;
  document.querySelectorAll('[data-key]').forEach((button) => { button.disabled = !connected; });
  updateComposer();
}

function sessionUrl(path, name = selectedSession) {
  return name ? `${path}?session=${encodeURIComponent(name)}` : path;
}

function storageKey() { return `codex-web-session:${workspacePath}`; }
function rememberSession() {
  try { if (selectedSession) localStorage.setItem(storageKey(), selectedSession); else localStorage.removeItem(storageKey()); } catch {}
}

function updateSessionDetails() {
  const state = sessions.find((session) => session.session === selectedSession);
  sessionExists = Boolean(state);
  $('session-state').textContent = state ? state.alive ? '会话运行中' : '进程已退出' : '准备开始';
  $('end-session').disabled = !state || sessionMutation;
  $('copy-screen').disabled = !state;
  attachCommand = state ? `tmux -L ${state.socket} attach -t ${state.session}` : '';
  $('attach-command').textContent = attachCommand || '新建会话后可复制接管命令';
  $('terminal-session-name').textContent = state?.session || '未选择会话';
}

function renderSessions() {
  const list = $('session-list');
  const fragment = document.createDocumentFragment();
  if (!sessions.length) {
    const empty = document.createElement('p');
    empty.className = 'sessions-empty';
    empty.textContent = '点击「新建会话」开始';
    fragment.append(empty);
  }
  for (const session of sessions) {
    const row = document.createElement('div');
    row.className = `session-row${session.session === selectedSession ? ' is-active' : ''}`;
    row.dataset.session = session.session;
    const button = document.createElement('button');
    button.className = 'nav-button session-select';
    button.dataset.selectSession = session.session;
    button.setAttribute('aria-label', `切换到：${session.title}`);
    button.setAttribute('aria-current', session.session === selectedSession ? 'true' : 'false');
    button.innerHTML = icon('chat');
    const title = document.createElement('span');
    title.textContent = session.title;
    button.title = `${agents.find(agent => agent.id === session.agent)?.label || session.agent} · ${session.title}`;
    const dot = document.createElement('span');
    dot.className = `chat-dot${session.alive ? '' : ' is-dead'}`;
    dot.title = session.alive ? '进程运行中' : '进程已退出';
    button.append(title, dot);
    const close = document.createElement('button');
    close.className = 'session-close';
    close.dataset.closeSession = session.session;
    close.title = `关闭会话：${session.title}`;
    close.setAttribute('aria-label', close.title);
    close.innerHTML = icon('close');
    row.append(button, close);
    fragment.append(row);
  }
  list.replaceChildren(fragment);
  $('new-chat').disabled = sessionMutation;
  $('agent-picker').disabled = sessionMutation;
  $('close-session').disabled = !selectedSession || sessionMutation;
}

function resetView(name) {
  if (selectedSession) {
    sessionViews.set(selectedSession, { draft: queued?.text || $('prompt').value, history, pending: queued ? null : pending, nativeBusy, nativeSessionId });
  }
  sessionEpoch++;
  shouldReconnect = false;
  clearTimeout(reconnectTimer);
  const previous = socket;
  socket = null;
  previous?.close(1000, 'Switch session');
  terminal.reset();
  selectedSession = name;
  const view = sessionViews.get(name);
  history = view?.history || [];
  pending = view?.pending || null;
  queued = null;
  nativeBusy = view?.nativeBusy || false;
  nativeSessionId = view?.nativeSessionId || null;
  nodes.clear();
  $('prompt').value = view?.draft || '';
  $('prompt').style.height = '';
  sessionExists = Boolean(name);
  updateSessionDetails();
  setConnection('offline', '未连接');
  renderChat();
  closeTerminal();
  rememberSession();
}

async function selectSession(name, attach = true) {
  if (name === selectedSession) { document.body.classList.remove('sidebar-open'); return; }
  resetView(name);
  document.body.classList.remove('sidebar-open');
  await refreshSession(true);
  if (attach && sessions.find((session) => session.session === selectedSession)?.alive) connect();
}

async function refreshSession(quiet = false) {
  if (sessionsRequest || sessionMutation) return;
  sessionsRequest = true;
  const requestEpoch = sessionEpoch;
  const requestVersion = sessionsVersion;
  try {
    const response = await fetch('/api/sessions');
    const data = await response.json();
    if (!response.ok) throw new Error(data.error);
    if (requestEpoch !== sessionEpoch || requestVersion !== sessionsVersion) return;
    sessions = data.sessions || [];
    agents = data.agents || agents;
    const previousAgent = $('agent-picker').value;
    $('agent-picker').replaceChildren(...agents.map(agent => {
      const option = document.createElement('option'); option.value = agent.id; option.textContent = agent.label + (agent.chatSupported ? '' : ' · 终端'); return option;
    }));
    $('agent-picker').value = agentPickerInitialized && agents.some(agent => agent.id === previousAgent) ? previousAgent : data.defaultAgent || agents[0]?.id;
    agentPickerInitialized = true;
    workspacePath = data.workdir;
    if (!bootstrapped) {
      bootstrapped = true;
      let remembered;
      try { remembered = localStorage.getItem(storageKey()); } catch {}
      resetView(sessions.find((session) => session.session === remembered)?.session || sessions[0]?.session || null);
    } else if (selectedSession && !sessions.some((session) => session.session === selectedSession)) {
      const closed = selectedSession;
      resetView(sessions[0]?.session || null);
      sessionViews.delete(closed);
    }
    const state = sessions.find((session) => session.session === selectedSession);
    sessionExists = Boolean(state);
    const name = data.workdir.split('/').filter(Boolean).at(-1) || data.workdir;
    for (const id of ['workspace-name', 'header-project', 'welcome-project', 'composer-project']) $(id).textContent = name;
    $('workdir').textContent = data.workdir;
    updateSessionDetails();
    if (!connected) $('connect').textContent = state ? '连接终端' : '启动并连接';
    renderSessions();
    if (state) refreshChat();
  } catch (error) { if (!quiet) notice(`读取会话失败：${error.message}`, true); }
  finally { sessionsRequest = false; }
}

async function createSession() {
  if (sessionMutation) return null;
  sessionMutation = true;
  sessionsVersion++;
  renderSessions(); updateComposer();
  try {
    const response = await fetch('/api/sessions', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ agent: $('agent-picker').value, cols: Math.max(20, Math.min(400, terminal.cols)), rows: Math.max(5, Math.min(150, terminal.rows)) }) });
    const state = await response.json();
    if (!response.ok) throw new Error(state.error);
    workspacePath = state.workdir;
    bootstrapped = true;
    sessions.unshift(state);
    resetView(state.session);
    document.body.classList.remove('sidebar-open');
    return state.session;
  } catch (error) { notice(`新建会话失败：${error.message}`, true); return null; }
  finally { sessionMutation = false; renderSessions(); updateSessionDetails(); updateComposer(); }
}

function send(message) {
  if (socket?.readyState !== WebSocket.OPEN) return false;
  socket.send(JSON.stringify(message));
  return true;
}

function resizeTerminal() {
  fit.fit();
  const cols = Math.max(20, Math.min(400, terminal.cols));
  const rows = Math.max(5, Math.min(150, terminal.rows));
  const size = `${cols} × ${rows}`;
  $('terminal-size').textContent = size;
  if (size !== lastSize && connected) send({ type: 'resize', cols, rows });
  lastSize = size;
}

async function transmitQueued() {
  if (!queued || !connected || flushing) return;
  flushing = true;
  const message = queued;
  const epoch = sessionEpoch;
  try {
    // Attaching to tmux can precede CLI startup. Wait for the idle composer.
    const response = await fetch(sessionUrl('/api/screen'));
    const data = await response.json();
    const screen = data.text || '';
    const lines = screen.split('\n');
    let promptLine = -1;
    for (let index = lines.length - 1; index >= 0; index--) {
      if (/^\s*[›❯](?:\s|$)/.test(lines[index])) { promptLine = index; break; }
    }
    const isClaude = currentAgent().id === 'claude';
    const idleComposer = promptLine >= 0 && (isClaude
      ? /^\s*❯(?:\s|$)/.test(lines[promptLine]) && !/^\s*❯\s*\d+[.)]/.test(lines[promptLine]) && /for shortcuts|shift\+tab|ctrl\+g/i.test(screen)
      : /Ask Codex to do anything/.test(lines[promptLine]));
    const startupWaiting = lines.slice(Math.max(0, promptLine)).join('\n').includes('Waiting for startup');
    const welcomeReady = isClaude || history.length > 0 || /To get started, describe a task|\/init - create an AGENTS\.md/.test(screen);
    if (!response.ok || !(idleComposer && welcomeReady && !startupWaiting)) return;
    if (!connected || queued !== message || epoch !== sessionEpoch) return;
    if (!send({ type: 'paste', text: message.text, submit: true })) return;
    queued = null;
    if (pending) pending.state = 'sent';
    renderChat();
  } catch { /* Retry after the CLI finishes initializing. */ }
  finally { flushing = false; }
}

async function connect() {
  if (!selectedSession && !(await createSession())) return;
  if (!sessions.find((session) => session.session === selectedSession)?.alive) { notice('该会话的 agent 已退出，请关闭它或新建会话。', true); return; }
  clearTimeout(reconnectTimer);
  if (socket && [WebSocket.CONNECTING, WebSocket.OPEN].includes(socket.readyState)) return;
  shouldReconnect = true;
  resizeTerminal();
  setConnection('connecting', '连接中');
  const current = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/terminal`);
  socket = current;
  current.onopen = () => {
    if (socket !== current) return;
    terminal.reset();
    send({ type: 'init', session: selectedSession, cols: Math.max(20, terminal.cols), rows: Math.max(5, Math.min(150, terminal.rows)) });
  };
  current.onmessage = (event) => {
    if (socket !== current) return;
    const message = JSON.parse(event.data);
    if (message.type === 'output') terminal.write(message.data);
    else if (message.type === 'ready') {
      reconnectAttempt = 0;
      sessionExists = true;
      $('notice').hidden = true;
      setConnection('online', '已连接');
      resizeTerminal();
      transmitQueued();
      refreshSession();
      refreshChat();
      if (!currentAgent().chatSupported) openTerminal();
      else if (!$('terminal-drawer').classList.contains('is-open')) $('prompt').focus();
    } else if (['taken-over', 'session-ended', 'detached'].includes(message.type)) {
      shouldReconnect = false;
      notice(message.type === 'taken-over' ? '另一网页已接管会话，点击连接可重新接入。' : message.type === 'session-ended' ? '会话已结束。' : '终端已断开，点击连接可重新接入。');
    } else if (message.type === 'error') {
      if (!connected) shouldReconnect = false;
      if (pending) pending.state = 'failed';
      if (queued) { $('prompt').value = queued.text; queued = null; }
      renderChat();
      notice(message.message, true);
    }
  };
  current.onclose = () => {
    if (socket !== current) return;
    socket = null;
    setConnection('offline', '未连接');
    refreshSession(true);
    if (shouldReconnect && reconnectAttempt < 5) {
      reconnectTimer = setTimeout(connect, Math.min(1000 * 2 ** reconnectAttempt++, 8000));
    } else if (shouldReconnect) {
      shouldReconnect = false;
      if (pending && pending.state === 'queued') { pending.state = 'failed'; $('prompt').value = queued?.text || pending.text; queued = null; renderChat(); }
      notice('暂时无法连接，请确认后端正在运行，然后重试。', true);
    }
  };
  current.onerror = () => {};
}

async function sendPrompt() {
  const text = $('prompt').value.trim();
  if (!text || busy() || sessionMutation) return;
  if (!selectedSession && !(await createSession())) return;
  if (!sessions.find((session) => session.session === selectedSession)?.alive) { notice('该会话的 agent 已退出，请关闭它或新建会话。', true); return; }
  queued = { text };
  if (text.startsWith('/')) {
    openTerminal();
  } else {
    pending = { id: `pending-${Date.now()}`, role: 'user', phase: 'plain', text, state: 'queued', knownIds: new Set(history.map((message) => message.id)) };
  }
  $('prompt').value = '';
  $('prompt').style.height = '';
  renderChat();
  $('chat-scroll').scrollTop = $('chat-scroll').scrollHeight;
  if (connected) transmitQueued(); else connect();
}

function openTerminal() {
  $('terminal-drawer').classList.add('is-open');
  $('terminal-drawer').setAttribute('aria-hidden', 'false');
  $('drawer-backdrop').hidden = false;
  requestAnimationFrame(() => { resizeTerminal(); if (connected) terminal.focus(); });
}
function closeTerminal() {
  $('terminal-drawer').classList.remove('is-open');
  $('terminal-drawer').setAttribute('aria-hidden', 'true');
  $('drawer-backdrop').hidden = true;
  $('prompt').focus();
}

terminal.onData((data) => { if (connected) send({ type: 'input', data }); });
terminal.attachCustomKeyEventHandler((event) => {
  if (event.type === 'keydown' && (event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'c' && terminal.hasSelection()) { copy(terminal.getSelection()); return false; }
  return true;
});
new ResizeObserver(resizeTerminal).observe($('terminal'));
$('connect').addEventListener('click', connect);
$('agent-picker').addEventListener('change', () => { if (!selectedSession) updateComposer(); });
$('send-prompt').addEventListener('click', sendPrompt);
$('prompt').addEventListener('input', () => { $('prompt').style.height = 'auto'; $('prompt').style.height = `${Math.min($('prompt').scrollHeight, 180)}px`; updateComposer(); });
$('prompt').addEventListener('keydown', (event) => {
  if (event.key === 'Enter' && (!event.shiftKey || event.metaKey || event.ctrlKey) && !event.isComposing) { event.preventDefault(); sendPrompt(); }
});
$('stop-generation').addEventListener('click', () => {
  if (queued) { $('prompt').value = queued.text; queued = null; pending = null; renderChat(); notice('已取消等待发送。'); }
  else { send({ type: 'input', data: currentAgent().id === 'claude' ? '\x1b' : '\x03' }); notice('已请求停止当前任务。'); }
});
$('disconnect').addEventListener('click', () => { shouldReconnect = false; clearTimeout(reconnectTimer); socket?.close(1000, 'User disconnected'); notice('已断开连接，当前会话继续保留。'); });
for (const id of ['open-terminal', 'terminal-toggle']) $(id).addEventListener('click', openTerminal);
for (const id of ['close-terminal', 'drawer-backdrop']) $(id).addEventListener('click', closeTerminal);
const keys = { escape: '\x1b', up: '\x1b[A', down: '\x1b[B', tab: '\t', enter: '\r', interrupt: '\x03' };
document.querySelectorAll('[data-key]').forEach((button) => button.addEventListener('click', () => { if (connected) send({ type: 'input', data: keys[button.dataset.key] }); terminal.focus(); }));
document.querySelectorAll('[data-suggestion]').forEach((button) => button.addEventListener('click', () => { $('prompt').value = button.dataset.suggestion; $('prompt').dispatchEvent(new Event('input')); $('prompt').focus(); }));
$('workspace-details').addEventListener('click', () => { $('workspace-popover').hidden = !$('workspace-popover').hidden; });
$('copy-workdir').addEventListener('click', () => copy(workspacePath, '工作目录已复制'));
$('copy-attach').addEventListener('click', () => { if (attachCommand) copy(attachCommand, '接管命令已复制'); });
$('copy-screen').addEventListener('click', async () => {
  try { const response = await fetch(sessionUrl('/api/screen')); const data = await response.json(); if (!response.ok) throw new Error(data.error); await copy(data.text, '终端屏幕已复制'); }
  catch (error) { notice(error.message, true); }
});
function toggleSidebar() {
  if (window.innerWidth < 700) document.body.classList.toggle('sidebar-open');
  else document.body.classList.toggle('sidebar-collapsed');
}
$('sidebar-toggle').addEventListener('click', toggleSidebar);
$('sidebar-close').addEventListener('click', () => { document.body.classList.remove('sidebar-open'); if (window.innerWidth >= 700) document.body.classList.add('sidebar-collapsed'); });
$('session-list').addEventListener('click', (event) => {
  const close = event.target.closest('[data-close-session]');
  if (close) { closeSession(close.dataset.closeSession); return; }
  const select = event.target.closest('[data-select-session]');
  if (select && !sessionMutation) selectSession(select.dataset.selectSession);
});

async function closeSession(name = selectedSession) {
  if (!name || sessionMutation) return false;
  const title = sessions.find((session) => session.session === name)?.title || '当前会话';
  if (!confirm(`关闭「${title}」？该会话中的 agent 和正在运行的任务会停止，其他会话会继续运行。`)) return false;
  sessionMutation = true;
  sessionsVersion++;
  renderSessions(); updateComposer();
  try {
    const response = await fetch(sessionUrl('/api/session', name), { method: 'DELETE' });
    const data = await response.json();
    if (!response.ok) throw new Error(data.error);
    sessions = sessions.filter((session) => session.session !== name);
    sessionViews.delete(name);
    if (selectedSession === name) {
      resetView(sessions[0]?.session || null);
      sessionViews.delete(name);
      if (selectedSession && sessions.find((session) => session.session === selectedSession)?.alive) connect();
    }
    await refreshSession(true);
    notice('会话已关闭。');
    return true;
  } catch (error) { notice(error.message, true); return false; }
  finally { sessionMutation = false; renderSessions(); updateSessionDetails(); updateComposer(); }
}
for (const id of ['end-session', 'close-session']) $(id).addEventListener('click', () => closeSession());
$('new-chat').addEventListener('click', async () => { if (await createSession()) { connect(); $('prompt').focus(); } });
window.addEventListener('keydown', (event) => { if (event.key === 'Escape') { closeTerminal(); document.body.classList.remove('sidebar-open'); } });
window.addEventListener('pagehide', () => { shouldReconnect = false; socket?.close(); });
refreshSession();
setInterval(refreshChat, 900);
setInterval(transmitQueued, 500);
setInterval(() => refreshSession(true), 5000);
