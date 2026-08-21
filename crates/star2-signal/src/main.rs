use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use star2_proto::{flags, ClientMsg, MediaHeader, Member, ServerMsg, SessionId, MEDIA_HEADER_LEN};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

struct Session {
    name: String,
    room: Option<String>,
    tx: UnboundedSender<ServerMsg>,
}

struct Hub {
    sessions: HashMap<SessionId, Session>,

    rooms: HashMap<String, Vec<SessionId>>,
}

struct App {
    hub: Mutex<Hub>,
    next_session: AtomicU32,
    token: String,

    reflex: String,

    updates: Mutex<Vec<UnboundedSender<String>>>,
}

impl App {

    fn send(hub: &Hub, to: SessionId, msg: ServerMsg) {
        if let Some(s) = hub.sessions.get(&to) {
            let _ = s.tx.send(msg);
        }
    }

    fn broadcast(hub: &Hub, room: &str, except: SessionId, msg: &ServerMsg) {
        let Some(members) = hub.rooms.get(room) else { return };
        for &m in members.iter().filter(|&&m| m != except) {
            Self::send(hub, m, msg.clone());
        }
    }

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

    let sock = UdpSocket::bind(&udp_bind).await?;
    eprintln!("[signal] reflex udp on {udp_bind} (advertising {reflex})");
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, src)) = sock.recv_from(&mut buf).await else { continue };

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

    while let Some(Ok(_)) = inc.next().await {}
    writer.abort();

    app.updates.lock().unwrap().retain(|t| !t.is_closed());
    eprintln!("[signal] updater gone ({} left)", app.updates.lock().unwrap().len());
}

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

const PING_EVERY: Duration = Duration::from_secs(10);

const DEAD_AFTER: Duration = Duration::from_secs(30);

async fn client_conn(sock: WebSocket, app: Arc<App>) {
    let (mut out, mut inc) = sock.split();
    let (tx, mut rx) = unbounded_channel::<ServerMsg>();

    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await;
        loop {
            let sent = tokio::select! {
                m = rx.recv() => {
                    let Some(m) = m else { break };
                    let Ok(txt) = serde_json::to_string(&m) else { continue };
                    out.send(Message::Text(txt.into())).await
                }
                _ = ping.tick() => out.send(Message::Ping(Vec::new().into())).await,
            };
            if sent.is_err() {
                break;
            }
        }
    });

    let mut id: Option<SessionId> = None;
    loop {

        let Ok(next) = tokio::time::timeout(DEAD_AFTER, inc.next()).await else {
            match id {
                Some(me) => eprintln!("[signal] session {me} timed out"),
                None => eprintln!("[signal] connection timed out before hello"),
            }
            break;
        };
        let Some(Ok(msg)) = next else { break };
        let Message::Text(txt) = msg else { continue };
        let Ok(cm) = serde_json::from_str::<ClientMsg>(&txt) else { continue };

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

    if let Some(me) = id {
        let mut hub = app.hub.lock().unwrap();
        App::leave_room(&mut hub, me);
        hub.sessions.remove(&me);
        eprintln!("[signal] session {me} gone");
    }
    writer.abort();
}

fn handle(app: &App, me: SessionId, cm: ClientMsg) {
    let mut hub = app.hub.lock().unwrap();
    match cm {
        ClientMsg::Hello { .. } => {}
        ClientMsg::Join { room } => {
            App::leave_room(&mut hub, me);
            hub.rooms.entry(room.clone()).or_default().push(me);
            if let Some(s) = hub.sessions.get_mut(&me) {
                s.room = Some(room.clone());
            }
            let name = hub.sessions.get(&me).map(|s| s.name.clone()).unwrap_or_default();
            App::broadcast(&hub, &room, me, &ServerMsg::Joined { session: me, name });

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

        ClientMsg::Stats {
            loss_pct,
            jitter_ms,
            buf_ms,
            out_ms,
            rx_pps,
            play_fps,
            late_pps,
            resyncs,
            expand_pps,
            tx_pps,
            mic_db,
            dev_in_ms,
            in_ring_ms,
            enc_ms,
            rtt_ms,
            jb_ms,
            dev_out_ms,
            tx_path_ms,
            rx_path_ms,
            path,
        } => {
            let name = hub.sessions.get(&me).map(|s| s.name.as_str()).unwrap_or("?");

            eprintln!(
                "[stats] {name}(s{me}) path={path} tx={tx_pps}/s mic={mic_db:.0}dB \
                 rx={rx_pps}/s play={play_fps}/s loss={loss_pct:.1}% late={late_pps}/s \
                 expand={expand_pps}/s resync={resyncs} jitter={jitter_ms:.1}ms \
                 buf={buf_ms}ms out={out_ms}ms                  rtt={rtt_ms:.1}ms devin={dev_in_ms:.1}ms ring={in_ring_ms:.2}ms enc={enc_ms:.2}ms                  jb={jb_ms:.1}ms devout={dev_out_ms:.1}ms txpath={tx_path_ms:.1}ms rxpath={rx_path_ms:.1}ms"
            );
        }

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

fn forward(hub: &Hub, from: SessionId, to: SessionId, msg: ServerMsg) {
    let room_of = |id: SessionId| hub.sessions.get(&id).and_then(|s| s.room.clone());
    if room_of(from).is_some() && room_of(from) == room_of(to) {
        App::send(hub, to, msg);
    }
}
