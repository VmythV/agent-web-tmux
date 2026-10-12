use crate::{
    agent::Agent,
    claude_history,
    history::{self, ChatSnapshot},
    session_history,
    terminal::{Output, Terminal},
    tmux::{TmuxSession, terminal_size},
};
use anyhow::{Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{
        Query, Request, State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    http::{HeaderMap, Method, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::{
    sync::{Mutex as AsyncMutex, mpsc, watch},
    time::{Duration, timeout},
};

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'";

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

struct Inner {
    session: TmuxSession,
    operation: AsyncMutex<()>,
    active: Mutex<HashMap<String, ActiveClient>>,
    next_id: AtomicU64,
    closing: AtomicBool,
    shutdown: watch::Sender<bool>,
    codex_home: std::path::PathBuf,
    history_path: AsyncMutex<HashMap<String, std::path::PathBuf>>,
    binding: Mutex<HashSet<String>>,
    agents: Vec<Agent>,
    default_agent: String,
    verify_launchers: bool,
    claude_home: std::path::PathBuf,
}

struct ActiveClient {
    id: u64,
    control: mpsc::UnboundedSender<Control>,
}

enum Control {
    TakenOver,
    SessionEnded,
    Shutdown,
}

impl AppState {
    pub fn new(session: TmuxSession) -> Self {
        Self::with_history(session, history::default_home())
    }

    pub fn with_history(session: TmuxSession, codex_home: std::path::PathBuf) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self(Arc::new(Inner {
            agents: vec![Agent {
                id: session.agent.clone(),
                label: "Codex".into(),
                chat_supported: true,
                command: session.command.clone(),
            }],
            default_agent: session.agent.clone(),
            verify_launchers: false,
            claude_home: claude_history::default_home(),
            session,
            operation: AsyncMutex::new(()),
            active: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closing: AtomicBool::new(false),
            shutdown,
            codex_home,
            history_path: AsyncMutex::new(HashMap::new()),
            binding: Mutex::new(HashSet::new()),
        }))
    }

    pub fn session(&self) -> &TmuxSession {
        &self.0.session
    }

    pub fn with_agents(mut self, agents: Vec<Agent>, default_agent: String) -> Self {
        let inner = Arc::get_mut(&mut self.0).expect("configure before cloning state");
        inner.agents = agents;
        inner.default_agent = default_agent;
        self
    }

    pub fn verify_launchers(mut self) -> Self {
        Arc::get_mut(&mut self.0)
            .expect("configure before cloning state")
            .verify_launchers = true;
        self
    }

    pub fn shutdown(&self) {
        self.0.closing.store(true, Ordering::SeqCst);
        self.0.shutdown.send_replace(true);
        for active in self.0.active.lock().unwrap().values() {
            let _ = active.control.send(Control::Shutdown);
        }
    }

    fn resolve(&self, name: Option<&str>) -> Result<TmuxSession> {
        let name = name.unwrap_or(&self.0.session.name);
        anyhow::ensure!(self.valid_name(name), "会话标识无效");
        let mut session = self.0.session.clone();
        session.name = name.into();
        Ok(session)
    }

    fn valid_name(&self, name: &str) -> bool {
        name == self.0.session.name
            || name
                .strip_prefix("codex-web-")
                .or_else(|| name.strip_prefix("agent-web-"))
                .is_some_and(|suffix| {
                    suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
    }

    async fn bind_history(&self, session: &TmuxSession) -> Result<Option<std::path::PathBuf>> {
        let status = session.status().await?;
        if status.agent == "custom" {
            return Ok(None);
        }
        if let Some(path) = self.0.history_path.lock().await.get(&session.name).cloned() {
            return Ok(Some(path));
        }
        if let Some(path) = session
            .option("@codex-web-history")
            .await?
            .map(std::path::PathBuf::from)
            && if status.agent == "claude" {
                session
                    .name
                    .rsplit('-')
                    .next()
                    .and_then(|suffix| uuid::Uuid::parse_str(suffix).ok())
                    .is_some_and(|id| {
                        claude_history::belongs(&path, &session.workdir, &id.to_string())
                    })
            } else {
                session_history::belongs(&path, &session.workdir)
            }
        {
            self.0
                .history_path
                .lock()
                .await
                .insert(session.name.clone(), path.clone());
            return Ok(Some(path));
        }
        if status.agent == "claude" {
            if let Some(path) =
                claude_history::discover(&self.0.claude_home, &session.workdir, &session.name)?
            {
                session
                    .set_option("@codex-web-history", &path.to_string_lossy())
                    .await?;
                self.0
                    .history_path
                    .lock()
                    .await
                    .insert(session.name.clone(), path.clone());
                return Ok(Some(path));
            }
            return Ok(None);
        }
        if !status.alive {
            return Ok(None);
        }
        let Some(pid) = status.pid else {
            return Ok(None);
        };
        if let Some(path) =
            session_history::opened_rollout(pid, &self.0.codex_home, &session.workdir).await?
        {
            session
                .set_option("@codex-web-history", &path.to_string_lossy())
                .await?;
            self.0
                .history_path
                .lock()
                .await
                .insert(session.name.clone(), path.clone());
            return Ok(Some(path));
        }
        Ok(None)
    }

    fn watch_history(&self, session: TmuxSession) {
        if session.agent == "custom" {
            return;
        }
        if !self.0.binding.lock().unwrap().insert(session.name.clone()) {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            let mut shutdown = state.0.shutdown.subscribe();
            loop {
                if *shutdown.borrow() || state.bind_history(&session).await.ok().flatten().is_some()
                {
                    break;
                }
                if !session.status().await.is_ok_and(|status| status.alive) {
                    break;
                }
                tokio::select! { _ = shutdown.changed() => break, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
            }
            state.0.binding.lock().unwrap().remove(&session.name);
        });
    }
}

#[derive(Default, Deserialize)]
struct Selection {
    session: Option<String>,
}

#[derive(Deserialize)]
struct CreateSession {
    cols: u16,
    rows: u16,
    #[serde(default)]
    agent: Option<String>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route(
            "/api/session",
            get(session_status).merge(delete(end_session)),
        )
        .route("/api/screen", get(screen))
        .route("/api/chat", get(chat))
        .route("/terminal", get(terminal_upgrade))
        .fallback(get(asset))
        .layer(middleware::from_fn(local_guard))
        .with_state(state)
}

fn local_request(method: &Method, uri: &Uri, headers: &HeaderMap) -> bool {
    let allowed = || -> Option<bool> {
        let host = headers.get(header::HOST)?.to_str().ok()?;
        let base = url::Url::parse(&format!("http://{host}")).ok()?;
        if !matches!(base.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")) {
            return Some(false);
        }
        // A link from chat is cross-site, but opening the top-level page is valid.
        if method == Method::GET
            && uri.path() == "/"
            && header_value(headers, "sec-fetch-mode") == Some("navigate")
            && header_value(headers, "sec-fetch-dest") == Some("document")
        {
            return Some(true);
        }
        if let Some(origin) = headers.get(header::ORIGIN) {
            if url::Url::parse(origin.to_str().ok()?).ok()?.origin() != base.origin() {
                return Some(false);
            }
        }
        Some(matches!(
            header_value(headers, "sec-fetch-site"),
            None | Some("same-origin" | "none")
        ))
    };
    allowed().unwrap_or(false)
}

fn header_value<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    headers.get(key).and_then(|value| value.to_str().ok())
}

async fn local_guard(request: Request, next: Next) -> Response {
    let mut response = if local_request(request.method(), request.uri(), request.headers()) {
        next.run(request).await
    } else {
        error(StatusCode::FORBIDDEN, "只允许本机、同源访问")
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
        .headers_mut()
        .insert(header::CONTENT_SECURITY_POLICY, CSP.parse().unwrap());
    response
}

fn error(status: StatusCode, message: impl ToString) -> Response {
    (status, Json(json!({"error": message.to_string()}))).into_response()
}

async fn list_sessions(State(state): State<AppState>) -> Response {
    let result = async {
        let mut sessions = Vec::new();
        for name in state.session().names().await? {
            if !state.valid_name(&name) { continue; }
            let session = state.resolve(Some(&name))?;
            let status = session.status().await?;
            if status.exists { sessions.push(status); }
        }
        Ok::<_, anyhow::Error>(json!({"sessions":sessions,"workdir":state.session().workdir,"socket":state.session().socket,"agents":state.0.agents,"defaultAgent":state.0.default_agent}))
    }.await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn create_session(
    State(state): State<AppState>,
    Json(size): Json<CreateSession>,
) -> Response {
    let result = async {
        terminal_size(size.cols, size.rows)?;
        let _operation = state.0.operation.lock().await;
        anyhow::ensure!(!state.0.closing.load(Ordering::SeqCst), "后端正在停止");
        let agent_id = size.agent.as_deref().unwrap_or(&state.0.default_agent);
        let agent = state
            .0
            .agents
            .iter()
            .find(|agent| agent.id == agent_id)
            .ok_or_else(|| anyhow::anyhow!("未配置的 agent：{agent_id}"))?;
        if state.0.verify_launchers {
            Agent::check(agent_id, &state.session().workdir).await?;
        }
        let name = format!("agent-web-{}", uuid::Uuid::new_v4().simple());
        let mut session = state.resolve(Some(&name))?;
        session.command = agent.for_session(&name);
        session.agent = agent.id.clone();
        let status = session.ensure(size.cols, size.rows).await?;
        state.watch_history(session);
        Ok::<_, anyhow::Error>(status)
    }
    .await;
    match result {
        Ok(status) => (StatusCode::CREATED, Json(status)).into_response(),
        Err(err) => error(StatusCode::BAD_REQUEST, err),
    }
}

async fn session_status(
    State(state): State<AppState>,
    Query(selection): Query<Selection>,
) -> Response {
    let session = match state.resolve(selection.session.as_deref()) {
        Ok(session) => session,
        Err(err) => return error(StatusCode::BAD_REQUEST, err),
    };
    match session.status().await {
        Ok(status) => Json(status).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn screen(State(state): State<AppState>, Query(selection): Query<Selection>) -> Response {
    let session = match state.resolve(selection.session.as_deref()) {
        Ok(session) => session,
        Err(err) => return error(StatusCode::BAD_REQUEST, err),
    };
    match session.capture().await {
        Ok(text) => Json(json!({"text":text})).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn chat(State(state): State<AppState>, Query(selection): Query<Selection>) -> Response {
    let session = match state.resolve(selection.session.as_deref()) {
        Ok(session) => session,
        Err(err) => return error(StatusCode::BAD_REQUEST, err),
    };
    let result = async {
        let status = session.status().await?;
        if !status.exists || status.agent == "custom" {
            return Ok::<_, anyhow::Error>(ChatSnapshot::default());
        }
        let mut path = state.bind_history(&session).await?;
        // Migrate the original single-session installation once. New sessions never use time matching.
        if path.is_none()
            && status.agent == "codex"
            && session.name == state.session().name
            && state
                .session()
                .names()
                .await?
                .iter()
                .all(|name| name == &session.name)
            && let Some(started) = session.started_millis().await?
        {
            let home = state.0.codex_home.clone();
            let cwd = session.workdir.clone();
            path = tokio::task::spawn_blocking(move || history::discover(&home, &cwd, started))
                .await??;
            if let Some(path) = &path {
                session
                    .set_option("@codex-web-history", &path.to_string_lossy())
                    .await?;
                state
                    .0
                    .history_path
                    .lock()
                    .await
                    .insert(session.name.clone(), path.clone());
            }
        }
        let Some(path) = path else {
            state.watch_history(session.clone());
            return Ok(ChatSnapshot::default());
        };
        let snapshot = tokio::task::spawn_blocking(move || {
            if status.agent == "claude" {
                claude_history::read(&path)
            } else {
                history::read(&path)
            }
        })
        .await??;
        if let Some(message) = snapshot
            .messages
            .iter()
            .find(|message| message.role == "user")
        {
            let title: String = message
                .text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(22)
                .collect();
            if session.option("@codex-web-title").await?.as_deref() != Some(&title) {
                let _ = session.set_option("@codex-web-title", &title).await;
            }
        }
        Ok(snapshot)
    }
    .await;
    match result {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn end_session(
    State(state): State<AppState>,
    Query(selection): Query<Selection>,
) -> Response {
    let session = match state.resolve(selection.session.as_deref()) {
        Ok(session) => session,
        Err(err) => return error(StatusCode::BAD_REQUEST, err),
    };
    let _operation = state.0.operation.lock().await;
    if let Some(active) = state.0.active.lock().unwrap().remove(&session.name) {
        let _ = active.control.send(Control::SessionEnded);
    }
    if let Err(err) = session.stop().await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, err);
    }
    state.0.history_path.lock().await.remove(&session.name);
    match session.status().await {
        Ok(status) => Json(status).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn asset(uri: Uri) -> Response {
    let (mime, bytes): (&str, &[u8]) = match uri.path() {
        "/" => (
            "text/html; charset=utf-8",
            include_bytes!("../public/index.html"),
        ),
        "/app.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../public/app.js"),
        ),
        "/style.css" => (
            "text/css; charset=utf-8",
            include_bytes!("../public/style.css"),
        ),
        "/vendor/xterm.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../public/vendor/xterm.js"),
        ),
        "/vendor/xterm.css" => (
            "text/css; charset=utf-8",
            include_bytes!("../public/vendor/xterm.css"),
        ),
        "/vendor/addon-fit.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../public/vendor/addon-fit.js"),
        ),
        "/vendor/marked.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../public/vendor/marked.js"),
        ),
        "/vendor/purify.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../public/vendor/purify.js"),
        ),
        "/vendor/xterm.LICENSE" => (
            "text/plain; charset=utf-8",
            include_bytes!("../public/vendor/xterm.LICENSE"),
        ),
        "/vendor/addon-fit.LICENSE" => (
            "text/plain; charset=utf-8",
            include_bytes!("../public/vendor/addon-fit.LICENSE"),
        ),
        _ => return error(StatusCode::NOT_FOUND, "Not found"),
    };
    ([(header::CONTENT_TYPE, mime)], Body::from(bytes)).into_response()
}

async fn terminal_upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if state.0.closing.load(Ordering::SeqCst) || !headers.contains_key(header::ORIGIN) {
        return error(StatusCode::FORBIDDEN, "只允许本机、同源访问");
    }
    upgrade
        .max_message_size(128 * 1024)
        .max_frame_size(128 * 1024)
        .on_upgrade(move |socket| handle_terminal(socket, state))
        .into_response()
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Incoming {
    Init {
        cols: u16,
        rows: u16,
        #[serde(default)]
        session: Option<String>,
    },
    Input {
        data: String,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Paste {
        text: String,
        #[serde(default)]
        submit: bool,
    },
}

type Sender = SplitSink<WebSocket, Message>;

async fn send(sender: &mut Sender, value: Value) -> Result<()> {
    timeout(
        Duration::from_secs(5),
        sender.send(Message::Text(value.to_string().into())),
    )
    .await??;
    Ok(())
}

async fn close(sender: &mut Sender, code: u16, reason: &str) {
    let _ = timeout(
        Duration::from_secs(2),
        sender.send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_owned().into(),
        }))),
    )
    .await;
}

