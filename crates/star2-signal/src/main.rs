//! star2 signal server - rendezvous only. Audio never touches this process.
//!
//! Two jobs:
//!   1. **WebSocket rendezvous**: room membership, and forwarding P2P offer/answer/
//!      candidate/abort between the two members of a room.
//!   2. **UDP reflexive responder**: answers `flags::REFLEX` probes with the source
//!      address it observed, so a client learns its own NAT mapping (mini-STUN).
//!
//! Deliberately not here: media forwarding, auth beyond a shared token, persistence.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use star2_proto::{flags, ClientMsg, MediaHeader, Member, ServerMsg, SessionId, MEDIA_HEADER_LEN};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

/// One connected client.
struct Session {
    name: String,
    room: Option<String>,
    tx: UnboundedSender<ServerMsg>,
}

struct Hub {
    sessions: HashMap<SessionId, Session>,
    /// room -> members. A room is dropped when its last member leaves.
    rooms: HashMap<String, Vec<SessionId>>,
}

struct App {
    hub: Mutex<Hub>,
    next_session: AtomicU32,
    token: String,
    /// The public `host:port` clients should send REFLEX probes to.
    reflex: String,
    /// Connected updaters waiting to be told a new build exists. These are NOT call
    /// sessions - they hold no room and never touch media.
    updates: Mutex<Vec<UnboundedSender<String>>>,
}

impl App {
    /// Send to one session, if it still exists.
    fn send(hub: &Hub, to: SessionId, msg: ServerMsg) {
        if let Some(s) = hub.sessions.get(&to) {
            let _ = s.tx.send(msg);
        }
    }

    /// Send to everyone in `room` except `except`.
    fn broadcast(hub: &Hub, room: &str, except: SessionId, msg: &ServerMsg) {
        let Some(members) = hub.rooms.get(room) else { return };
        for &m in members.iter().filter(|&&m| m != except) {
            Self::send(hub, m, msg.clone());
        }
    }

    /// Remove `id` from whatever room it is in, telling the room it left.
    fn leave_room(hub: &mut Hub, id: SessionId) {
        let Some(room) = hub.sessions.get_mut(&id).and_then(|s| s.room.take()) else { return };
        if let Some(members) = hub.rooms.get_mut(&room) {
            members.retain(|&m| m != id);
            if members.is_empty() {
                hub.rooms.remove(&room);
            }
        }
        Self::broadcast(hub, &room, id, &ServerMsg::Left { session: id });
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ws_bind = env_or("STAR2_WS_BIND", "127.0.0.1:9101");
    let udp_bind = env_or("STAR2_UDP_BIND", "0.0.0.0:40001");
    let reflex = env_or("STAR2_REFLEX", "127.0.0.1:40001");
    let token = env_or("STAR2_TOKEN", "star2-dev");

    let app = Arc::new(App {
        hub: Mutex::new(Hub { sessions: HashMap::new(), rooms: HashMap::new() }),
        next_session: AtomicU32::new(1),
        token,
        reflex: reflex.clone(),
        updates: Mutex::new(Vec::new()),
    });

    // --- UDP reflexive responder (our mini-STUN) ---
    let sock = UdpSocket::bind(&udp_bind).await?;
    eprintln!("[signal] reflex udp on {udp_bind} (advertising {reflex})");
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, src)) = sock.recv_from(&mut buf).await else { continue };
            // Only answer well-formed REFLEX probes; ignore anything else that lands here.
            let Some(h) = MediaHeader::decode(&buf[..n]) else { continue };
            if !h.is_reflex() {
                continue;
            }
            let observed = src.to_string();
            let mut reply = vec![0u8; MEDIA_HEADER_LEN + observed.len()];
            MediaHeader::new(h.session, 0, 0, flags::REFLEX | flags::KEEPALIVE)
                .encode(&mut reply[..MEDIA_HEADER_LEN]);
            reply[MEDIA_HEADER_LEN..].copy_from_slice(observed.as_bytes());
            let _ = sock.send_to(&reply, src).await;
        }
    });

    // --- WebSocket rendezvous ---
    let router = axum::Router::new()
        .route("/", get(ws_handler))
        .route("/updates", get(updates_handler))
        .route("/notify", axum::routing::post(notify_handler))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(&ws_bind).await?;
    eprintln!("[signal] ws on {ws_bind}");
    axum::serve(listener, router).await?;
    Ok(())
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

async fn ws_handler(ws: WebSocketUpgrade, State(app): State<Arc<App>>) -> impl IntoResponse {
    ws.on_upgrade(move |sock| client_conn(sock, app))
}

// ---------------------------------------------------------------------------
// Update notification plane
// ---------------------------------------------------------------------------
// Deliberately dumb: the server never stores or serves a build, it only says
// "go look again". The updater fetches the manifest from v15.studio and decides
// for itself, so a malicious or confused nudge can't point anyone at a binary.

/// `GET /updates` - an updater parks here waiting for a nudge. No auth: the only
/// thing it can learn is that a build happened.
async fn updates_handler(ws: WebSocketUpgrade, State(app): State<Arc<App>>) -> impl IntoResponse {
    ws.on_upgrade(move |sock| updater_conn(sock, app))
}

async fn updater_conn(sock: WebSocket, app: Arc<App>) {
    let (mut out, mut inc) = sock.split();
    let (tx, mut rx) = unbounded_channel::<String>();
    app.updates.lock().unwrap().push(tx);
    eprintln!("[signal] updater connected ({} total)", app.updates.lock().unwrap().len());

    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if out.send(Message::Text(m.into())).await.is_err() {
                break;
            }
        }
    });
    // We expect nothing from an updater; this just waits for the socket to close.
    while let Some(Ok(_)) = inc.next().await {}
    writer.abort();
    // Drop closed senders: this is the only place the list is pruned.
    app.updates.lock().unwrap().retain(|t| !t.is_closed());
    eprintln!("[signal] updater gone ({} left)", app.updates.lock().unwrap().len());
}

