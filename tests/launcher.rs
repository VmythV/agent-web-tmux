use agent_web_tmux::{launcher::ShellLauncher, tmux::TmuxSession};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    time::timeout,
};

struct Fixture {
    root: PathBuf,
    session: TmuxSession,
}

impl Fixture {
    fn new(shell: &str, setup: &str, external_cx: bool, explicit: bool) -> Self {
        let root = std::env::temp_dir().join(format!("codex-web-shell-{}", uuid::Uuid::new_v4()));
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        for program in if external_cx {
            vec!["codex", "cx"]
        } else {
            vec!["codex"]
        } {
            let path = bin.join(program);
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\nprintf 'LAUNCHED_{program}\\n'\nfor arg do printf 'ARG=<%s>\\n' \"$arg\"; done\nwhile IFS= read -r line; do :; done\n"
                ),
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let rc = format!("export PATH='{}:/usr/bin:/bin'\n{setup}\n", bin.display());
        fs::write(root.join(".zshrc"), &rc).unwrap();
        fs::write(root.join(".bashrc"), &rc).unwrap();
        fs::write(root.join(".bash_profile"), ". \"$HOME/.bashrc\"\n").unwrap();
        let launcher = ShellLauncher::new(
            shell.into(),
            explicit.then(|| bin.join("codex").to_string_lossy().into_owned()),
        );
        let mut command = vec![
            "/usr/bin/env".into(),
            format!("HOME={}", root.display()),
            format!("ZDOTDIR={}", root.display()),
        ];
        command.extend(launcher.command(&[
            "中文 空格".into(),
            "--no-alt-screen".into(),
            "$(touch SHOULD_NOT_EXIST); `touch SHOULD_NOT_EXIST` 'quoted'".into(),
        ]));
        let mut session = TmuxSession::new(root.clone(), command);
        session.socket = format!("codex-web-launcher-test-{}", uuid::Uuid::new_v4());
        Self { root, session }
    }

    async fn screen(&self, expected: &str) -> String {
        timeout(Duration::from_secs(8), async {
            loop {
                let screen = self.session.capture().await.unwrap();
                if screen.contains(expected) {
                    break screen;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("missing terminal output: {expected}"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.session.tmux_bin)
            .args(["-L", &self.session.socket, "kill-server"])
            .output();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn login_shell_loads_aliases_functions_and_prefers_cx() {
    for shell in ["/bin/zsh", "/bin/bash"] {
        if !PathBuf::from(shell).exists() {
            continue;
        }
        for setup in [
            "alias cx='codex --via-alias'",
            "cx() { codex --via-function \"$@\"; }",
            "",
        ] {
            let fixture = Fixture::new(shell, setup, setup.is_empty(), false);
            fixture.session.ensure(160, 30).await.unwrap();
            let expected = match setup {
                "" => "LAUNCHED_cx",
                setup if setup.starts_with("alias") => "ARG=<--via-alias>",
                _ => "ARG=<--via-function>",
            };
            let screen = fixture.screen(expected).await;
            assert!(screen.contains("ARG=<中文 空格>"), "{shell}: {screen}");
            assert!(screen.contains("ARG=<--no-alt-screen>"));
            assert!(screen.contains("$(touch SHOULD_NOT_EXIST)"));
            assert!(!fixture.root.join("SHOULD_NOT_EXIST").exists());
            assert!(fixture.session.status().await.unwrap().alive);
        }
    }
}

#[tokio::test]
async fn uses_codex_when_cx_is_missing_and_honors_explicit_binary() {
    for shell in ["/bin/zsh", "/bin/bash"] {
        if !PathBuf::from(shell).exists() {
            continue;
        }
        for explicit in [false, true] {
            let fixture = Fixture::new(shell, "", explicit, explicit);
            fixture.session.ensure(160, 30).await.unwrap();
            let screen = fixture.screen("LAUNCHED_codex").await;
            assert!(!screen.contains("LAUNCHED_cx"));
        }
    }
}

#[tokio::test]
async fn claude_prefers_cc_alias_or_function_and_falls_back_to_claude() {
    for shell in ["/bin/zsh", "/bin/bash"] {
        if !PathBuf::from(shell).exists() {
            continue;
        }
        for setup in [
            "alias cc='claude --via-alias'",
            "cc() { claude --via-function \"$@\"; }",
            "",
        ] {
            let mut fixture = Fixture::new(shell, setup, false, false);
            let path = fixture.root.join("bin/claude");
            let source = fs::read_to_string(fixture.root.join("bin/codex"))
                .unwrap()
                .replace("LAUNCHED_codex", "LAUNCHED_claude");
            fs::write(&path, source).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            fixture.session.command = vec![
                "/usr/bin/env".into(),
                format!("HOME={}", fixture.root.display()),
                format!("ZDOTDIR={}", fixture.root.display()),
            ];
            fixture.session.command.extend(
                ShellLauncher::claude(shell.into(), None)
                    .command(&["--session-id".into(), "中文 空格".into()]),
            );
            fixture.session.ensure(160, 30).await.unwrap();
            let screen = fixture.screen("ARG=<中文 空格>").await;
            assert!(screen.contains("LAUNCHED_claude"));
            if setup.starts_with("alias") {
                assert!(screen.contains("ARG=<--via-alias>"));
            }
            if setup.starts_with("cc()") {
                assert!(screen.contains("ARG=<--via-function>"));
            }
        }
    }
}

#[tokio::test]
async fn failing_cx_is_not_silently_replaced_with_codex() {
    let fixture = Fixture::new(
        "/bin/bash",
        "cx() { printf 'CX_FAILED\\n'; return 42; }",
        false,
        false,
    );
    fixture.session.ensure(160, 30).await.unwrap();
    let screen = fixture.screen("CX_FAILED").await;
    assert!(!screen.contains("LAUNCHED_codex"));
}

#[tokio::test]
async fn probe_checks_names_without_invoking_cli() {
    // An isolated process avoids changing this test runner's global HOME / PATH.
    for shell in ["/bin/zsh", "/bin/bash"] {
        if !PathBuf::from(shell).exists() {
            continue;
        }
        for (setup, expected) in [
            (
                "cx() { /usr/bin/touch \"$HOME/UNEXPECTED_CLI_CALL\"; return 42; }",
                "cx",
            ),
            ("alias cx='codex --via-alias'", "cx"),
            ("", "codex"),
        ] {
            let fixture = Fixture::new(shell, setup, false, false);
            let mut child = Command::new(env!("CARGO_BIN_EXE_agent-web-tmux"))
                .env("HOME", &fixture.root)
                .env("ZDOTDIR", &fixture.root)
                .env("SHELL", shell)
                .env("CODEX_WORKDIR", &fixture.root)
                .env_remove("CODEX_BIN")
                .env("AGENT", "codex")
                .env("PORT", "0")
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
            let selected = timeout(Duration::from_secs(20), async {
                while let Some(line) = lines.next_line().await.unwrap() {
                    if line.starts_with("默认 shell:") {
                        return line;
                    }
                }
                panic!("server exited before startup completed");
            })
            .await
            .unwrap();
            assert!(
                selected.contains(&format!("启动命令: {expected}")),
                "{selected}"
            );
            assert!(!fixture.root.join("UNEXPECTED_CLI_CALL").exists());
            child.kill().await.unwrap();
        }
    }
}
