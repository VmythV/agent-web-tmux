#[cfg(not(target_os = "linux"))]
use anyhow::Context;
use anyhow::Result;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{process::Command, time::timeout};

/// Bind the rollout actually held open by this pane's Codex process, not another CLI in the same cwd.
pub async fn opened_rollout(pid: u32, home: &Path, workdir: &Path) -> Result<Option<PathBuf>> {
    let processes = timeout(
        Duration::from_secs(3),
        Command::new("ps")
            .args(["-ax", "-o", "pid=,ppid="])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let pairs: Vec<(u32, u32)> = String::from_utf8_lossy(&processes.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
        .collect();
    let mut pids = HashSet::from([pid]);
    loop {
        let children: Vec<_> = pairs
            .iter()
            .filter(|(_, parent)| pids.contains(parent))
            .map(|(pid, _)| *pid)
            .collect();
        let previous = pids.len();
        pids.extend(children);
        if pids.len() == previous {
            break;
        }
    }
    let root = home
        .join("sessions")
        .canonicalize()
        .unwrap_or_else(|_| home.join("sessions"));
    let mut paths = Vec::new();
    #[cfg(target_os = "linux")]
    for pid in &pids {
        if let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
            paths.extend(
                entries
                    .flatten()
                    .filter_map(|entry| std::fs::read_link(entry.path()).ok()),
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let ids = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let output = timeout(
            Duration::from_secs(4),
            Command::new("lsof")
                .args(["-a", "-p", &ids, "-Fn"])
                .kill_on_drop(true)
                .output(),
        )
        .await?
        .context("无法绑定聊天记录：macOS 需要 lsof")?;
        paths.extend(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.strip_prefix('n').map(PathBuf::from)),
        );
    }
    paths.sort();
    paths.dedup();
    for path in paths {
        let path = path.canonicalize().unwrap_or(path);
        if path.starts_with(&root)
            && path.extension().is_some_and(|ext| ext == "jsonl")
            && belongs(&path, workdir)
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

pub fn belongs(path: &Path, workdir: &Path) -> bool {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut line = String::new();
    if std::io::BufReader::new(file).read_line(&mut line).is_err() {
        return false;
    }
    let Ok(row) = serde_json::from_str::<serde_json::Value>(&line) else {
        return false;
    };
    row["type"] == "session_meta"
        && row["payload"]["source"] == "cli"
        && row["payload"]["cwd"].as_str() == workdir.to_str()
}