/// `POST /notify` with header `x-token: <token>` - tell every updater to re-check.
/// Called by `deploy/publish-client.sh` right after a successful upload.
async fn notify_handler(
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let ok = headers.get("x-token").and_then(|v| v.to_str().ok()) == Some(app.token.as_str());
    if !ok {
        return (axum::http::StatusCode::UNAUTHORIZED, "bad token\n".to_string());
    }
    let subs = app.updates.lock().unwrap();
    for t in subs.iter() {
        let _ = t.send(r#"{"t":"Update"}"#.to_string());
    }
    let n = subs.len();
    eprintln!("[signal] notified {n} updater(s)");
    (axum::http::StatusCode::OK, format!("notified {n}\n"))
}

async fn client_conn(sock: WebSocket, app: Arc<App>) {
    let (mut out, mut inc) = sock.split();
    let (tx, mut rx) = unbounded_channel::<ServerMsg>();

    // Pump queued ServerMsgs to the socket on their own task, so a slow client can
    // never block the hub lock held by whoever is broadcasting to it.
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let Ok(txt) = serde_json::to_string(&m) else { continue };
            if out.send(Message::Text(txt.into())).await.is_err() {
                break;
            }
        }
    });

    // --- Handshake: the first message must be a valid Hello ---
    let mut id: Option<SessionId> = None;
    while let Some(Ok(msg)) = inc.next().await {
        let Message::Text(txt) = msg else { continue };
        let Ok(cm) = serde_json::from_str::<ClientMsg>(&txt) else { continue };

        // Everything before a successful Hello is rejected.
        let Some(me) = id else {
            match cm {
                ClientMsg::Hello { name, ver, token } => {
                    if ver != star2_proto::PROTO_VERSION as u32 {
                        let _ = tx.send(ServerMsg::Error { msg: format!("proto {ver} != {}", star2_proto::PROTO_VERSION) });
                        break;
                    }
                    if token != app.token {
                        let _ = tx.send(ServerMsg::Error { msg: "bad token".into() });
                        break;
                    }
                    let me = app.next_session.fetch_add(1, Ordering::Relaxed);
                    app.hub.lock().unwrap().sessions.insert(
                        me,
                        Session { name: name.clone(), room: None, tx: tx.clone() },
                    );
                    let _ = tx.send(ServerMsg::Welcome { session: me, reflex: app.reflex.clone() });
                    eprintln!("[signal] session {me} hello name={name}");
                    id = Some(me);
                }
                _ => {
                    let _ = tx.send(ServerMsg::Error { msg: "expected Hello".into() });
                    break;
                }
            }
            continue;
        };

        handle(&app, me, cm);
    }

    // --- Teardown ---
    if let Some(me) = id {
        let mut hub = app.hub.lock().unwrap();
        App::leave_room(&mut hub, me);
        hub.sessions.remove(&me);
        eprintln!("[signal] session {me} gone");
    }
    writer.abort();
}

/// Handle one post-handshake message from session `me`.
fn handle(app: &App, me: SessionId, cm: ClientMsg) {
    let mut hub = app.hub.lock().unwrap();
    match cm {
        ClientMsg::Hello { .. } => {} // already said hello; ignore
        ClientMsg::Join { room } => {
            App::leave_room(&mut hub, me);
            hub.rooms.entry(room.clone()).or_default().push(me);
            if let Some(s) = hub.sessions.get_mut(&me) {
                s.room = Some(room.clone());
            }
            let name = hub.sessions.get(&me).map(|s| s.name.clone()).unwrap_or_default();
            App::broadcast(&hub, &room, me, &ServerMsg::Joined { session: me, name });
            // Roster snapshot back to the joiner, so it sees whoever was already here.
            let members = hub
                .rooms
                .get(&room)
                .map(|ms| {
                    ms.iter()
                        .filter_map(|&m| {
                            hub.sessions.get(&m).map(|s| Member { session: m, name: s.name.clone() })
                        })
                        .collect()
                })
                .unwrap_or_default();
            App::send(&hub, me, ServerMsg::Room { room, members });
        }
        ClientMsg::Leave => App::leave_room(&mut hub, me),
        // P2P signaling: forward verbatim, stamping `from`, but only within a room.
        ClientMsg::P2pOffer { to, nonce, cands } => {
            forward(&hub, me, to, ServerMsg::P2pOffer { from: me, nonce, cands })
        }
        ClientMsg::P2pAnswer { to, nonce, cands } => {
            forward(&hub, me, to, ServerMsg::P2pAnswer { from: me, nonce, cands })
        }
        ClientMsg::P2pCandidate { to, cand } => {
            forward(&hub, me, to, ServerMsg::P2pCandidate { from: me, cand })
        }
        ClientMsg::P2pAbort { to } => forward(&hub, me, to, ServerMsg::P2pAbort { from: me }),
    }
}

/// Deliver a P2P message only if both sessions are in the same room - otherwise any
/// client could spray offers (and punch nonces) at strangers.
fn forward(hub: &Hub, from: SessionId, to: SessionId, msg: ServerMsg) {
    let room_of = |id: SessionId| hub.sessions.get(&id).and_then(|s| s.room.clone());
    if room_of(from).is_some() && room_of(from) == room_of(to) {
        App::send(hub, to, msg);
    }
}
