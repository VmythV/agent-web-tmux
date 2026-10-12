use agent_web_tmux::{AppState, router, tmux::TmuxSession};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct TestServer {
    state: AppState,
    base: String,
    quit: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl TestServer {
    async fn start(session: TmuxSession) -> Self {
        Self::start_state(AppState::new(session)).await
    }

    async fn start_state(state: AppState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (quit, stopped) = oneshot::channel();
        let app = router(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Self {
            state,
            base,
            quit: Some(quit),
            task,
        }
    }

    async fn close(&mut self) {
        self.state.shutdown();
        if let Some(quit) = self.quit.take() {
            let _ = quit.send(());
        }
        timeout(Duration::from_secs(5), &mut self.task)
            .await
            .expect("server shutdown timeout")
            .unwrap();
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.state.shutdown();
        self.task.abort();
        // Clean up only this test's isolated socket, even after an assertion fails.
        let session = self.state.session();
        let _ = std::process::Command::new(&session.tmux_bin)
            .args(["-L", &session.socket, "kill-server"])
            .output();
    }
}

fn fixture_session() -> TmuxSession {
    let mut session = TmuxSession::new(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "stty raw -echo; printf 'MVP_READY\\r\\n'; cat".into(),
        ],
    );
    session.socket = format!("codex-web-rust-test-{}", uuid::Uuid::new_v4());
    session
}

async fn next_json(socket: &mut Socket) -> Value {
    loop {
        let message = timeout(Duration::from_secs(8), socket.next())
            .await
            .expect("WebSocket response timeout")
            .expect("WebSocket closed")
            .expect("WebSocket error");
        if let Message::Text(text) = message {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

async fn connect(server: &TestServer) -> Socket {
    connect_session(server, None).await
}

async fn connect_session(server: &TestServer, name: Option<&str>) -> Socket {
    let mut request = format!("{}/terminal", server.base.replace("http", "ws"))
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Origin", server.base.parse().unwrap());
    let (mut socket, _) = connect_async(request).await.unwrap();
    socket
        .send(Message::Text(
            json!({"type":"init","cols":90,"rows":25,"session":name})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    loop {
        let response = next_json(&mut socket).await;
        assert_ne!(response["type"], "error", "{response}");
        if response["type"] == "ready" {
            break;
        }
    }
    socket
}

#[tokio::test]
async fn independent_sessions_survive_switch_restart_and_selective_close() {
    let config = fixture_session();
    let mut server = TestServer::start(config.clone()).await;
    let client = Client::new();
    let mut sessions = Vec::new();
    for _ in 0..2 {
        let response = client
            .post(format!("{}/api/sessions", server.base))
            .header("Origin", &server.base)
            .json(&json!({"cols":90,"rows":25}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        sessions.push(response.json::<Value>().await.unwrap());
    }
    let first = sessions[0]["session"].as_str().unwrap();
    let second = sessions[1]["session"].as_str().unwrap();
    assert_ne!(first, second);
    assert_ne!(sessions[0]["pid"], sessions[1]["pid"]);
    assert_eq!(sessions[0]["socket"], sessions[1]["socket"]);
    let listing: Value = client
        .get(format!("{}/api/sessions", server.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["sessions"].as_array().unwrap().len(), 2);
    let mut a = connect_session(&server, Some(first)).await;
    let mut b = connect_session(&server, Some(second)).await;
    for (socket, text) in [(&mut a, "ONLY_FIRST"), (&mut b, "ONLY_SECOND")] {
        socket
            .send(Message::Text(
                json!({"type":"input","data":text}).to_string().into(),
            ))
            .await
            .unwrap();
    }
    timeout(Duration::from_secs(5), async {
        loop {
            let mut found = true;
            for (name, expected, other) in [
                (first, "ONLY_FIRST", "ONLY_SECOND"),
                (second, "ONLY_SECOND", "ONLY_FIRST"),
            ] {
                let mut session = config.clone();
                session.name = name.into();
                let screen = session.capture().await.unwrap();
                found &= screen.contains(expected);
                assert!(!screen.contains(other));
            }
            if found {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    // Closing the missing legacy name must not prefix-match either independent session.
    assert_eq!(
        client
            .delete(format!("{}/api/session", server.base))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    server.close().await;
    let mut restarted = TestServer::start(config.clone()).await;
    for (index, name) in [first, second].iter().enumerate() {
        let status: Value = client
            .get(format!("{}/api/session?session={name}", restarted.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status["pid"], sessions[index]["pid"]);
        assert_eq!(status["alive"], true);
    }
    let mut a = connect_session(&restarted, Some(first)).await;
    let _b = connect_session(&restarted, Some(second)).await;
    for _ in 0..2 {
        assert_eq!(
            client
                .delete(format!("{}/api/session?session={first}", restarted.base))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    loop {
        if next_json(&mut a).await["type"] == "session-ended" {
            break;
        }
    }
    let remaining: Value = client
        .get(format!("{}/api/session?session={second}", restarted.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(remaining["pid"], sessions[1]["pid"]);
    assert_eq!(remaining["alive"], true);
    let listing: Value = client
        .get(format!("{}/api/sessions", restarted.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["sessions"].as_array().unwrap().len(), 1);
    for name in ["codex-web*", "../other", "unmanaged"] {
        assert_eq!(
            client
                .delete(format!("{}/api/session?session={name}", restarted.base))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    restarted.close().await;
}

#[tokio::test]
async fn concurrent_transcripts_stay_bound_after_backend_restart() {
    let home =
        std::env::temp_dir().join(format!("codex-web-history-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    let home = home.canonicalize().unwrap();
    let script = format!(
        r#"
exec 3>>'{}/sessions/rollout-'"$CODEX_WEB_TMUX_SESSION"'.jsonl'
printf '{{"type":"session_meta","payload":{{"id":"%s","source":"cli","cwd":"{}"}}}}\n' "$CODEX_WEB_TMUX_SESSION" >&3
printf '{{"type":"event_msg","payload":{{"type":"item_completed","item":{{"type":"UserMessage","id":"user-1","content":[{{"type":"text","text":"%s"}}]}}}}}}\n' "$CODEX_WEB_TMUX_SESSION" >&3
stty raw -echo
cat
"#,
        home.display(),
        home.display()
    );
    let mut config = TmuxSession::new(home.clone(), vec!["/bin/sh".into(), "-c".into(), script]);
    config.socket = format!("codex-web-history-test-{}", uuid::Uuid::new_v4());
    let mut server =
        TestServer::start_state(AppState::with_history(config.clone(), home.clone())).await;
    let client = Client::new();
    let mut names = Vec::new();
    for _ in 0..2 {
        let response: Value = client
            .post(format!("{}/api/sessions", server.base))
            .json(&json!({"cols":90,"rows":25}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        names.push(response["session"].as_str().unwrap().to_owned());
    }
    for name in &names {
        timeout(Duration::from_secs(8), async {
            loop {
                let chat: Value = client
                    .get(format!("{}/api/chat?session={name}", server.base))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if chat["messages"]
                    .as_array()
                    .is_some_and(|messages| !messages.is_empty())
                {
                    assert_eq!(chat["sessionId"], *name);
                    assert_eq!(chat["messages"][0]["text"], *name);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
    }
    server.close().await;
    let mut restarted = TestServer::start_state(AppState::with_history(config, home.clone())).await;
    for name in &names {
        let chat: Value = client
            .get(format!("{}/api/chat?session={name}", restarted.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(chat["sessionId"], *name);
        assert_eq!(chat["messages"][0]["text"], *name);
    }
    restarted.close().await;
    drop(restarted);
    drop(server);
    std::fs::remove_dir_all(home).unwrap();
}

async fn await_screen(server: &TestServer, texts: &[&str]) {
    timeout(Duration::from_secs(8), async {
        loop {
            let screen = server.state.session().capture().await.unwrap();
            if texts.iter().all(|text| screen.contains(text)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("terminal screen timeout");
}

#[tokio::test]
async fn terminal_survives_disconnect_and_backend_restart() {
    let config = fixture_session();
    let mut server = TestServer::start(config.clone()).await;
    let client = Client::new();
    assert!(!server.state.session().status().await.unwrap().exists);
    let mut socket = connect(&server).await;
    await_screen(&server, &["MVP_READY"]).await;
    let original_pid = server.state.session().status().await.unwrap().pid;
    socket
        .send(Message::Text(
            json!({"type":"input","data":"ping\r"}).to_string().into(),
        ))
        .await
        .unwrap();
    await_screen(&server, &["ping"]).await;
    socket
        .send(Message::Text(
            json!({"type":"paste","text":"中文first\nsecond","submit":true})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    await_screen(&server, &["中文first", "second"]).await;
    socket
        .send(Message::Text(
            json!({"type":"resize","cols":110,"rows":35})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            let status = server.state.session().status().await.unwrap();
            if status.cols == Some(110) && status.rows == Some(35) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    socket.close(None).await.unwrap();
    drop(socket);
    assert_eq!(
        server.state.session().status().await.unwrap().pid,
        original_pid
    );
    let mut socket = connect(&server).await;
    loop {
        if next_json(&mut socket).await["type"] == "output" {
            break;
        }
    }
    assert_eq!(
        server.state.session().status().await.unwrap().pid,
        original_pid
    );
    // Shutdown must also close a currently connected browser, not just an idle server.
    server.close().await;
    assert!(server.state.session().status().await.unwrap().alive);
    let mut restarted = TestServer::start(config).await;
    let mut socket = connect(&restarted).await;
    assert_eq!(
        restarted.state.session().status().await.unwrap().pid,
        original_pid
    );
    let mut other = connect(&restarted).await;
    loop {
        if next_json(&mut socket).await["type"] == "taken-over" {
            break;
        }
    }
    other
        .send(Message::Text(
            json!({"type":"resize","cols":-1,"rows":10})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    loop {
        if next_json(&mut other).await["type"] == "error" {
            break;
        }
    }
    assert!(restarted.state.session().status().await.unwrap().alive);
    let response = client
        .delete(format!("{}/api/session", restarted.base))
        .header("Origin", &restarted.base)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Value>().await.unwrap()["exists"], false);
    drop(other);
    restarted.close().await;
}

#[tokio::test]
async fn allows_external_links_but_rejects_cross_origin_terminal_control() {
    let mut server = TestServer::start(fixture_session()).await;
    let client = Client::new();
    assert_eq!(
        client.get(&server.base).send().await.unwrap().status(),
        StatusCode::OK
    );
    for (path, mode, dest, expected) in [
        ("/", "navigate", "document", StatusCode::OK),
        ("/", "navigate", "iframe", StatusCode::FORBIDDEN),
        ("/", "cors", "empty", StatusCode::FORBIDDEN),
        (
            "/api/session",
            "navigate",
            "document",
            StatusCode::FORBIDDEN,
        ),
        ("/api/screen", "cors", "empty", StatusCode::FORBIDDEN),
    ] {
        let response = client
            .get(format!("{}{path}", server.base))
            .header("Sec-Fetch-Site", "cross-site")
            .header("Sec-Fetch-Mode", mode)
            .header("Sec-Fetch-Dest", dest)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path}");
    }
    assert_eq!(
        client
            .get(&server.base)
            .header("Host", "evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .delete(format!("{}/api/session", server.base))
            .header("Origin", "https://example.com")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let mut request = format!("{}/terminal", server.base.replace("http", "ws"))
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Origin", "https://example.com".parse().unwrap());
    match connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status().as_u16(), 403)
        }
        _ => panic!("cross-origin WebSocket must be rejected"),
    }
    assert!(!server.state.session().status().await.unwrap().exists);
    server.close().await;
}
