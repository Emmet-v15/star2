//! star2 call engine: mic -> Opus -> direct UDP -> jitter buffer -> speakers.
//!
//! One call, two people, no relay. The signal server only brokers the hole punch;
//! once the punch confirms, audio flows peer-to-peer and the server sees nothing.
//!
//! Threading model (all real threads spawned by [`start_call`]):
//!   * **control** - a tokio runtime driving the signaling WebSocket
//!   * **recv**    - blocking `recv_from` on the media socket; classifies datagrams
//!   * **punch**   - probe bursts + the direct-path stall watchdog
//!   * **encode**  - 5 ms framing, Opus encode, `send_to` the peer
//!   * **playout** - jitter buffer -> decode/PLC -> the master output ring
//!   * cpal's own input/output callback threads

use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use audiopus::coder::Encoder;
use audiopus::{Application, Bitrate, Channels, SampleRate};
use cpal::traits::{DeviceTrait, StreamTrait};
use futures_util::{SinkExt, StreamExt};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;
use star2_proto::{
    flags, ClientMsg, MediaHeader, ServerMsg, SessionId, MEDIA_HEADER_LEN, PROTO_VERSION,
    PUNCH_PROBE,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

mod audio;
mod p2p;
mod playout;

use audio::{buffer_size_for, pick_config, pick_device, Resampler};
use p2p::*;
use playout::*;

// --- Audio framing ---
pub const SR: u32 = 48_000;
const FRAME_MS: u32 = 5;
const FRAME: usize = (SR as usize / 1000) * FRAME_MS as usize; // samples PER CHANNEL
const STEREO_FRAME: usize = FRAME * 2; // interleaved L/R - the internal mix layout

// --- Output pre-buffer ---
#[cfg(target_os = "android")]
const OUT_FLOOR_FRAMES: usize = (30 / FRAME_MS) as usize;
#[cfg(not(target_os = "android"))]
const OUT_FLOOR_FRAMES: usize = (10 / FRAME_MS) as usize;
const OUT_MAX_FRAMES: usize = (120 / FRAME_MS) as usize;
const OUT_SHRINK_AFTER_S: u64 = 2;

// --- Jitter buffer ---
const MAX_FRAMES: usize = (300 / FRAME_MS) as usize; // hard cap on the adaptive buffer
const SHED_MARGIN: usize = (80 / FRAME_MS) as usize; // catch up beyond target + this
const JITTER_K: f64 = 3.0; // buffer depth = K x measured jitter percentile
const JITTER_MARGIN_MS: f64 = 8.0; // + safety margin
const RESYNC_CONCEAL: u32 = 500 / FRAME_MS; // ~0.5 s of solid PLC => force a re-lock
const JB_BINS: usize = 64; // histogram bins for the |jitter| distribution
const JB_BIN_MS: f64 = 4.0; // ms per bin -> 0..256 ms range, fine at the low end
const JB_DECAY_EVERY: u32 = 256; // halve the histogram every N observations
const JB_PCTILE: f64 = 0.97; // target the 97th percentile (latency-vs-loss knob)
const SPIKE_MULT: f64 = 3.0; // d > mult x mean|d| => spike (fast attack)
const SPIKE_MAX_MS: f64 = 400.0; // cap the transient target bump
const SPIKE_DECAY: f64 = 0.985; // per-observation decay of the spike bump

// --- P2P ---
const P2P_PROBE_INTERVAL: Duration = Duration::from_millis(200);
const P2P_PUNCH_TIMEOUT: Duration = Duration::from_secs(8);
/// No peer traffic for this long on an established direct path => the call is dead.
/// Generous because audio itself is the liveness signal and a muted peer still sends.
const P2P_DIRECT_DEAD: Duration = Duration::from_secs(5);
const P2P_MAX_CANDS: usize = 8;
const P2P_MAX_TXIDS: usize = 256;
const PKT_BUF: usize = 4096;

/// Log a line to stderr. The CLI is the UI, so there is nowhere else to put it.
pub fn log_line(line: String) {
    eprintln!("{line}");
}

/// Ask Windows for 1 ms timer granularity. The default (~15.6 ms) is three times our
/// frame, so `sleep(1ms)` overshoots badly: the output ring drains dry between wakeups
/// and the adaptive buffer compensates by growing, costing real mouth-to-ear latency.
/// Process-wide and left set for the process lifetime; harmless if it fails.
#[cfg(windows)]
fn sharpen_timer() {
    unsafe {
        let _ = windows::Win32::Media::timeBeginPeriod(1);
    }
}
#[cfg(not(windows))]
fn sharpen_timer() {}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CallConfig {
    /// Signal server WebSocket URL, e.g. `ws://127.0.0.1:9101` or `wss://star2.example`.
    pub url: String,
    pub room: String,
    pub name: String,
    pub token: String,
    /// Send 2 channels instead of 1. Receiving handles either, per-packet.
    pub stereo: bool,
    /// Opus bitrate in bits/sec.
    pub bitrate: i32,
    /// Substring match on the device name; empty = system default.
    pub input: String,
    pub output: String,
    /// Device buffer request in ms; 0 = device default.
    pub dev_buf_ms: u32,
}

impl Default for CallConfig {
    fn default() -> Self {
        // Defaults point at the deployed server so a bare `star2` just works. The
        // token only gates the rendezvous, and anyone who can download the binary
        // has it anyway - it stops strangers stumbling in, not a real attacker.
        Self {
            url: "wss://star.v15.studio/star2".into(),
            room: "general".into(),
            name: "anon".into(),
            token: "ad7afaabdfe6a6636c3e3e478321039c".into(),
            stereo: false,
            bitrate: 128_000,
            input: String::new(),
            output: String::new(),
            dev_buf_ms: 0,
        }
    }
}

/// Things the engine tells the caller about. Deliberately coarse - this is an MVP.
#[derive(Debug, Clone)]
pub enum Event {
    Status(String),
    /// The direct path came up on this peer address.
    Direct(SocketAddr),
    /// The call ended and will not recover (punch failed, peer left, path died).
    Ended(String),
    /// Periodic 1 Hz readout while a call is live. `rx_pps` / `play_fps` should both
    /// sit at 1000/FRAME_MS (200); a gap between them is the loss, and which one is
    /// wrong says whether the sender or the playout clock is at fault.
    Stats { jitter_ms: f64, target_ms: f64, loss_pct: f32, out_ms: f64, rx_pps: u64, play_fps: u64 },
}

/// Owns the running call. Dropping it stops every thread.
///
/// Not `Send`: it holds the cpal streams, which must be dropped on the thread that
/// created them. Keep it on the thread that called [`start_call`].
pub struct CallHandle {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    /// Kept alive for the duration of the call; the audio callbacks read the rings.
    _streams: (cpal::Stream, cpal::Stream),
}

impl CallHandle {
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for CallHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Shared state every thread reaches into.
struct Shared {
    session: Mutex<Option<SessionId>>,
    /// Our server-reflexive `ip:port`, learned from the signal server's UDP responder.
    reflex: Mutex<Option<String>>,
    inbox: Mutex<HashMap<SessionId, SenderBuf>>,
    p2p: Mutex<P2pState>,
    route: MediaRoute,
    /// Queue of signaling messages for the control loop to send.
    ctrl_tx: Mutex<Option<UnboundedSender<ClientMsg>>>,
    stop: Arc<AtomicBool>,
}

impl Shared {
    fn send_ctrl(&self, m: ClientMsg) {
        if let Some(tx) = self.ctrl_tx.lock().unwrap().as_ref() {
            let _ = tx.send(m);
        }
    }
    fn my_session(&self) -> SessionId {
        self.session.lock().unwrap().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// start_call
// ---------------------------------------------------------------------------

pub fn start_call<F>(cfg: CallConfig, on_event: F) -> Result<CallHandle>
where
    F: Fn(Event) + Send + Sync + 'static,
{
    sharpen_timer();
    let on_event: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(on_event);
    let stop = Arc::new(AtomicBool::new(false));
    let send_ch = if cfg.stereo { 2 } else { 1 };

    // 1. Media socket. Every thread shares this one socket: the NAT mapping the peer
    //    punches is *this* socket's, so probing from anywhere else opens a hole nobody
    //    sends through.
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(Duration::from_millis(100)))?;
    let local_port = sock.local_addr()?.port();

    let shared = Arc::new(Shared {
        session: Mutex::new(None),
        reflex: Mutex::new(None),
        inbox: Mutex::new(HashMap::new()),
        p2p: Mutex::new(P2pState::new()),
        route: MediaRoute { dst: Mutex::new(None), allowed: Mutex::new(HashSet::new()) },
        ctrl_tx: Mutex::new(None),
        stop: stop.clone(),
    });

    // 2. Audio devices.
    let host = cpal::default_host();
    let input = pick_device(&host, &cfg.input, true)?;
    let output = pick_device(&host, &cfg.output, false)?;
    let in_cfg = pick_config(&input, true)?;
    let out_cfg = pick_config(&output, false)?;
    log_line(format!(
        "[engine] audio in={}Hz {}ch {:?} / out={}Hz {}ch {:?}",
        in_cfg.sample_rate().0,
        in_cfg.channels(),
        in_cfg.sample_format(),
        out_cfg.sample_rate().0,
        out_cfg.channels(),
        out_cfg.sample_format()
    ));
    if in_cfg.sample_format() != cpal::SampleFormat::F32
        || out_cfg.sample_format() != cpal::SampleFormat::F32
    {
        bail!("no F32 audio config (in={:?} out={:?})", in_cfg.sample_format(), out_cfg.sample_format());
    }
    let in_rate = in_cfg.sample_rate().0;
    let out_rate = out_cfg.sample_rate().0;
    // Capture is resampled to 48k below; output resampling isn't implemented, and
    // essentially every output device offers 48k, so this is a hard requirement.
    if out_rate != SR {
        bail!("output device must be 48 kHz (got {out_rate}Hz)");
    }
    let in_ch = in_cfg.channels() as usize;
    let out_ch = out_cfg.channels() as usize;

    let underruns = Arc::new(AtomicU64::new(0));
    let out_target = Arc::new(AtomicUsize::new(OUT_FLOOR_FRAMES * STEREO_FRAME));
    let out_fill = Arc::new(AtomicUsize::new(0));

    // Input ring holds interleaved `send_ch`; master holds interleaved stereo.
    let (mut in_prod, mut in_cons) = HeapRb::<f32>::new(SR as usize * 2).split();
    let (mut master_prod, mut master_cons) = HeapRb::<f32>::new(SR as usize * 2).split();

    let err_fn = |e| log_line(format!("[engine] audio stream error: {e}"));
    if in_rate != SR {
        log_line(format!("[engine] resampling mic {in_rate}Hz -> {SR}Hz"));
    }

    let in_stream = {
        let mut cfg2: cpal::StreamConfig = in_cfg.clone().into();
        cfg2.buffer_size = buffer_size_for(in_cfg.buffer_size(), cfg.dev_buf_ms);
        let mut resamp = (in_rate != SR).then(|| Resampler::new(in_rate, SR, send_ch));
        let mut dm: Vec<f32> = Vec::with_capacity(2048);
        let mut rs: Vec<f32> = Vec::with_capacity(2048);
        input.build_input_stream(
            &cfg2,
            move |data: &[f32], _| {
                dm.clear();
                let mut i = 0;
                while i + in_ch <= data.len() {
                    if send_ch == 1 {
                        // Take channel 0 rather than averaging: averaging halves the
                        // level when a mono mic drives only one channel of a 2ch
                        // capture and the other is silent - the common case.
                        dm.push(data[i]);
                    } else {
                        dm.push(data[i]);
                        dm.push(if in_ch >= 2 { data[i + 1] } else { data[i] });
                    }
                    i += in_ch;
                }
                let samples: &[f32] = match resamp.as_mut() {
                    Some(r) => {
                        rs.clear();
                        r.process(&dm, &mut rs);
                        &rs
                    }
                    None => &dm,
                };
                for &s in samples {
                    let _ = in_prod.try_push(s);
                }
            },
            err_fn,
            None,
        )?
    };

    let out_stream = {
        let mut cfg2: cpal::StreamConfig = out_cfg.clone().into();
        cfg2.buffer_size = buffer_size_for(out_cfg.buffer_size(), cfg.dev_buf_ms);
        let underruns = underruns.clone();
        output.build_output_stream(
            &cfg2,
            move |data: &mut [f32], _| {
                let mut i = 0;
                while i + out_ch <= data.len() {
                    // An empty pop means the buffer under-ran (we emit silence); the
                    // playout thread watches this count and deepens the buffer.
                    let l = match master_cons.try_pop() {
                        Some(v) => v,
                        None => {
                            underruns.fetch_add(1, Ordering::Relaxed);
                            0.0
                        }
                    };
                    let r = master_cons.try_pop().unwrap_or(0.0);
                    if out_ch == 1 {
                        data[i] = 0.5 * (l + r);
                    } else {
                        data[i] = l;
                        data[i + 1] = r;
                        for c in 2..out_ch {
                            data[i + c] = 0.0;
                        }
                    }
                    i += out_ch;
                }
            },
            err_fn,
            None,
        )?
    };
    in_stream.play()?;
    out_stream.play()?;

    let mut threads = Vec::new();

    // 3. Control thread: its own tokio runtime driving the signaling WebSocket.
    threads.push({
        let shared = shared.clone();
        let on_event = on_event.clone();
        let cfg = cfg.clone();
        let sock = sock.try_clone()?;
        std::thread::Builder::new().name("control".into()).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    on_event(Event::Ended(format!("runtime: {e}")));
                    return;
                }
            };
            if let Err(e) = rt.block_on(control_loop(&cfg, &shared, &sock, local_port, &on_event)) {
                if !shared.stop.load(Ordering::Relaxed) {
                    on_event(Event::Ended(format!("signaling: {e}")));
                }
            }
        })?
    });

    // 4. Recv thread: classify every inbound datagram.
    threads.push({
        let shared = shared.clone();
        let on_event = on_event.clone();
        let sock = sock.try_clone()?;
        std::thread::Builder::new().name("recv".into()).spawn(move || {
            recv_loop(&shared, &sock, &on_event);
        })?
    });

    // 5. Punch thread: probe bursts, punch timeout, direct-path stall watchdog.
    threads.push({
        let shared = shared.clone();
        let on_event = on_event.clone();
        let sock = sock.try_clone()?;
        std::thread::Builder::new().name("punch".into()).spawn(move || {
            punch_loop(&shared, &sock, &on_event);
        })?
    });

    // 6. Encode thread: 5 ms frames -> Opus -> the peer.
    threads.push({
        let shared = shared.clone();
        let on_event = on_event.clone();
        let sock = sock.try_clone()?;
        let bitrate = cfg.bitrate;
        std::thread::Builder::new().name("encode".into()).spawn(move || {
            if let Err(e) = encode_loop(&shared, &sock, send_ch, bitrate, &mut in_cons) {
                on_event(Event::Ended(format!("encoder: {e}")));
            }
        })?
    });

    // 7. Playout thread: jitter buffer -> decode/PLC -> master ring.
    threads.push({
        let shared = shared.clone();
        let on_event = on_event.clone();
        std::thread::Builder::new().name("playout".into()).spawn(move || {
            if let Err(e) = playout_loop(&shared, &mut master_prod, &out_target, &out_fill, &underruns, &on_event) {
                on_event(Event::Ended(format!("playout: {e}")));
            }
        })?
    });

    Ok(CallHandle { stop, threads, _streams: (in_stream, out_stream) })
}

