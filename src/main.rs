use agent_web_tmux::{AppState, agent::Agent, launcher::default_shell, router, tmux::TmuxSession};
use anyhow::{Context, Result};
use std::{env, path::PathBuf, time::Duration};
use tokio::{net::TcpListener, process::Command, time::timeout};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("启动失败：{error:#}");
        std::process::exit(1);
    }
}

async fn verify(program: &str, version_arg: &str) -> Result<()> {
    let result = timeout(
        Duration::from_secs(5),
        Command::new(program)
            .arg(version_arg)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("启动检查超时")?
    .with_context(|| format!("找不到可执行文件 {program}；请安装它或指定完整路径。"))?;
    anyhow::ensure!(
        result.status.success(),
        "{program} 版本检查失败：{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}

async fn run() -> Result<()> {
    let port = env::var("PORT")
        .unwrap_or_else(|_| "3001".into())
        .parse::<u16>()
        .context("PORT 必须是有效端口")?;
    let workdir = match env::var_os("AGENT_WORKDIR").or_else(|| env::var_os("CODEX_WORKDIR")) {
        Some(path) => PathBuf::from(path),
        None => env::current_dir()?,
    }
    .canonicalize()
    .context("CODEX_WORKDIR 不存在或无法访问")?;
    anyhow::ensure!(workdir.is_dir(), "CODEX_WORKDIR 必须是目录");
    let agents = Agent::configured(&workdir)?;
    let requested = env::var("AGENT").unwrap_or_else(|_| "auto".into());
    let (agent_id, selected) = if requested == "auto" {
        match Agent::check("claude", &workdir).await {
            Ok(program) => ("claude".to_string(), program),
            Err(_) => ("codex".to_string(), Agent::check("codex", &workdir).await?),
        }
    } else {
        (requested.clone(), Agent::check(&requested, &workdir).await?)
    };
    let tmux_bin = env::var("TMUX_BIN").unwrap_or_else(|_| "tmux".into());
    verify(&tmux_bin, "-V").await?;
    let agent = agents
        .iter()
        .find(|agent| agent.id == agent_id)
        .context("未配置该 agent")?;
    let mut session = TmuxSession::new(workdir.clone(), agent.command.clone());
    // Keep the original socket and legacy default session name for upgrades.
    session.tmux_bin = tmux_bin;
    let state = AppState::new(session)
        .with_agents(agents, agent_id.clone())
        .verify_launchers();
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .context("无法监听端口")?;
    println!("Agent Web: http://{}", listener.local_addr()?);
    println!("工作目录: {}", workdir.display());
    println!(
        "默认 shell: {}，启动命令: {selected}（{agent_id}）",
        default_shell().display()
    );
    println!(
        "tmux socket: {}（每个会话的接管命令可在网页终端面板中复制）",
        state.session().socket
    );
    let shutdown_state = state.clone();
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown_state.shutdown();
            println!("\n后端已停止，tmux 中的 agent 会话继续保留。");
        })
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
