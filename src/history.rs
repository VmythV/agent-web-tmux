use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize)]
pub struct ChatMessage {
    pub id: String,
    pub role: String,
    pub text: String,
    pub phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<ChatActivity>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChatActivity {
    pub kind: String,
    pub status: String,
    pub details: Vec<String>,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSnapshot {
    pub session_id: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub busy: bool,
    pub source: String,
}

pub fn default_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".codex")
        })
}

// Read only a CLI rollout created for this workspace after this tmux session started.
// Earlier conversations and subagent records are never exposed by the chat endpoint.
pub fn discover(home: &Path, workdir: &Path, started: i64) -> Result<Option<PathBuf>> {
    let root = home.join("sessions");
    if !root.exists() {
        return Ok(None);
    }
    let mut directories = vec![root];
    let mut best: Option<(i64, PathBuf)> = None;
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                directories.push(entry.path());
                continue;
            }
            if !kind.is_file() || entry.path().extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            // Most historical files can be skipped without parsing their metadata.
            if entry
                .metadata()?
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis()
                < started as u128
            {
                continue;
            }
            let mut reader = BufReader::new(fs::File::open(entry.path())?);
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let Ok(meta) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let payload = &meta["payload"];
            if meta["type"] != "session_meta"
                || payload["source"] != "cli"
                || payload["thread_source"] == "subagent"
                || payload["cwd"].as_str() != workdir.to_str()
            {
                continue;
            }
            let Some(created) = payload["timestamp"]
                .as_str()
                .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
                .map(|stamp| stamp.timestamp_millis())
            else {
                continue;
            };
            if created < started {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|(earliest, _)| created < *earliest)
            {
                best = Some((created, entry.path()));
            }
        }
    }
    Ok(best.map(|(_, path)| path))
}