// ---------------------------------------------------------------------------
// Control (signaling) loop
// ---------------------------------------------------------------------------

async fn control_loop(
    cfg: &CallConfig,
    shared: &Arc<Shared>,
    sock: &UdpSocket,
    local_port: u16,
    on_event: &Arc<dyn Fn(Event) + Send + Sync>,
) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (ws, _) = tokio_tungstenite::connect_async(&cfg.url).await.context("connect")?;
    let (mut tx_ws, mut rx_ws) = ws.split();
    let (tx, mut rx) = unbounded_channel::<ClientMsg>();
    *shared.ctrl_tx.lock().unwrap() = Some(tx.clone());

    tx.send(ClientMsg::Hello {
        name: cfg.name.clone(),
        ver: PROTO_VERSION as u32,
        token: cfg.token.clone(),
    })?;

    let mut joined = false;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        tokio::select! {
            // Outbound: drain the queue any thread can push to.
            Some(m) = rx.recv() => {
                tx_ws.send(tokio_tungstenite::tungstenite::Message::Text(
                    serde_json::to_string(&m)?.into(),
                )).await?;
            }
            msg = rx_ws.next() => {
                let Some(msg) = msg else { bail!("signal server closed") };
                let tokio_tungstenite::tungstenite::Message::Text(txt) = msg? else { continue };
                let Ok(sm) = serde_json::from_str::<ServerMsg>(&txt) else { continue };
                match sm {
                    ServerMsg::Welcome { session, reflex } => {
                        *shared.session.lock().unwrap() = Some(session);
                        on_event(Event::Status(format!("session {session}, reflex via {reflex}")));
                        // Learn our NAT mapping before offering candidates, otherwise the
                        // offer carries only a LAN address and the punch can't work.
                        discover_reflex(shared, sock, &reflex, session).await;
                        let mut cands = Vec::new();
                        if let Some(r) = shared.reflex.lock().unwrap().clone() {
                            cands.push(r);
                        }
                        if let Some(ip) = lan_ip() {
                            cands.push(SocketAddr::new(ip, local_port).to_string());
                        }
                        if cands.is_empty() {
                            bail!("no usable candidates (no reflexive address, no LAN address)");
                        }
                        on_event(Event::Status(format!("candidates: {}", cands.join(", "))));
                        shared.p2p.lock().unwrap().my_cands = cands;
                        tx.send(ClientMsg::Join { room: cfg.room.clone() })?;
                        joined = true;
                    }
                    ServerMsg::Room { room, members } => {
                        on_event(Event::Status(if members.len() < 2 {
                            format!("in room {room:?} alone - waiting for your peer to join the same room")
                        } else {
                            format!("room {room:?}: {} members", members.len())
                        }));
                        let ids: HashSet<SessionId> = members.iter().map(|m| m.session).collect();
                        on_membership(shared, &ids, on_event);
                    }
                    ServerMsg::Joined { session, name } => {
                        on_event(Event::Status(format!("{name} joined")));
                        let me = shared.my_session();
                        on_membership(shared, &HashSet::from([me, session]), on_event);
                    }
                    ServerMsg::Left { session } => {
                        let peer = shared.p2p.lock().unwrap().peer_session;
                        if peer == Some(session) {
                            p2p_teardown(&shared.p2p, &shared.route, "peer left");
                            on_event(Event::Ended("peer left".into()));
                        }
                    }
                    ServerMsg::P2pOffer { from, nonce, cands } => {
                        let mut s = shared.p2p.lock().unwrap();
                        if s.peer_session != Some(from) {
                            continue;
                        }
                        s.remote_nonce = Some(nonce);
                        for c in cands.iter().filter_map(|c| c.parse().ok()) {
                            s.merge_cand(c);
                        }
                        s.phase = P2pPhase::Punching;
                        s.started = Instant::now();
                        s.last_peer_rx = Instant::now();
                        let (my_nonce, my_cands) = (s.local_nonce, s.my_cands.clone());
                        drop(s);
                        tx.send(ClientMsg::P2pAnswer { to: from, nonce: my_nonce, cands: my_cands })?;
                        on_event(Event::Status("punching (answered offer)".into()));
                    }
                    ServerMsg::P2pAnswer { from, nonce, cands } => {
                        let mut s = shared.p2p.lock().unwrap();
                        if s.peer_session != Some(from) {
                            continue;
                        }
                        s.remote_nonce = Some(nonce);
                        for c in cands.iter().filter_map(|c| c.parse().ok()) {
                            s.merge_cand(c);
                        }
                        s.last_peer_rx = Instant::now();
                    }
                    ServerMsg::P2pCandidate { from, cand } => {
                        let mut s = shared.p2p.lock().unwrap();
                        if s.peer_session == Some(from) {
                            if let Ok(a) = cand.parse() {
                                s.merge_cand(a);
                            }
                        }
                    }
                    ServerMsg::P2pAbort { from } => {
                        if shared.p2p.lock().unwrap().peer_session == Some(from) {
                            p2p_teardown(&shared.p2p, &shared.route, "peer aborted");
                            on_event(Event::Ended("peer gave up on the direct path".into()));
                        }
                    }
                    ServerMsg::Error { msg } => bail!("server: {msg}"),
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                if joined && shared.stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
            }
        }
    }
}

