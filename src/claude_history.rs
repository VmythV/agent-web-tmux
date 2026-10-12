use crate::history::{ChatMessage, ChatSnapshot, activity_message};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

pub fn default_home() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude")
        })
}

pub fn discover(home: &Path, workdir: &Path, session: &str) -> Result<Option<PathBuf>> {
    let Some(suffix) = session.rsplit('-').next() else {
        return Ok(None);
    };
    let Ok(id) = uuid::Uuid::parse_str(suffix) else {
        return Ok(None);
    };
    let root = home.join("projects");
    if !root.exists() {
        return Ok(None);
    }
    let mut dirs = vec![root];
    let filename = format!("{id}.jsonl");
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                dirs.push(entry.path());
            } else if entry.file_type()?.is_file()
                && entry.file_name().to_string_lossy() == filename
                && belongs(&entry.path(), workdir, &id.to_string())
            {
                return Ok(Some(entry.path()));
            }
        }
    }
    Ok(None)
}

pub fn belongs(path: &Path, workdir: &Path, id: &str) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    BufReader::new(file)
        .lines()
        .take(100)
        .filter_map(|line| serde_json::from_str::<Value>(&line.ok()?).ok())
        .any(|row| {
            row["sessionId"].as_str() == Some(id)
                && row["cwd"].as_str() == workdir.to_str()
                && row["isSidechain"] != true
        })
}

pub fn read(path: &Path) -> Result<ChatSnapshot> {
    let mut snapshot = ChatSnapshot {
        source: "claude".into(),
        ..Default::default()
    };
    let mut seen = HashSet::new();
    let mut tools: HashMap<String, usize> = HashMap::new();
    for (index, line) in BufReader::new(fs::File::open(path)?).lines().enumerate() {
        let Ok(row) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        if row["isSidechain"] == true || row["isMeta"] == true || row["isCompactSummary"] == true {
            continue;
        }
        if let Some(id) = row["sessionId"].as_str() {
            snapshot.session_id = Some(id.into());
        }
        let role = match row["type"].as_str() {
            Some("user") => "user",
            Some("assistant") => "assistant",
            Some("system") if row["subtype"] == "turn_duration" => {
                snapshot.busy = false;
                continue;
            }
            _ => continue,
        };
        let id = row["uuid"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("row-{index}"));
        if !seen.insert(id.clone()) {
            continue;
        }
        let message = &row["message"];
        if role == "assistant" {
            snapshot.busy =
                message["stop_reason"] != "end_turn" && row["isApiErrorMessage"] != true;
        }
        let content = &message["content"];
        let parts = if let Some(text) = content.as_str() {
            vec![json!({"type":"text","text":text})]
        } else {
            content.as_array().cloned().unwrap_or_default()
        };
        for (part_index, part) in parts.iter().enumerate() {
            match part["type"].as_str() {
                Some("text") => {
                    let Some(text) = part["text"].as_str().filter(|text| !text.trim().is_empty())
                    else {
                        continue;
                    };
                    if role == "user" {
                        snapshot.busy = true;
                    }
                    snapshot.messages.push(ChatMessage {
                        id: format!("{id}-{part_index}"),
                        role: role.into(),
                        text: text.into(),
                        phase: if role == "assistant" && message["stop_reason"] != "end_turn" {
                            "commentary"
                        } else {
                            "final_answer"
                        }
                        .into(),
                        activity: None,
                    });
                }
                Some("tool_use") if role == "assistant" => {
                    let call = part["id"].as_str().unwrap_or(&id);
                    if tools.contains_key(call) {
                        continue;
                    }
                    let name = part["name"].as_str().unwrap_or("tool");
                    let input = &part["input"];
                    let item = match name {
                        "Read" => {
                            json!({"type":"CommandExecution","parsed_cmd":[{"type":"read","name":input["file_path"],"path":input["file_path"]}]})
                        }
                        "Glob" | "Grep" => {
                            json!({"type":"CommandExecution","parsed_cmd":[{"type":"search","query":input["pattern"],"path":input["path"]}]})
                        }
                        "Bash" => json!({"type":"CommandExecution","command":input["command"]}),
                        "WebSearch" => {
                            json!({"type":"Extension","kind":"web.search","action":{"type":"search","query":input["query"]}})
                        }
                        "WebFetch" => {
                            json!({"type":"Extension","kind":"web.search","action":{"type":"openPage","url":input["url"]}})
                        }
                        "Edit" | "Write" | "MultiEdit" => {
                            let mut changes = serde_json::Map::new();
                            if let Some(path) = input["file_path"].as_str() {
                                changes.insert(path.into(), json!({"type":"update"}));
                            }
                            json!({"type":"FileChange","changes":changes})
                        }
                        _ => json!({"type":"McpToolCall","tool":name}),
                    };
                    if let Some(activity) = activity_message(&item, call, "running") {
                        tools.insert(call.into(), snapshot.messages.len());
                        snapshot.messages.push(activity);
                    }
                }
                Some("tool_result") => {
                    if let Some(position) =
                        part["tool_use_id"].as_str().and_then(|id| tools.get(id))
                        && let Some(activity) = &mut snapshot.messages[*position].activity
                    {
                        activity.status = if part["is_error"] == true {
                            "failed"
                        } else {
                            "completed"
                        }
                        .into();
                    }
                }
                _ => {} // Thinking blocks, tool output and injected metadata stay out of the UI.
            }
        }
    }
    if snapshot.messages.len() > 200 {
        snapshot.messages.drain(..snapshot.messages.len() - 200);
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_claude_messages_and_tools_without_thinking_or_tool_results() {
        let root =
            std::env::temp_dir().join(format!("agent-web-claude-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("projects/project")).unwrap();
        let id = uuid::Uuid::new_v4();
        let path = root.join(format!("projects/project/{id}.jsonl"));
        let rows = vec![
            json!({"type":"user","uuid":"u1","sessionId":id.to_string(),"cwd":"/project","message":{"content":"你好"}}),
            json!({"type":"user","uuid":"meta","isMeta":true,"message":{"content":"PRIVATE_CONTEXT"}}),
            json!({"type":"assistant","uuid":"a1","message":{"stop_reason":"tool_use","content":[{"type":"thinking","thinking":"PRIVATE_THINKING"},{"type":"text","text":"先读文件。"},{"type":"tool_use","id":"read","name":"Read","input":{"file_path":"/project/README.md"}}]}}),
            json!({"type":"user","uuid":"result","message":{"content":[{"type":"tool_result","tool_use_id":"read","content":"PRIVATE_TOOL_RESULT"}]}}),
            json!({"type":"assistant","uuid":"a2","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"**最终回复**"}]}}),
        ];
        fs::write(
            &path,
            rows.iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let name = format!("agent-web-{}", id.simple());
        assert_eq!(
            discover(&root, Path::new("/project"), &name).unwrap(),
            Some(path.clone())
        );
        assert!(
            discover(&root, Path::new("/other"), &name)
                .unwrap()
                .is_none()
        );
        let chat = read(&path).unwrap();
        assert_eq!(chat.messages.len(), 4);
        assert!(!chat.busy);
        assert_eq!(
            chat.messages[2].activity.as_ref().unwrap().status,
            "completed"
        );
        assert!(!serde_json::to_string(&chat).unwrap().contains("PRIVATE_"));
        fs::remove_dir_all(root).unwrap();
    }
}
