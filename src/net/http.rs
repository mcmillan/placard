use std::net::SocketAddr;
use std::sync::mpsc;

use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};

use crate::command::{Command, CommandError, parse_json};
use crate::state::{Envelope, Reply};

use super::{dispatch, reply_json};

#[derive(Clone)]
struct App {
    tx: mpsc::Sender<Envelope>,
}

/// HTTP listener: `POST /api/command`, `GET /api/status`, `GET /api/history`
/// and the history page at `/` — nothing else. Runs a current-thread tokio
/// runtime on its own thread; the only await points are accept/read/write,
/// so GStreamer never sees async.
pub fn run(listener: std::net::TcpListener, tx: mpsc::Sender<Envelope>) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(ui))
        .route("/api/command", post(command))
        .route("/api/status", get(status))
        .route("/api/history", get(history))
        .with_state(App { tx });

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;
        Ok(())
    })
}

/// The read-only history page; self-contained, no external assets.
async fn ui() -> Html<&'static str> {
    Html(include_str!("history.html"))
}

async fn command(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: String,
) -> Response {
    // Same parser as TCP so the two transports can't drift; parse failures
    // dispatch too so they land in the history.
    let result = run_blocking(app, parse_json(&body), body, peer).await;
    let code = if result.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    json_response(code, reply_json(&result))
}

async fn status(State(app): State<App>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    query(app, Command::Status, peer).await
}

async fn history(State(app): State<App>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    query(app, Command::History, peer).await
}

async fn query(app: App, command: Command, peer: SocketAddr) -> Response {
    let result = run_blocking(app, Ok(command), String::new(), peer).await;
    let code = if result.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    json_response(code, reply_json(&result))
}

/// The state-thread round trip blocks on an mpsc recv; keep it off the
/// current-thread runtime's core.
async fn run_blocking(
    app: App,
    command: Result<Command, CommandError>,
    raw: String,
    peer: SocketAddr,
) -> Result<Reply, CommandError> {
    tokio::task::spawn_blocking(move || dispatch(&app.tx, command, raw, "http", Some(peer)))
        .await
        .unwrap_or(Err(CommandError::ShuttingDown))
}

fn json_response(code: StatusCode, body: String) -> Response {
    (
        code,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}