/// Send REFLEX probes to the signal server's UDP port until the recv thread reports
/// our observed address, or we give up. This is the whole of our STUN.
async fn discover_reflex(shared: &Arc<Shared>, sock: &UdpSocket, reflex: &str, session: SessionId) {
    let Ok(mut addrs) = reflex.to_socket_addrs() else { return };
    let Some(dst) = addrs.next() else { return };
    let mut probe = [0u8; MEDIA_HEADER_LEN];
    MediaHeader::new(session, 0, 0, flags::REFLEX | flags::KEEPALIVE).encode(&mut probe);
    for _ in 0..20 {
        if shared.reflex.lock().unwrap().is_some() {
            return;
        }
        let _ = sock.send_to(&probe, dst);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    log_line("[engine] no reflexive address (signal server UDP unreachable?)".into());
}

/// Roster changed: start or tear down the 1:1 negotiation. Exactly-two-with-a-new-peer
/// resets state; the lower session id is the controller and sends the offer (glare-free).
fn on_membership(
    shared: &Arc<Shared>,
    members: &HashSet<SessionId>,
    on_event: &Arc<dyn Fn(Event) + Send + Sync>,
) {
    let me = shared.my_session();
    if members.len() == 2 && members.contains(&me) {
        let Some(peer) = members.iter().copied().find(|&m| m != me) else { return };
        let mut s = shared.p2p.lock().unwrap();
        if s.peer_session == Some(peer) {
            return; // already negotiating with this exact peer
        }
        // Fresh negotiation: new nonce, cleared cands/txids. A stale nonce from a prior
        // peer must never authorize this one.
        let my_cands = s.my_cands.clone();
        *s = P2pState::new();
        s.peer_session = Some(peer);
        s.local_nonce = rand_u64();
        s.my_cands = my_cands.clone();
        s.last_peer_rx = Instant::now();
        if me < peer {
            s.phase = P2pPhase::Punching;
            s.started = Instant::now();
            let nonce = s.local_nonce;
            drop(s);
            shared.send_ctrl(ClientMsg::P2pOffer { to: peer, nonce, cands: my_cands });
            on_event(Event::Status(format!("punching peer {peer} (controller)")));
        }
    } else if members.len() > 2 {
        // This is a 1:1 program; a third party would need a mixer we don't have.
        on_event(Event::Ended("room has more than 2 members".into()));
    }
}

// ---------------------------------------------------------------------------
// Recv loop
// ---------------------------------------------------------------------------

fn recv_loop(shared: &Arc<Shared>, sock: &UdpSocket, on_event: &Arc<dyn Fn(Event) + Send + Sync>) {
    let mut buf = vec![0u8; PKT_BUF];
    let base = Instant::now();
    while !shared.stop.load(Ordering::Relaxed) {
        let (n, src) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(_) => continue, // read timeout - loop so we can observe `stop`
        };
        let Some(h) = MediaHeader::decode(&buf[..n]) else { continue };
        if h.version != PROTO_VERSION {
            continue;
        }
        let payload = &buf[MEDIA_HEADER_LEN..n];

        // Reflexive reply from the signal server: our own NAT mapping.
        if h.is_reflex() {
            if let Ok(addr) = std::str::from_utf8(payload) {
                let mut slot = shared.reflex.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(addr.to_string());
                    log_line(format!("[engine] reflexive addr {addr}"));
                }
            }
            continue;
        }

        // Punch probes authorize themselves by nonce, so they're handled before the
        // source allowlist - that allowlist is what they exist to populate.
        if h.is_punch() {
            // Only fires on the transition into Direct, not on every keepalive probe.
            if let Some(peer) =
                handle_punch(shared.my_session(), &shared.p2p, &shared.route, sock, src, h.session, payload)
            {
                on_event(Event::Direct(peer));
            }
            continue;
        }

        // Everything else must come from the confirmed peer address.
        if !shared.route.allowed.lock().unwrap().contains(&src) {
            continue;
        }
        shared.p2p.lock().unwrap().last_peer_rx = Instant::now();
        if h.is_keepalive() {
            continue;
        }

        // Audio: observe jitter on arrival, then buffer by sequence number.
        let arr_ms = base.elapsed().as_secs_f64() * 1000.0;
        let mut inbox = shared.inbox.lock().unwrap();
        let sb = inbox.entry(h.session).or_default();
        if let (Some(la), Some(lt)) = (sb.last_arr_ms, sb.last_ts) {
            // d = |inter-arrival gap - inter-timestamp gap|, i.e. RFC3550 jitter.
            let d_arr = arr_ms - la;
            let d_ts = (h.timestamp.wrapping_sub(lt)) as f64 / (SR as f64 / 1000.0);
            sb.est.observe((d_arr - d_ts).abs());
        }
        sb.last_arr_ms = Some(arr_ms);
        sb.last_ts = Some(h.timestamp);
        sb.recv_count += 1;
        sb.insert_capped(h.seq, AudioPkt { flags: h.flags, data: payload.to_vec() });
    }
}