async fn initialize(
    state: &AppState,
    selection: Option<&str>,
    cols: u16,
    rows: u16,
    control: mpsc::UnboundedSender<Control>,
) -> Result<(Terminal, mpsc::Receiver<Output>, Value, u64)> {
    terminal_size(cols, rows)?;
    let Ok(_operation) = state.0.operation.try_lock() else {
        bail!("另一连接正在建立，请稍后重试");
    };
    let mut session = state.resolve(selection)?;
    if selection.is_some() && !session.status().await?.exists {
        bail!("会话已关闭，请新建会话");
    }
    let status = session.ensure(cols, rows).await?;
    session.agent = status.agent.clone();
    if state.0.closing.load(Ordering::SeqCst) {
        bail!("后端正在停止");
    }
    state.watch_history(session.clone());
    let name = session.name.clone();
    let (terminal, output) =
        tokio::task::spawn_blocking(move || Terminal::attach(&session, cols, rows)).await??;
    if state.0.closing.load(Ordering::SeqCst) {
        bail!("后端正在停止");
    }
    let id = state.0.next_id.fetch_add(1, Ordering::Relaxed);
    if let Some(previous) = state
        .0
        .active
        .lock()
        .unwrap()
        .insert(name, ActiveClient { id, control })
    {
        let _ = previous.control.send(Control::TakenOver);
    }
    let mut ready = serde_json::to_value(status)?;
    ready["type"] = json!("ready");
    Ok((terminal, output, ready, id))
}

