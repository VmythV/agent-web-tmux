use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, time::timeout};

#[derive(Clone)]
pub struct TmuxSession {
    pub workdir: PathBuf,
    pub command: Vec<String>,
    pub socket: String,
    pub name: String,
    pub tmux_bin: String,
    pub agent: String,
}

#[derive(Debug, Serialize)]
pub struct SessionStatus {
    pub exists: bool,
    pub alive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    pub workdir: String,
    pub session: String,
    pub socket: String,
    pub title: String,
    pub agent: String,
}

pub fn terminal_size(cols: u16, rows: u16) -> Result<()> {
    if !(20..=400).contains(&cols) || !(5..=150).contains(&rows) {
        bail!("终端尺寸无效");
    }
    Ok(())
}

impl TmuxSession {
    pub fn new(workdir: PathBuf, command: Vec<String>) -> Self {
        let digest = format!("{:x}", Sha256::digest(workdir.to_string_lossy().as_bytes()));
        Self {
            workdir,
            command,
            socket: format!("codex-web-{}", &digest[..12]),
            name: "codex-web".into(),
            tmux_bin: "tmux".into(),
            agent: "codex".into(),
        }
    }

    async fn execute(&self, args: &[String]) -> Result<std::process::Output> {
        timeout(
            Duration::from_secs(8),
            Command::new(&self.tmux_bin)
                .args(["-L", &self.socket])
                .args(args)
                .env_remove("TMUX")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("tmux 操作超时")?
        .with_context(|| {
            format!(
                "无法启动 {}，请确认 tmux 已安装且位于 PATH 中",
                self.tmux_bin
            )
        })
    }

    async fn run(&self, args: Vec<String>) -> Result<String> {
        let output = self.execute(&args).await?;
        if !output.status.success() {
            bail!("tmux: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub async fn status(&self) -> Result<SessionStatus> {
        let mut status = SessionStatus {
            exists: false,
            alive: false,
            pid: None,
            cols: None,
            rows: None,
            workdir: self.workdir.to_string_lossy().into_owned(),
            session: self.name.clone(),
            socket: self.socket.clone(),
            title: self
                .option("@codex-web-title")
                .await?
                .unwrap_or_else(|| "新会话".into()),
            agent: self
                .option("@agent-web-agent")
                .await?
                .unwrap_or_else(|| self.agent.clone()),
        };
        let args = vec![
            "list-panes".into(),
            "-t".into(),
            self.target(),
            "-F".into(),
            "#{pane_dead}\t#{pane_pid}\t#{pane_width}\t#{pane_height}".into(),
        ];
        let output = self.execute(&args).await?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            if output.status.code() == Some(1)
                && [
                    "no server running",
                    "error connecting",
                    "can't find",
                    "no sessions",
                ]
                .iter()
                .any(|text| error.contains(text))
            {
                return Ok(status);
            }
            bail!("tmux: {}", error.trim());
        }
        let text = String::from_utf8(output.stdout)?;
        let fields: Vec<_> = text.trim().split('\t').collect();
        if fields.len() != 4 {
            bail!("tmux 返回了无法识别的会话状态");
        }
        status.exists = true;
        status.alive = fields[0] == "0";
        status.pid = Some(fields[1].parse()?);
        status.cols = Some(fields[2].parse()?);
        status.rows = Some(fields[3].parse()?);
        Ok(status)
    }

    // Caller holds the application operation lock, serializing start and stop.
    pub async fn ensure(&self, cols: u16, rows: u16) -> Result<SessionStatus> {
        terminal_size(cols, rows)?;
        let previous = self.status().await?;
        if previous.exists {
            if !previous.alive {
                bail!("Agent 已退出，请先结束旧会话，再启动新会话。");
            }
            return Ok(previous);
        }
        if self.command.len() < 2 {
            bail!("启动命令必须包含可执行文件和参数");
        }
        let mut args = vec![
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            self.name.clone(),
            "-x".into(),
            cols.to_string(),
            "-y".into(),
            rows.to_string(),
            "-c".into(),
            self.workdir.to_string_lossy().into_owned(),
            "-e".into(),
            format!("CODEX_WEB_TMUX_SESSION={}", self.name),
        ];
        // Preserve argv boundaries, including the configured login shell's script and arguments.
        args.extend(self.command.clone());
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .to_string();
        args.extend([
            ";".into(),
            "set-option".into(),
            "-t".into(),
            self.name.clone(),
            "@agent-web-agent".into(),
            self.agent.clone(),
            ";".into(),
            "set-option".into(),
            "-t".into(),
            self.name.clone(),
            "@codex-web-started".into(),
            started,
        ]);
        for (verb, option, value) in [
            ("set-option", "status", "off"),
            ("set-option", "history-limit", "20000"),
            ("set-window-option", "remain-on-exit", "on"),
            ("set-window-option", "window-size", "latest"),
        ] {
            args.extend([
                ";".into(),
                verb.into(),
                "-t".into(),
                if verb == "set-option" {
                    self.name.clone()
                } else {
                    self.target()
                },
                option.into(),
                value.into(),
            ]);
        }
        self.run(args).await?;
        self.status().await
    }

    pub async fn capture(&self) -> Result<String> {
        if !self.status().await?.exists {
            bail!("尚未创建会话");
        }
        self.run(vec![
            "capture-pane".into(),
            "-p".into(),
            "-J".into(),
            "-S".into(),
            "-2000".into(),
            "-t".into(),
            format!("{}:0.0", self.target()),
        ])
        .await
    }

    pub async fn started_millis(&self) -> Result<Option<i64>> {
        Ok(self
            .option("@codex-web-started")
            .await?
            .and_then(|text| text.parse().ok()))
    }

    fn target(&self) -> String {
        format!("={}", self.name)
    }

    pub async fn option(&self, option: &str) -> Result<Option<String>> {
        let output = self
            .execute(&[
                "show-options".into(),
                "-v".into(),
                "-t".into(),
                self.name.clone(),
                option.into(),
            ])
            .await?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8(output.stdout)?.trim().into()))
    }

    pub async fn set_option(&self, option: &str, value: &str) -> Result<()> {
        self.run(vec![
            "set-option".into(),
            "-t".into(),
            self.name.clone(),
            option.into(),
            value.into(),
        ])
        .await?;
        Ok(())
    }

    pub async fn names(&self) -> Result<Vec<String>> {
        let output = self
            .execute(&[
                "list-sessions".into(),
                "-F".into(),
                "#{session_name}".into(),
            ])
            .await?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            if error.contains("no server running")
                || error.contains("error connecting")
                || error.contains("no sessions")
            {
                return Ok(Vec::new());
            }
            bail!("tmux: {}", error.trim());
        }
        Ok(String::from_utf8(output.stdout)?
            .lines()
            .map(str::to_owned)
            .collect())
    }

    pub async fn stop(&self) -> Result<()> {
        if self.status().await?.exists {
            self.run(vec!["kill-session".into(), "-t".into(), self.target()])
                .await?;
        }
        Ok(())
    }

    pub fn attach_args(&self) -> Vec<String> {
        vec![
            "-L".into(),
            self.socket.clone(),
            "attach-session".into(),
            "-t".into(),
            self.target(),
        ]
    }
}