// ---------------------------------------------------------------------------
// Punch loop
// ---------------------------------------------------------------------------

fn punch_loop(shared: &Arc<Shared>, sock: &UdpSocket, on_event: &Arc<dyn Fn(Event) + Send + Sync>) {
    while !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(P2P_PROBE_INTERVAL);
        let me = shared.my_session();
        let mut s = shared.p2p.lock().unwrap();
        match s.phase {
            P2pPhase::Punching => {
                if s.started.elapsed() > P2P_PUNCH_TIMEOUT {
                    let peer = s.peer_session;
                    p2p_fail(&mut s, &shared.route, "punch timed out");
                    drop(s);
                    if let Some(p) = peer {
                        shared.send_ctrl(ClientMsg::P2pAbort { to: p });
                    }
                    on_event(Event::Ended(
                        "hole punch failed - no direct path (symmetric NAT/CGNAT?)".into(),
                    ));
                    continue;
                }
                // Probe every candidate; the ACK tells us which one actually works.
                let Some(rn) = s.remote_nonce else { continue };
                let cands = s.cands.clone();
                for c in cands {
                    let txid = s.new_txid();
                    send_punch(sock, me, PUNCH_PROBE, rn, txid, c, true);
                }
            }
            P2pPhase::Direct => {
                if s.last_peer_rx.elapsed() > P2P_DIRECT_DEAD {
                    let peer = s.peer_session;
                    p2p_fail(&mut s, &shared.route, "direct path went silent");
                    drop(s);
                    if let Some(p) = peer {
                        shared.send_ctrl(ClientMsg::P2pAbort { to: p });
                    }
                    on_event(Event::Ended("peer stopped responding".into()));
                    continue;
                }
                // Small keepalive probe holds the NAT mapping open when nobody talks.
                if let (Some(rn), Some(dst)) = (s.remote_nonce, *shared.route.dst.lock().unwrap()) {
                    let txid = s.new_txid();
                    send_punch(sock, me, PUNCH_PROBE, rn, txid, dst, false);
                }
            }
            P2pPhase::Idle | P2pPhase::Failed => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Encode loop
// ---------------------------------------------------------------------------

fn encode_loop(
    shared: &Arc<Shared>,
    sock: &UdpSocket,
    send_ch: usize,
    bitrate: i32,
    in_cons: &mut impl Consumer<Item = f32>,
) -> Result<()> {
    let ch = if send_ch == 2 { Channels::Stereo } else { Channels::Mono };
    let mut enc = Encoder::new(SampleRate::Hz48000, ch, Application::LowDelay)
        .context("create opus encoder")?;
    enc.set_bitrate(Bitrate::BitsPerSecond(bitrate)).context("opus bitrate")?;
    // In-band FEC lets the decoder rebuild a lost frame from the next one - it costs a
    // little bitrate and no latency, which is the right trade on a UDP path.
    let _ = enc.set_inband_fec(true);
    let _ = enc.set_packet_loss_perc(5);

    let need = FRAME * send_ch;
    let mut pcm_f = vec![0.0f32; need];
    let mut pcm_i16 = vec![0i16; need];
    let mut dg = vec![0u8; MEDIA_HEADER_LEN + PKT_BUF];
    let base_flags = if send_ch == 2 { flags::STEREO } else { 0 };
    let (mut seq, mut ts) = (0u16, 0u32);

    while !shared.stop.load(Ordering::Relaxed) {
        if in_cons.occupied_len() < need {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        for s in pcm_f.iter_mut() {
            *s = in_cons.try_pop().unwrap_or(0.0);
        }
        // No direct path yet: keep draining the mic ring (so it can't overflow and
        // desync the capture clock) but send nothing - there is nowhere to send.
        let Some(dst) = *shared.route.dst.lock().unwrap() else {
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(FRAME as u32);
            continue;
        };
        for (i, &s) in pcm_f.iter().enumerate() {
            pcm_i16[i] = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        let session = shared.my_session();
        if let Ok(len) = enc.encode(&pcm_i16, &mut dg[MEDIA_HEADER_LEN..]) {
            MediaHeader::new(session, seq, ts, base_flags).encode(&mut dg[..MEDIA_HEADER_LEN]);
            let _ = sock.send_to(&dg[..MEDIA_HEADER_LEN + len], dst);
        }
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(FRAME as u32);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Playout loop
// ---------------------------------------------------------------------------

fn playout_loop(
    shared: &Arc<Shared>,
    master: &mut impl Producer<Item = f32>,
    out_target: &Arc<AtomicUsize>,
    out_fill: &Arc<AtomicUsize>,
    underruns: &Arc<AtomicU64>,
    on_event: &Arc<dyn Fn(Event) + Send + Sync>,
) -> Result<()> {
    let mut dec = DecState::new()?;
    let mut frame = vec![0.0f32; STEREO_FRAME];
    let mut scratch = vec![0i16; FRAME * 2];
    let mut last_under = underruns.load(Ordering::Relaxed);
    let mut clean_since = Instant::now();
    let mut clean_needed = OUT_SHRINK_AFTER_S;
    let mut last_stats = Instant::now();
    // Previous-window counters, so the readout is per-second rather than lifetime.
    let (mut win_played, mut win_concealed, mut win_recv) = (0u64, 0u64, 0u64);

    while !shared.stop.load(Ordering::Relaxed) {
        // FILL-DRIVEN pacing: the output device is the master clock. We emit a frame
        // only once the device ring has drained below its target depth, so playout
        // consumes at exactly the rate the hardware plays.
        //
        // A wall-clock tick cannot do this. Sleep granularity (~15.6 ms on Windows,
        // vs our 5 ms frame) makes the loop fall behind and then spin to catch up,
        // advancing the sequencer faster than packets arrive; `retain` then drops
        // those arrivals as stale and every slot conceals until the desync watchdog
        // fires. That sawtooth is what a steady double-digit loss rate looks like.
        if master.occupied_len() >= out_target.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }

        // ONE lock acquisition per iteration. Taking it twice let this thread - which
        // loops far faster than 200 Hz - monopolise the mutex and starve `recv_loop`,
        // so datagrams piled up in the socket buffer and were dropped by the kernel.
        // That showed up as a collapsed receive rate, not as network loss.
        let (outcome, sess) = {
            let mut inbox = shared.inbox.lock().unwrap();
            let Some((&sess, sb)) = inbox.iter_mut().next().map(|(k, v)| (k, v)) else {
                drop(inbox);
                std::thread::sleep(Duration::from_millis(2));
                continue;
            };
            let tgt_ms = sb.est.target_ms(JB_PCTILE, JITTER_K, JITTER_MARGIN_MS);
            let target = ((tgt_ms / FRAME_MS as f64).ceil() as usize).clamp(1, MAX_FRAMES);
            sb.target_frames = target;

            let o = dec.produce(&mut sb.pkts, target, &mut frame, &mut scratch);
            match o {
                Playout::Rendered => {
                    sb.played += 1;
                    dec.dead = 0;
                    dec.dead_recv = sb.recv_count;
                }
                Playout::Concealed => {
                    sb.played += 1;
                    sb.concealed += 1;
                    dec.dead += 1;
                    // Concealing for ages *while packets keep arriving* means the
                    // sequencer has run ahead of the stream: re-lock instead of
                    // playing silence forever.
                    if dec.dead > RESYNC_CONCEAL && sb.recv_count > dec.dead_recv {
                        log_line("[engine] jitter buffer desync - resyncing".into());
                        dec.resync(&mut sb.pkts);
                        dec.dead = 0;
                    }
                }
                Playout::Idle => {}
            }
            (o, sess)
        };

        if matches!(outcome, Playout::Idle) {
            // Still pre-buffering. Without this sleep the loop spins at full tilt
            // re-taking the lock, which is exactly what starved the recv thread.
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        for &s in frame.iter() {
            let _ = master.try_push(s);
        }
        out_fill.store(master.occupied_len(), Ordering::Relaxed);

        // Adaptive output pre-buffer: grow on under-run, shrink after a clean stretch.
        // The clean period doubles on every flap, so a marginal device settles instead
        // of oscillating between two depths forever.
        let under = underruns.load(Ordering::Relaxed);
        let mut tgt = out_target.load(Ordering::Relaxed);
        if under > last_under {
            last_under = under;
            if tgt < OUT_MAX_FRAMES * STEREO_FRAME {
                tgt += STEREO_FRAME;
                out_target.store(tgt, Ordering::Relaxed);
                clean_needed = (clean_needed * 2).min(64);
                log_line(format!("[engine] output buffer -> {} ms (under-runs)", tgt / STEREO_FRAME * FRAME_MS as usize));
            }
            clean_since = Instant::now();
        } else if clean_since.elapsed().as_secs() >= clean_needed
            && tgt > OUT_FLOOR_FRAMES * STEREO_FRAME
        {
            tgt -= STEREO_FRAME;
            out_target.store(tgt, Ordering::Relaxed);
            clean_since = Instant::now();
        }

        if last_stats.elapsed() >= Duration::from_secs(1) {
            let secs = last_stats.elapsed().as_secs_f64();
            last_stats = Instant::now();
            let inbox = shared.inbox.lock().unwrap();
            if let Some(sb) = inbox.get(&sess) {
                // Windowed, not cumulative: a lifetime average hides recovery, and
                // startup pre-buffering would dominate it forever.
                let played = sb.played - win_played;
                let concealed = sb.concealed - win_concealed;
                let rx = sb.recv_count - win_recv;
                win_played = sb.played;
                win_concealed = sb.concealed;
                win_recv = sb.recv_count;
                let loss = if played > 0 { concealed as f32 / played as f32 * 100.0 } else { 0.0 };
                on_event(Event::Stats {
                    jitter_ms: sb.est.mean_abs,
                    target_ms: sb.target_frames as f64 * FRAME_MS as f64,
                    loss_pct: loss,
                    out_ms: out_fill.load(Ordering::Relaxed) as f64 / STEREO_FRAME as f64
                        * FRAME_MS as f64,
                    rx_pps: (rx as f64 / secs).round() as u64,
                    play_fps: (played as f64 / secs).round() as u64,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_math() {
        assert_eq!(FRAME, 240); // 5 ms @ 48 kHz
        assert_eq!(STEREO_FRAME, 480);
    }

    /// A near-perfect link should still keep a small buffer, not zero, or every
    /// microscopic reordering becomes an audible conceal.
    #[test]
    fn calm_link_targets_a_small_buffer() {
        let mut e = DelayEstimator::default();
        for _ in 0..500 {
            e.observe(0.2);
        }
        let ms = e.target_ms(JB_PCTILE, JITTER_K, JITTER_MARGIN_MS);
        assert!(ms >= JITTER_MARGIN_MS, "got {ms}");
        assert!(ms < 30.0, "calm link should not hoard latency, got {ms}");
    }
}