pub fn read(path: &Path) -> Result<ChatSnapshot> {
    let file = fs::File::open(path).context("无法读取当前 Codex 会话记录")?;
    let mut snapshot = ChatSnapshot {
        source: "codex".into(),
        ..Default::default()
    };
    let mut seen = HashSet::new();
    let mut activities = HashMap::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        // A live append may leave an incomplete last JSON row; try again next poll.
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let payload = &row["payload"];
        if row["type"] == "session_meta" {
            snapshot.session_id = payload["id"].as_str().map(str::to_owned);
            continue;
        }
        if row["type"] != "event_msg" {
            continue;
        }
        match payload["type"].as_str() {
            Some("task_started") => snapshot.busy = true,
            Some("task_complete" | "turn_aborted") => {
                snapshot.busy = false;
                for message in &mut snapshot.messages {
                    if let Some(activity) = &mut message.activity
                        && activity.status == "running"
                    {
                        activity.status = if payload["type"] == "turn_aborted" {
                            "interrupted"
                        } else {
                            "finished"
                        }
                        .into();
                    }
                }
            }
            Some("item_completed" | "item_started" | "item_updated") => {
                let item = &payload["item"];
                let id = item["id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("row-{index}"));
                let default_status = if payload["type"] == "item_completed" {
                    "completed"
                } else {
                    "running"
                };
                if let Some(message) = activity_message(item, &id, default_status) {
                    upsert_activity(&mut snapshot, &mut activities, message);
                    continue;
                }
                if payload["type"] != "item_completed" {
                    continue;
                }
                let role = match item["type"].as_str() {
                    Some("UserMessage") => "user",
                    Some("AgentMessage") => "assistant",
                    _ => continue, // Never render Reasoning or developer context.
                };
                let phase = item["phase"].as_str().unwrap_or("final_answer");
                if phase == "analysis" {
                    continue;
                }
                let text = message_text(&item["content"]);
                if !text.trim().is_empty() && seen.insert(id.clone()) {
                    snapshot.messages.push(ChatMessage {
                        id,
                        role: role.into(),
                        text,
                        phase: phase.into(),
                        activity: None,
                    });
                }
            }
            // Compatibility with older CLI rollouts.
            Some("user_message" | "agent_message") => {
                if payload["phase"] == "analysis" {
                    continue;
                }
                if payload["kind"].as_str().is_some_and(|kind| kind != "plain") {
                    continue;
                }
                if let Some(text) = payload["message"]
                    .as_str()
                    .filter(|text| !text.trim().is_empty())
                {
                    snapshot.messages.push(ChatMessage {
                        id: format!("row-{index}"),
                        role: if payload["type"] == "user_message" {
                            "user"
                        } else {
                            "assistant"
                        }
                        .into(),
                        text: text.into(),
                        phase: payload["phase"].as_str().unwrap_or("final_answer").into(),
                        activity: None,
                    });
                }
            }
            Some(
                "exec_command_begin" | "exec_command_end" | "web_search_begin" | "web_search_end",
            ) => {
                let mut item = payload.clone();
                let command = payload["type"].as_str().unwrap().starts_with("exec_");
                item["type"] = if command {
                    "CommandExecution"
                } else {
                    "Extension"
                }
                .into();
                if !command {
                    item["kind"] = "web.search".into();
                }
                let id = payload["call_id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("row-{index}"));
                let status = if payload["type"].as_str().unwrap().ends_with("_begin") {
                    "running"
                } else {
                    "completed"
                };
                if let Some(message) = activity_message(&item, &id, status) {
                    upsert_activity(&mut snapshot, &mut activities, message);
                }
            }
            _ => {}
        }
    }
    // Bound the UI response, retaining recent conversation instead of model context.
    if snapshot.messages.len() > 200 {
        snapshot.messages.drain(..snapshot.messages.len() - 200);
    }
    Ok(snapshot)
}

fn upsert_activity(
    snapshot: &mut ChatSnapshot,
    positions: &mut HashMap<String, usize>,
    message: ChatMessage,
) {
    if let Some(index) = positions.get(&message.id) {
        snapshot.messages[*index] = message;
    } else {
        positions.insert(message.id.clone(), snapshot.messages.len());
        snapshot.messages.push(message);
    }
}

fn bounded(text: &str) -> String {
    let mut chars = text.chars();
    let mut text: String = chars.by_ref().take(4000).collect();
    if chars.next().is_some() {
        text.push_str("…（已截断）");
    }
    text
}

// Expose CLI-visible activity summaries, never raw tool arguments, results or reasoning.
pub(crate) fn activity_message(
    item: &Value,
    id: &str,
    default_status: &str,
) -> Option<ChatMessage> {
    let mut details = Vec::new();
    let (kind, label) = match item["type"].as_str()? {
        "CommandExecution" => {
            let parsed = item["parsed_cmd"].as_array();
            let known = parsed.filter(|commands| {
                !commands.is_empty()
                    && commands.iter().all(|command| {
                        matches!(
                            command["type"].as_str(),
                            Some("read" | "search" | "list_files")
                        )
                    })
            });
            let (kind, label) = if let Some(commands) = known {
                for command in commands {
                    let verb = match command["type"].as_str() {
                        Some("read") => "读取",
                        Some("search") => "搜索",
                        _ => "列出文件",
                    };
                    let name = command["name"]
                        .as_str()
                        .or_else(|| command["query"].as_str())
                        .or_else(|| command["path"].as_str())
                        .unwrap_or("");
                    details.push(format!("{verb} {name}"));
                    if let Some(path) = command["path"].as_str().filter(|path| *path != name) {
                        details.push(path.into());
                    }
                }
                if commands.iter().all(|command| command["type"] == "read") {
                    ("read", "读取文件")
                } else if commands.iter().all(|command| command["type"] == "search") {
                    ("search", "搜索文件")
                } else {
                    ("files", "探索文件")
                }
            } else {
                let command = if let Some(text) = item["command"].as_str() {
                    text.to_owned()
                } else {
                    let args: Vec<_> = item["command"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    if args.len() == 3 && matches!(args[1], "-c" | "-lc") {
                        args[2].to_owned()
                    } else {
                        args.join(" ")
                    }
                };
                if !command.is_empty() {
                    details.push(command);
                }
                ("command", "执行命令")
            };
            if let Some(code) = item["exit_code"].as_i64().filter(|code| *code != 0) {
                details.push(format!("退出码：{code}"));
            }
            (kind, label)
        }
        "Extension" => {
            let action = &item["action"];
            if item["kind"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("web."))
            {
                match action["type"].as_str() {
                    Some("openPage" | "open_page" | "open") => {
                        if let Some(url) = action["url"].as_str().or_else(|| item["query"].as_str())
                        {
                            details.push(url.into());
                        }
                        ("web", "打开网页")
                    }
                    Some("search") => {
                        if let Some(query) =
                            action["query"].as_str().or_else(|| item["query"].as_str())
                        {
                            details.push(query.into());
                        }
                        if let Some(queries) = action["queries"].as_array() {
                            details.extend(
                                queries.iter().filter_map(Value::as_str).map(str::to_owned),
                            );
                        }
                        ("web", "搜索网页")
                    }
                    _ => {
                        if let Some(query) = item["query"].as_str() {
                            details.push(query.into());
                        }
                        ("web", "浏览网页")
                    }
                }
            } else {
                if let Some(kind) = item["kind"].as_str() {
                    details.push(kind.into());
                }
                ("tool", "调用工具")
            }
        }
        "McpToolCall" | "DynamicToolCall" => {
            if let Some(server) = item["server"].as_str() {
                details.push(server.into());
            }
            if let Some(tool) = item["tool"].as_str().or_else(|| item["name"].as_str()) {
                details.push(tool.into());
            }
            ("tool", "调用工具")
        }
        "FileChange" => {
            if let Some(changes) = item["changes"].as_object() {
                for (path, change) in changes {
                    let verb = match change["type"].as_str() {
                        Some("add") => "新增",
                        Some("delete" | "remove") => "删除",
                        _ => "修改",
                    };
                    details.push(format!("{verb} {path}"));
                }
            }
            ("edit", "修改文件")
        }
        "ImageView" => {
            if let Some(path) = item["path"].as_str() {
                details.push(path.into());
            }
            ("image", "查看图片")
        }
        "ContextCompaction" => ("tool", "整理会话上下文"),
        _ => return None,
    };
    let status = if item["exit_code"].as_i64().is_some_and(|code| code != 0) {
        "failed"
    } else {
        match item["status"].as_str().unwrap_or(default_status) {
            "in_progress" | "running" | "pending" => "running",
            "failed" | "error" | "declined" => "failed",
            "interrupted" | "cancelled" => "interrupted",
            "completed" | "success" => "completed",
            _ => default_status,
        }
    };
    Some(ChatMessage {
        id: format!("activity-{id}"),
        role: "activity".into(),
        text: label.into(),
        phase: "activity".into(),
        activity: Some(ChatActivity {
            kind: kind.into(),
            status: status.into(),
            details: details
                .iter()
                .filter(|text| !text.is_empty())
                .take(24)
                .map(|text| bounded(text))
                .collect(),
        }),
    })
}

fn message_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| {
            if matches!(part["type"].as_str(), Some("text" | "Text" | "output_text")) {
                part["text"].as_str()
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_rows(path: &Path, rows: Vec<Value>) {
        fs::write(
            path,
            rows.iter()
                .map(|row| row.to_string() + "\n")
                .collect::<String>(),
        )
        .unwrap();
    }

    #[test]
    fn reads_visible_messages_without_private_context_or_reasoning() {
        let root = std::env::temp_dir().join(format!("codex-chat-parser-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("rollout.jsonl");
        let assistant = json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"a1","phase":"final_answer","content":[{"type":"Text","text":"**真实回复**\n\n```rust\nfn main() {}\n```"}]}}});
        write_rows(
            &path,
            vec![
                json!({"type":"session_meta","payload":{"id":"session-1"}}),
                json!({"type":"response_item","payload":{"role":"developer","content":[{"type":"input_text","text":"PRIVATE_CONTEXT"}]}}),
                json!({"type":"event_msg","payload":{"type":"task_started"}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","id":"u1","content":[{"type":"text","text":"中文\n第二行"}]}}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Reasoning","id":"secret","raw_content":"PRIVATE_REASONING"}}}),
                assistant.clone(),
                assistant,
                json!({"type":"event_msg","payload":{"type":"agent_message","phase":"analysis","message":"PRIVATE_ANALYSIS"}}),
                json!({"type":"event_msg","payload":{"type":"task_complete"}}),
            ],
        );
        let snapshot = read(&path).unwrap();
        assert_eq!(snapshot.messages.len(), 2);
        assert_eq!(snapshot.messages[0].role, "user");
        assert_eq!(snapshot.messages[0].text, "中文\n第二行");
        assert_eq!(snapshot.messages[1].role, "assistant");
        assert!(snapshot.messages[1].text.contains("```rust"));
        assert!(!snapshot.busy);
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("PRIVATE_")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn activities_follow_message_order_and_update_without_duplicates() {
        let root =
            std::env::temp_dir().join(format!("codex-chat-activities-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("rollout.jsonl");
        write_rows(
            &path,
            vec![
                json!({"type":"event_msg","payload":{"type":"task_started"}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","id":"u1","content":[{"type":"text","text":"介绍项目"}]}}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"a1","phase":"commentary","content":[{"type":"Text","text":"先查看说明。"}]}}}),
                json!({"type":"event_msg","payload":{"type":"item_started","item":{"type":"CommandExecution","id":"read","status":"in_progress","command":["/bin/zsh","-lc","cat /skills/openai-docs/SKILL.md"],"parsed_cmd":[{"type":"read","name":"SKILL.md","path":"/skills/openai-docs/SKILL.md"}]}}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"read","status":"completed","exit_code":0,"parsed_cmd":[{"type":"read","name":"SKILL.md","path":"/skills/openai-docs/SKILL.md"}],"aggregated_output":"PRIVATE_TOOL_RESULT"}}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"a2","phase":"commentary","content":[{"type":"Text","text":"再核对网页。"}]}}}),
                json!({"type":"event_msg","payload":{"type":"item_started","item":{"type":"Extension","id":"search","kind":"web.search","action":{"type":"search","query":"Codex capabilities coding tasks"}}}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Extension","id":"search","kind":"web.search","action":{"type":"search","query":"Codex capabilities coding tasks"},"results":[{"snippet":"PRIVATE_TOOL_RESULT"}]}}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Extension","id":"open","kind":"web.search","action":{"type":"openPage","url":"https://developers.openai.com/api/docs/guides/code-generation"}}}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"a3","content":[{"type":"Text","text":"这是最终答复。"}]}}}),
                json!({"type":"event_msg","payload":{"type":"task_complete"}}),
            ],
        );
        let snapshot = read(&path).unwrap();
        let ids: Vec<_> = snapshot
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "u1",
                "a1",
                "activity-read",
                "a2",
                "activity-search",
                "activity-open",
                "a3"
            ]
        );
        let read = snapshot.messages[2].activity.as_ref().unwrap();
        assert_eq!(read.status, "completed");
        assert_eq!(
            read.details,
            ["读取 SKILL.md", "/skills/openai-docs/SKILL.md"]
        );
        assert_eq!(snapshot.messages[4].text, "搜索网页");
        assert_eq!(
            snapshot.messages[4].activity.as_ref().unwrap().status,
            "completed"
        );
        assert_eq!(snapshot.messages[5].text, "打开网页");
        assert!(
            snapshot.messages[5].activity.as_ref().unwrap().details[0]
                .starts_with("https://developers.openai.com/")
        );
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("PRIVATE_")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_and_failed_tools_are_visible_but_raw_payloads_stay_private() {
        let root =
            std::env::temp_dir().join(format!("codex-chat-tool-status-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("rollout.jsonl");
        write_rows(
            &path,
            vec![
                json!({"type":"event_msg","payload":{"type":"exec_command_begin","call_id":"legacy","command":["/bin/bash","-lc","cargo test"]}}),
                json!({"type":"event_msg","payload":{"type":"exec_command_end","call_id":"legacy","command":["/bin/bash","-lc","cargo test"],"exit_code":1,"stdout":"PRIVATE_TOOL_OUTPUT"}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"FileChange","id":"edit","status":"completed","changes":{"/project/src/main.rs":{"type":"update","content":"PRIVATE_FILE_CONTENT"}}}}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"McpToolCall","id":"mcp","server":"openaiDeveloperDocs","tool":"search_openai_docs","status":"completed","arguments":{"q":"PRIVATE_TOOL_ARGS"},"result":"PRIVATE_TOOL_RESULT"}}}),
                json!({"type":"event_msg","payload":{"type":"web_search_begin","call_id":"web"}}),
                json!({"type":"event_msg","payload":{"type":"web_search_end","call_id":"web","action":{"type":"search","query":"Rust docs"}}}),
                json!({"type":"event_msg","payload":{"type":"item_started","item":{"type":"CommandExecution","id":"cancelled","command":["sleep","60"]}}}),
                json!({"type":"event_msg","payload":{"type":"turn_aborted"}}),
            ],
        );
        let snapshot = read(&path).unwrap();
        assert_eq!(snapshot.messages.len(), 5);
        assert_eq!(
            snapshot.messages[0].activity.as_ref().unwrap().status,
            "failed"
        );
        assert_eq!(
            snapshot.messages[0].activity.as_ref().unwrap().details,
            ["cargo test", "退出码：1"]
        );
        assert_eq!(
            snapshot.messages[1].activity.as_ref().unwrap().details,
            ["修改 /project/src/main.rs"]
        );
        assert_eq!(
            snapshot.messages[2].activity.as_ref().unwrap().details,
            ["openaiDeveloperDocs", "search_openai_docs"]
        );
        assert_eq!(snapshot.messages[3].text, "搜索网页");
        assert_eq!(
            snapshot.messages[4].activity.as_ref().unwrap().status,
            "interrupted"
        );
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("PRIVATE_")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovery_selects_only_current_workspace_cli_session() {
        let root = std::env::temp_dir().join(format!("codex-chat-scope-{}", uuid::Uuid::new_v4()));
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let workdir = PathBuf::from("/project/current");
        for (file, cwd, source, stamp) in [
            (
                "old.jsonl",
                "/project/current",
                json!("cli"),
                started - 10000,
            ),
            ("other.jsonl", "/project/other", json!("cli"), started + 10),
            (
                "subagent.jsonl",
                "/project/current",
                json!({"subagent":{}}),
                started + 10,
            ),
            (
                "current.jsonl",
                "/project/current",
                json!("cli"),
                started + 20,
            ),
        ] {
            write_rows(
                &sessions.join(file),
                vec![json!({"type":"session_meta","payload":{
                    "id":file,"cwd":cwd,"source":source,
                    "timestamp":chrono::DateTime::from_timestamp_millis(stamp).unwrap().to_rfc3339()
                }})],
            );
        }
        assert_eq!(
            discover(&root, &workdir, started).unwrap(),
            Some(sessions.join("current.jsonl"))
        );
        fs::remove_dir_all(root).unwrap();
    }
}
