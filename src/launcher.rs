use anyhow::{Context, Result, bail};
use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{process::Command, time::timeout};

const RESOLVE: &str = r#"if command -v cx >/dev/null 2>&1; then
    printf '\n__AGENT_WEB_LAUNCHER__cx\n'
elif command -v codex >/dev/null 2>&1; then
    printf '\n__AGENT_WEB_LAUNCHER__codex\n'
else
    printf '找不到 cx 或 codex，请检查默认 shell 的配置和 PATH。\n' >&2
    exit 127
fi"#;

const LAUNCH: &str = r#"if command -v cx >/dev/null 2>&1; then
    cx "$@"
elif command -v codex >/dev/null 2>&1; then
    codex "$@"
else
    printf '找不到 cx 或 codex，请检查默认 shell 的配置和 PATH。\n' >&2
    exit 127
fi"#;

const RESOLVE_EXPLICIT: &str = r#"if command -v "$1" >/dev/null 2>&1; then
    printf '\n__AGENT_WEB_LAUNCHER__%s\n' "$1"
else
    printf '找不到指定的启动命令：%s\n' "$1" >&2
    exit 127
fi"#;

fn claude_script(script: &str) -> String {
    script.replace("cx", "cc").replace("codex", "claude")
        .replace("if command -v cc >/dev/null 2>&1; then", "if command -v cc >/dev/null 2>&1 && [ \"$(command -v cc)\" != /usr/bin/cc ] && [ \"$(command -v cc)\" != /bin/cc ]; then")
}

/// Run the user's login shell interactively so its aliases and functions are available.
pub struct ShellLauncher {
    pub shell: PathBuf,
    explicit: Option<String>,
    claude: bool,
}

impl ShellLauncher {
    pub fn new(shell: PathBuf, explicit: Option<String>) -> Self {
        Self {
            shell,
            explicit,
            claude: false,
        }
    }

    pub fn claude(shell: PathBuf, explicit: Option<String>) -> Self {
        Self {
            shell,
            explicit,
            claude: true,
        }
    }

    fn argv(&self, script: &str, args: &[String]) -> Vec<String> {
        let mut command = vec![
            self.shell.to_string_lossy().into_owned(),
            "-l".into(),
            "-i".into(),
            "-c".into(),
            script.into(),
            "agent-web".into(), // $0; user input is passed in argv, never interpolated.
        ];
        if let Some(program) = &self.explicit {
            command.push(program.clone());
        }
        command.extend_from_slice(args);
        command
    }

    fn probe(&self) -> Command {
        let script = if self.claude {
            claude_script(RESOLVE)
        } else {
            RESOLVE.into()
        };
        let command = self.argv(
            if self.explicit.is_some() {
                RESOLVE_EXPLICIT
            } else {
                &script
            },
            &[],
        );
        let mut probe = Command::new(&command[0]);
        probe.args(&command[1..]).env_remove("TMUX");
        probe
    }

    pub async fn check(&self, workdir: &Path) -> Result<String> {
        let output = timeout(
            Duration::from_secs(15),
            self.probe()
                .current_dir(workdir)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("默认 shell 启动检查超时，请检查 shell 启动文件")?
        .with_context(|| format!("无法启动默认 shell {}", self.shell.display()))?;
        if !output.status.success() {
            bail!(
                "默认 shell {} 中的命令检查失败：{}",
                self.shell.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix("__AGENT_WEB_LAUNCHER__"))
            .map(str::to_owned)
            .context("默认 shell 未返回命令检查结果，请检查 shell 启动文件")
    }

    pub fn command(&self, args: &[String]) -> Vec<String> {
        let script = if self.claude {
            claude_script(LAUNCH)
        } else {
            LAUNCH.into()
        };
        let explicit_script = self.explicit.as_ref().map(|program| {
            let word = if !program.is_empty()
                && program
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            {
                program.clone()
            } else {
                format!("'{}'", program.replace('\'', "'\\''"))
            };
            format!("shift\n{word} \"$@\"")
        });
        self.argv(explicit_script.as_deref().unwrap_or(&script), args)
    }
}

pub fn default_shell() -> PathBuf {
    if let Some(shell) = env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .map(PathBuf::from)
    {
        return shell;
    }
    account_shell().unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

#[cfg(unix)]
fn account_shell() -> Option<PathBuf> {
    use std::{ffi::CStr, os::unix::ffi::OsStrExt};
    let mut buffer = vec![0_u8; 16_384];
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    // getpwuid_r writes into caller-owned storage and is safe to use concurrently.
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    let entry = unsafe { entry.assume_init() };
    if entry.pw_shell.is_null() {
        return None;
    }
    let shell = unsafe { CStr::from_ptr(entry.pw_shell) }.to_bytes();
    if shell.is_empty() {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(shell)))
}

#[cfg(not(unix))]
fn account_shell() -> Option<PathBuf> {
    None
}
