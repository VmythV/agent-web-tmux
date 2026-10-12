use crate::launcher::{ShellLauncher, default_shell};
use anyhow::{Result, bail};
use serde::Serialize;
use std::{env, path::Path};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub id: String,
    pub label: String,
    pub chat_supported: bool,
    #[serde(skip)]
    pub command: Vec<String>,
}

impl Agent {
    pub fn configured(workdir: &Path) -> Result<Vec<Self>> {
        let shell = default_shell();
        let codex = ShellLauncher::new(shell.clone(), env::var("CODEX_BIN").ok());
        let claude = ShellLauncher::claude(shell.clone(), env::var("CLAUDE_BIN").ok());
        let mut agents = vec![
            Self {
                id: "codex".into(),
                label: "Codex".into(),
                chat_supported: true,
                command: codex.command(&[
                    "--no-alt-screen".into(),
                    "--no-daemon".into(),
                    "--sandbox".into(),
                    "workspace-write".into(),
                    "--ask-for-approval".into(),
                    "on-request".into(),
                    "-C".into(),
                    workdir.to_string_lossy().into_owned(),
                ]),
            },
            Self {
                id: "claude".into(),
                label: "Claude Code".into(),
                chat_supported: true,
                command: claude.command(&[]),
            },
        ];
        if let Ok(program) = env::var("AGENT_BIN") {
            let args: Vec<String> =
                serde_json::from_str(&env::var("AGENT_ARGS").unwrap_or_else(|_| "[]".into()))?;
            agents.push(Self {
                id: "custom".into(),
                label: env::var("AGENT_LABEL").unwrap_or_else(|_| "Custom agent".into()),
                chat_supported: false,
                command: ShellLauncher::new(shell, Some(program)).command(&args),
            });
        }
        Ok(agents)
    }

    pub fn for_session(&self, session_name: &str) -> Vec<String> {
        let mut command = self.command.clone();
        if self.id == "claude" {
            let suffix = session_name.rsplit('-').next().unwrap();
            if let Ok(id) = uuid::Uuid::parse_str(suffix) {
                command.extend(["--session-id".into(), id.to_string()]);
            }
        }
        command
    }

    pub async fn check(id: &str, workdir: &Path) -> Result<String> {
        let shell = default_shell();
        let launcher = match id {
            "codex" => ShellLauncher::new(shell, env::var("CODEX_BIN").ok()),
            "claude" => ShellLauncher::claude(shell, env::var("CLAUDE_BIN").ok()),
            "custom" => ShellLauncher::new(shell, Some(env::var("AGENT_BIN")?)),
            _ => bail!("未知 agent：{id}"),
        };
        launcher.check(workdir).await
    }
}
