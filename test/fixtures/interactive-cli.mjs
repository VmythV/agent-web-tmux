// Deterministic CLI + Codex-shaped transcript for browser tests; no model calls.
import { openSync, writeSync, mkdirSync } from 'node:fs';
import path from 'node:path';
import { randomUUID } from 'node:crypto';
const historyArg = process.argv.indexOf('--history-dir');
const historyHome = historyArg >= 0 ? process.argv[historyArg + 1] : null;
let rollout;
const isClaude = process.argv.includes('--agent-kind') && process.argv[process.argv.indexOf('--agent-kind') + 1] === 'claude';
const claudeSession = process.argv[process.argv.indexOf('--session-id') + 1] || randomUUID();
let turn = 0;
if (historyHome) {
  const directory = path.join(historyHome, isClaude ? 'projects/fixture-project' : 'sessions');
  mkdirSync(directory, { recursive: true });
  rollout = openSync(path.join(directory, isClaude ? `${claudeSession}.jsonl` : `rollout-${randomUUID()}.jsonl`), 'w');
  if (isClaude) writeSync(rollout, JSON.stringify({type:'system',isMeta:true,sessionId:claudeSession,cwd:process.cwd()})+'\n');
  else writeSync(rollout, JSON.stringify({ type: 'session_meta', payload: { id: randomUUID(), timestamp: new Date().toISOString(), cwd: process.cwd(), source: 'cli', thread_source: 'user' } }) + '\n');
}
function record(payload) {
  if (rollout !== undefined) writeSync(rollout, JSON.stringify({ timestamp: new Date().toISOString(), type: 'event_msg', payload }) + '\n');
}
function claudeRecord(row) {
  writeSync(rollout, JSON.stringify({ uuid:randomUUID(), sessionId:claudeSession, cwd:process.cwd(), ...row })+'\n');
}
function reply(text) {
  if (isClaude) {
    claudeRecord({type:'user',message:{content:text}});
    claudeRecord({type:'assistant',message:{stop_reason:'tool_use',content:[{type:'thinking',thinking:'PRIVATE_REASONING_MUST_NOT_RENDER'},{type:'text',text:'先读取项目文件。'},{type:'tool_use',id:'read',name:'Read',input:{file_path:'/project/README.md'}}]}});
    setTimeout(()=>{
      claudeRecord({type:'user',message:{content:[{type:'tool_result',tool_use_id:'read',content:'PRIVATE_TOOL_RESULT_MUST_NOT_RENDER'}]}});
      claudeRecord({type:'assistant',message:{stop_reason:'end_turn',content:[{type:'text',text:'Claude Code 已收到你的消息。'}]}});
    },1200);
    return;
  }
  const id = ++turn;
  record({ type: 'task_started', turn_id: `turn-${id}` });
  record({ type: 'item_completed', item: { type: 'UserMessage', id: `user-${id}`, content: [{ type: 'text', text }] } });
  record({ type: 'item_completed', item: { type: 'Reasoning', id: `private-${id}`, raw_content: 'PRIVATE_REASONING_MUST_NOT_RENDER' } });
  record({ type: 'item_completed', item: { type: 'AgentMessage', id: `commentary-${id}`, phase: 'commentary', content: [{ type: 'Text', text: '我先读取项目说明，再核对相关文档。' }] } });
  record({ type: 'item_completed', item: { type: 'CommandExecution', id: `read-${id}`, status: 'completed', command: ['/bin/zsh', '-lc', 'cat /skills/openai-docs/SKILL.md'], parsed_cmd: [{ type: 'read', name: 'SKILL.md', path: '/skills/openai-docs/SKILL.md' }], exit_code: 0, aggregated_output: 'PRIVATE_TOOL_RESULT_MUST_NOT_RENDER' } });
  record({ type: 'item_started', item: { type: 'Extension', id: `search-${id}`, kind: 'web.search', action: { type: 'search', query: 'Codex capabilities coding tasks' } } });
  setTimeout(() => {
    record({ type: 'item_completed', item: { type: 'Extension', id: `search-${id}`, kind: 'web.search', action: { type: 'search', query: 'Codex capabilities coding tasks' }, results: [{ snippet: 'PRIVATE_TOOL_RESULT_MUST_NOT_RENDER' }] } });
    record({ type: 'item_completed', item: { type: 'Extension', id: `open-${id}`, kind: 'web.search', action: { type: 'openPage', url: 'https://developers.openai.com/api/docs/guides/code-generation' } } });
    const answer = '已经收到你的消息。\n\n下面是一个 Rust 示例：\n\n```rust\nfn main() {\n    println!("Hello, Codex!");\n}\n```\n\n- 通过 tmux 保留会话\n- 在聊天中阅读回复\n\n现在可以继续描述你的需求。';
    record({ type: 'item_completed', item: { type: 'AgentMessage', id: `assistant-${id}`, phase: 'final_answer', content: [{ type: 'Text', text: answer }] } });
    record({ type: 'task_complete', turn_id: `turn-${id}` });
    process.stdout.write(`\r\n• ${answer}\r\n`);
  }, 2500);
}
process.stdin.setRawMode(true);
process.stdin.setEncoding('utf8');
process.stdin.resume();
if (isClaude) process.stdout.write('\x1b[?2004hClaude Code\r\n❯ \r\n? for shortcuts\r\n');
else process.stdout.write('\x1b[?2004hMVP_READY\r\nTo get started, describe a task or try one of these commands:\r\n\r\n› Ask Codex to do anything\r\n');
let buffer = '';
let draft = null;
process.stdin.on('data', (text) => {
  if (/^\x1b\[.*R$/.test(text)) return;
  process.stdout.write(`RECEIVED ${JSON.stringify(text)}\r\n`);
  buffer += text;
  const start = buffer.indexOf('\x1b[200~');
  const end = buffer.indexOf('\x1b[201~');
  if (start >= 0 && end > start) { draft = buffer.slice(start + 6, end); buffer = buffer.slice(end + 6); }
  if (draft !== null && buffer.includes('\r')) { reply(draft); draft = null; buffer = ''; }
  if (buffer.length > 64000) buffer = '';
});