async fn handle_terminal(socket: WebSocket, state: AppState) {
    let (mut sender, mut incoming) = socket.split();
    let mut shutdown = state.0.shutdown.subscribe();
    if *shutdown.borrow() {
        return;
    }
    let first = tokio::select! {
        message = timeout(Duration::from_secs(10), incoming.next()) => message,
        _ = shutdown.changed() => return,
    };
    let (cols, rows, selection) = match first {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<Incoming>(&text) {
            Ok(Incoming::Init {
                cols,
                rows,
                session,
            }) => (cols, rows, session),
            _ => {
                let _ = send(
                    &mut sender,
                    json!({"type":"error","message":"请先初始化终端"}),
                )
                .await;
                return;
            }
        },
        _ => return,
    };
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let (mut terminal, mut output, ready, id) =
        match initialize(&state, selection.as_deref(), cols, rows, control_tx).await {
            Ok(result) => result,
            Err(err) => {
                let _ = send(
                    &mut sender,
                    json!({"type":"error","message":err.to_string()}),
                )
                .await;
                close(&mut sender, 1011, "Unable to attach").await;
                return;
            }
        };

    let session_name = ready["session"].as_str().unwrap().to_owned();
    if send(&mut sender, ready).await.is_ok() {
        loop {
            tokio::select! {
                _ = shutdown.changed() => { close(&mut sender, 1000, "Server stopped").await; break; }
                control = control_rx.recv() => {
                    let (kind, code, reason) = match control {
                        Some(Control::TakenOver) => ("taken-over", 4001, "Another tab took control"),
                        Some(Control::SessionEnded) => ("session-ended", 1000, "Session ended"),
                        _ => ("detached", 1000, "Server stopped"),
                    };
                    let _ = send(&mut sender, json!({"type":kind})).await;
                    close(&mut sender, code, reason).await;
                    break;
                }
                event = output.recv() => {
                    match event {
                        Some(Output::Data(data)) => {
                            if send(&mut sender, json!({"type":"output","data":data})).await.is_err() { break; }
                        }
                        _ => {
                            let _ = send(&mut sender, json!({"type":"detached"})).await;
                            close(&mut sender, 1000, "Terminal client exited").await;
                            break;
                        }
                    }
                }
                message = incoming.next() => {
                    match message {
                        Some(Ok(Message::Text(text))) => {
                            let result = match serde_json::from_str::<Incoming>(&text) {
                                Ok(message) => handle_input(&mut terminal, message).await,
                                Err(_) => Err(anyhow::anyhow!("终端消息格式无效")),
                            };
                            if let Err(err) = result {
                                if send(&mut sender, json!({"type":"error","message":err.to_string()})).await.is_err() { break; }
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            if !matches!(timeout(Duration::from_secs(5), sender.send(Message::Pong(data))).await, Ok(Ok(()))) { break; }
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        _ => break,
                    }
                }
            }
        }
    }
    // Dropping this bridge detaches the tmux client, while the agent remains alive.
    drop(terminal);
    let mut active = state.0.active.lock().unwrap();
    if active
        .get(&session_name)
        .is_some_and(|client| client.id == id)
    {
        active.remove(&session_name);
    }
}

async fn handle_input(terminal: &mut Terminal, message: Incoming) -> Result<()> {
    match message {
        Incoming::Init { .. } => bail!("连接已初始化"),
        Incoming::Input { data } => {
            if data.len() > 64000 {
                bail!("输入过长");
            }
            terminal.write(&data)?;
        }
        Incoming::Resize { cols, rows } => terminal.resize(cols, rows)?,
        Incoming::Paste { text, submit } => {
            if text.trim().is_empty()
                || text.len() > 60000
                || text
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t')
            {
                bail!("提示文本无效或过长");
            }
            terminal.write(&format!("\x1b[200~{text}\x1b[201~"))?;
            if submit {
                tokio::time::sleep(Duration::from_millis(100)).await;
                terminal.write("\r")?;
            }
        }
    }
    Ok(())
}
