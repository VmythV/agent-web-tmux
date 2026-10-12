// Browser-only test harness. It never launches Codex or uses login credentials.
use agent_web_tmux::{AppState, agent::Agent, router, tmux::TmuxSession};
use anyhow::Result;
use std::{env, path::PathBuf};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let history_home = PathBuf::from(env::var("WEB_TEST_HISTORY")?);
    let mut session = TmuxSession::new(
        root.clone(),
        vec![
            env::var("NODE_BIN").unwrap_or_else(|_| "node".into()),
            root.join("test/fixtures/interactive-cli.mjs")
                .to_string_lossy()
                .into_owned(),
            "--history-dir".into(),
            history_home.to_string_lossy().into_owned(),
        ],
    );
    session.socket = env::var("WEB_TEST_SOCKET")?;
    let codex_command = session.command.clone();
    let mut claude_command = codex_command.clone();
    let index = claude_command
        .iter()
        .position(|arg| arg == "--history-dir")
        .unwrap()
        + 1;
    claude_command[index] = history_home.join("claude").to_string_lossy().into_owned();
    claude_command.extend(["--agent-kind".into(), "claude".into()]);
    let agents = vec![
        Agent {
            id: "codex".into(),
            label: "Codex".into(),
            chat_supported: true,
            command: codex_command,
        },
        Agent {
            id: "claude".into(),
            label: "Claude Code".into(),
            chat_supported: true,
            command: claude_command,
        },
        Agent {
            id: "custom".into(),
            label: "Custom agent".into(),
            chat_supported: false,
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf 'CUSTOM_AGENT_READY\\n'; cat".into(),
            ],
        },
    ];
    let state = AppState::with_history(session, history_home).with_agents(agents, "codex".into());
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    println!("Test server: http://{}", listener.local_addr()?);
    let shutdown = state.clone();
    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                let mut terminate =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .unwrap();
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
            shutdown.shutdown();
        })
        .await?;
    state.session().stop().await?;
    Ok(())
}
