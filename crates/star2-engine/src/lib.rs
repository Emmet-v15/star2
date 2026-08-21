use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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

pub const SR: u32 = 48_000;
const FRAME_MS: u32 = 5;
const FRAME: usize = (SR as usize / 1000) * FRAME_MS as usize;
const STEREO_FRAME: usize = FRAME * 2;

#[cfg(target_os = "android")]
const OUT_FLOOR_FRAMES: usize = (30 / FRAME_MS) as usize;
#[cfg(not(target_os = "android"))]
const OUT_FLOOR_FRAMES: usize = (10 / FRAME_MS) as usize;
const OUT_MAX_FRAMES: usize = (120 / FRAME_MS) as usize;
const OUT_SHRINK_AFTER_S: u64 = 2;

const MAX_FRAMES: usize = (300 / FRAME_MS) as usize;
const SHED_MARGIN: usize = (80 / FRAME_MS) as usize;
const CONTRACT_EVERY: u32 = 1000 / FRAME_MS;
const JITTER_K: f64 = 3.0;
const JITTER_MARGIN_MS: f64 = 8.0;
const RESYNC_CONCEAL: u32 = 500 / FRAME_MS;
const JB_BINS: usize = 64;
const JB_BIN_MS: f64 = 4.0;
const JB_DECAY_EVERY: u32 = 256;
const JB_PCTILE: f64 = 0.97;
const SPIKE_MULT: f64 = 3.0;
const SPIKE_MAX_MS: f64 = 400.0;
const SPIKE_DECAY: f64 = 0.985;

const P2P_PROBE_INTERVAL: Duration = Duration::from_millis(200);
const P2P_PUNCH_TIMEOUT: Duration = Duration::from_secs(8);

const P2P_DIRECT_DEAD: Duration = Duration::from_secs(5);
const P2P_MAX_CANDS: usize = 8;
const P2P_MAX_TXIDS: usize = 256;
const PKT_BUF: usize = 4096;

pub fn log_line(line: String) {
    eprintln!("{line}");
}

#[cfg(windows)]
fn sharpen_timer() {
    unsafe {
        let _ = windows::Win32::Media::timeBeginPeriod(1);
    }
}
#[cfg(not(windows))]
fn sharpen_timer() {}

#[derive(Debug, Clone)]
pub struct CallConfig {

    pub url: String,
    pub room: String,
    pub name: String,
    pub token: String,

    pub stereo: bool,

    pub bitrate: i32,

    pub input: String,
    pub output: String,

    pub dev_buf_ms: u32,
}

impl Default for CallConfig {
    fn default() -> Self {

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

#[derive(Debug, Clone)]
pub enum Event {
    Status(String),

    Direct(SocketAddr),

    Ended(String),

    Stats { jitter_ms: f64, target_ms: f64, loss_pct: f32, out_ms: f64, rx_pps: u64, play_fps: u64 },
}

pub struct CallHandle {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,

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

struct Shared {
    session: Mutex<Option<SessionId>>,

    reflex: Mutex<Option<String>>,
    inbox: Mutex<HashMap<SessionId, SenderBuf>>,
    p2p: Mutex<P2pState>,
    route: MediaRoute,

    members: Mutex<HashSet<SessionId>>,

    ctrl_tx: Mutex<Option<UnboundedSender<ClientMsg>>>,

    tx_pkts: AtomicU64,

    mic_peak: AtomicU32,

    dev_in_us: AtomicU32,
    dev_out_us: AtomicU32,
    ring_sum_us: AtomicU64,
    ring_n: AtomicU64,
    enc_sum_us: AtomicU64,
    enc_n: AtomicU64,
    stop: Arc<AtomicBool>,
}

fn go_idle(shared: &Arc<Shared>, why: &str) {
    p2p_teardown(&shared.p2p, &shared.route, why);
    shared.inbox.lock().unwrap().clear();
}

fn to_dbfs(peak: f32) -> f32 {
    if peak <= 1e-5 {
        -99.0
    } else {
        20.0 * peak.log10()
    }
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

pub fn start_call<F>(cfg: CallConfig, on_event: F) -> Result<CallHandle>
where
    F: Fn(Event) + Send + Sync + 'static,
{
    sharpen_timer();
    let on_event: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(on_event);
    let stop = Arc::new(AtomicBool::new(false));
    let send_ch = if cfg.stereo { 2 } else { 1 };

    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(Duration::from_millis(100)))?;
    let local_port = sock.local_addr()?.port();

    let shared = Arc::new(Shared {
        session: Mutex::new(None),
        reflex: Mutex::new(None),
        inbox: Mutex::new(HashMap::new()),
        p2p: Mutex::new(P2pState::new()),
        route: MediaRoute { dst: Mutex::new(None), allowed: Mutex::new(HashSet::new()) },
        members: Mutex::new(HashSet::new()),
        ctrl_tx: Mutex::new(None),
        tx_pkts: AtomicU64::new(0),
        mic_peak: AtomicU32::new(0),
        dev_in_us: AtomicU32::new(0),
        dev_out_us: AtomicU32::new(0),
        ring_sum_us: AtomicU64::new(0),
        ring_n: AtomicU64::new(0),
        enc_sum_us: AtomicU64::new(0),
        enc_n: AtomicU64::new(0),
        stop: stop.clone(),
    });

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

    if out_rate != SR {
        bail!("output device must be 48 kHz (got {out_rate}Hz)");
    }
    let in_ch = in_cfg.channels() as usize;
    let out_ch = out_cfg.channels() as usize;

    let underruns = Arc::new(AtomicU64::new(0));
    let out_target = Arc::new(AtomicUsize::new(OUT_FLOOR_FRAMES * STEREO_FRAME));
    let out_fill = Arc::new(AtomicUsize::new(0));

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
        let sh_in = shared.clone();
        input.build_input_stream(
            &cfg2,
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                let t = info.timestamp();
                if let Some(d) = t.callback.duration_since(&t.capture) {
                    sh_in.dev_in_us.store(d.as_micros() as u32, Ordering::Relaxed);
                }
                dm.clear();
                let mut i = 0;
                while i + in_ch <= data.len() {
                    if send_ch == 1 {

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
        let sh_out = shared.clone();
        output.build_output_stream(
            &cfg2,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                let t = info.timestamp();
                if let Some(d) = t.playback.duration_since(&t.callback) {
                    sh_out.dev_out_us.store(d.as_micros() as u32, Ordering::Relaxed);
                }
                let mut i = 0;
                while i + out_ch <= data.len() {

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
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }

                if shared.p2p.lock().unwrap().phase == P2pPhase::Direct {
                    on_event(Event::Status(format!(
                        "signalling lost ({e}) - call continues on the direct path"
                    )));
                } else {
                    on_event(Event::Ended(format!("signaling: {e}")));
                }
            }
        })?
    });

    threads.push({
        let shared = shared.clone();
        let on_event = on_event.clone();
        let sock = sock.try_clone()?;
        std::thread::Builder::new().name("recv".into()).spawn(move || {
            recv_loop(&shared, &sock, &on_event);
        })?
    });

    threads.push({
        let shared = shared.clone();
        let on_event = on_event.clone();
        let sock = sock.try_clone()?;
        std::thread::Builder::new().name("punch".into()).spawn(move || {
            punch_loop(&shared, &sock, &on_event);
        })?
    });

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

                        let ids = {
                            let mut m = shared.members.lock().unwrap();
                            m.insert(shared.my_session());
                            m.insert(session);
                            m.clone()
                        };
                        on_membership(shared, &ids, on_event);
                    }
                    ServerMsg::Left { session } => {
                        let (peer, phase) = {
                            let s = shared.p2p.lock().unwrap();
                            (s.peer_session, s.phase)
                        };

                        if peer == Some(session) && phase == P2pPhase::Direct {
                            on_event(Event::Status(
                                "relay says peer left, but the direct path is up - ignoring".into(),
                            ));
                        } else if peer == Some(session) {

                            go_idle(&shared, "peer left");
                            on_event(Event::Status("peer left - waiting for them to rejoin".into()));
                        }

                        let ids = {
                            let mut m = shared.members.lock().unwrap();
                            m.remove(&session);
                            m.clone()
                        };
                        on_membership(shared, &ids, on_event);
                    }
                    ServerMsg::P2pOffer { from, nonce, cands } => {

                        let in_room = {
                            let m = shared.members.lock().unwrap();
                            m.len() == 2 && m.contains(&from)
                        };
                        let mut s = shared.p2p.lock().unwrap();
                        if s.peer_session != Some(from) {
                            if !in_room {
                                continue;
                            }
                            s.peer_session = Some(from);
                            shared.inbox.lock().unwrap().clear();
                            if s.local_nonce == 0 {
                                s.local_nonce = rand_u64();
                            }
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

                            go_idle(&shared, "peer aborted");
                            on_event(Event::Status(
                                "peer gave up on the direct path - waiting".into(),
                            ));
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

fn on_membership(
    shared: &Arc<Shared>,
    members: &HashSet<SessionId>,
    on_event: &Arc<dyn Fn(Event) + Send + Sync>,
) {

    *shared.members.lock().unwrap() = members.clone();
    let me = shared.my_session();
    if members.len() == 2 && members.contains(&me) {
        let Some(peer) = members.iter().copied().find(|&m| m != me) else { return };
        let mut s = shared.p2p.lock().unwrap();
        if s.peer_session == Some(peer) {
            return;
        }

        let my_cands = s.my_cands.clone();
        *s = P2pState::new();
        shared.inbox.lock().unwrap().clear();
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

        go_idle(shared, "room has more than 2 members");
        on_event(Event::Status(format!(
            "room has {} members - 1:1 only, waiting for it to clear",
            members.len()
        )));
    }
}

fn recv_loop(shared: &Arc<Shared>, sock: &UdpSocket, on_event: &Arc<dyn Fn(Event) + Send + Sync>) {
    let mut buf = vec![0u8; PKT_BUF];
    let base = Instant::now();
    while !shared.stop.load(Ordering::Relaxed) {
        let (n, src) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(h) = MediaHeader::decode(&buf[..n]) else { continue };
        if h.version != PROTO_VERSION {
            continue;
        }
        let payload = &buf[MEDIA_HEADER_LEN..n];

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

        if h.is_punch() {

            if let Some(peer) =
                handle_punch(shared.my_session(), &shared.p2p, &shared.route, sock, src, h.session, payload)
            {
                on_event(Event::Direct(peer));
            }
            continue;
        }

        if !shared.route.allowed.lock().unwrap().contains(&src) {
            continue;
        }
        shared.p2p.lock().unwrap().last_peer_rx = Instant::now();
        if h.is_keepalive() {
            continue;
        }

        let arr_ms = base.elapsed().as_secs_f64() * 1000.0;
        let mut inbox = shared.inbox.lock().unwrap();
        let sb = inbox.entry(h.session).or_default();
        if let (Some(la), Some(lt)) = (sb.last_arr_ms, sb.last_ts) {

            let d_arr = arr_ms - la;
            let d_ts = (h.timestamp.wrapping_sub(lt)) as f64 / (SR as f64 / 1000.0);
            sb.est.observe((d_arr - d_ts).abs());
        }
        sb.last_arr_ms = Some(arr_ms);
        sb.last_ts = Some(h.timestamp);
        sb.recv_count += 1;
        sb.insert_capped(h.seq, AudioPkt { flags: h.flags, data: payload.to_vec(), arr: Instant::now() });
    }
}

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

                if let (Some(rn), Some(dst)) = (s.remote_nonce, *shared.route.dst.lock().unwrap()) {
                    let txid = s.new_txid();
                    send_punch(sock, me, PUNCH_PROBE, rn, txid, dst, false);
                }
            }

            P2pPhase::Idle | P2pPhase::Failed => {}
        }
    }
}

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
        let ring_us = (in_cons.occupied_len() as u64 * 1_000_000)
            / (SR as u64 * send_ch as u64);
        shared.ring_sum_us.fetch_add(ring_us, Ordering::Relaxed);
        shared.ring_n.fetch_add(1, Ordering::Relaxed);
        let enc_start = Instant::now();
        let mut peak = 0.0f32;
        for s in pcm_f.iter_mut() {
            *s = in_cons.try_pop().unwrap_or(0.0);
            peak = peak.max(s.abs());
        }

        let prev = f32::from_bits(shared.mic_peak.load(Ordering::Relaxed));
        shared.mic_peak.store(peak.max(prev).to_bits(), Ordering::Relaxed);

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
            if sock.send_to(&dg[..MEDIA_HEADER_LEN + len], dst).is_ok() {
                shared.tx_pkts.fetch_add(1, Ordering::Relaxed);
                shared.enc_sum_us.fetch_add(enc_start.elapsed().as_micros() as u64, Ordering::Relaxed);
                shared.enc_n.fetch_add(1, Ordering::Relaxed);
            }
        }
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(FRAME as u32);
    }
    Ok(())
}

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

    let (mut win_played, mut win_concealed, mut win_recv) = (0u64, 0u64, 0u64);
    let mut win_contract = 0u64;
    let (mut win_late, mut win_resync, mut win_expand) = (0u64, 0u64, 0u64);

    while !shared.stop.load(Ordering::Relaxed) {

        if master.occupied_len() >= out_target.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }

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

                    if dec.dead > RESYNC_CONCEAL && sb.recv_count > dec.dead_recv {
                        log_line("[engine] jitter buffer desync - resyncing".into());
                        dec.resync(&mut sb.pkts);
                        dec.dead = 0;
                    }
                }

                Playout::Expanded => sb.expanded += 1,
                Playout::Idle => {}
            }
            (o, sess)
        };

        if matches!(outcome, Playout::Idle) {

            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        for &s in frame.iter() {
            let _ = master.try_push(s);
        }
        out_fill.store(master.occupied_len(), Ordering::Relaxed);

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

            clean_needed = (clean_needed / 2).max(OUT_SHRINK_AFTER_S);
            log_line(format!(
                "[engine] output buffer -> {} ms (clean; shrinking)",
                tgt / STEREO_FRAME * FRAME_MS as usize
            ));
        }

        if last_stats.elapsed() >= Duration::from_secs(1) {
            let secs = last_stats.elapsed().as_secs_f64();
            last_stats = Instant::now();

            let tx = shared.tx_pkts.swap(0, Ordering::Relaxed);
            let tx_pps = (tx as f64 / secs).round() as u32;
            let mic_db = to_dbfs(f32::from_bits(shared.mic_peak.swap(0, Ordering::Relaxed)));
            let mean_ms = |sum: u64, n: u64| if n > 0 { sum as f32 / n as f32 / 1000.0 } else { 0.0 };
            let dev_in_ms = shared.dev_in_us.load(Ordering::Relaxed) as f32 / 1000.0;
            let dev_out_ms = shared.dev_out_us.load(Ordering::Relaxed) as f32 / 1000.0;
            let in_ring_ms = mean_ms(
                shared.ring_sum_us.swap(0, Ordering::Relaxed),
                shared.ring_n.swap(0, Ordering::Relaxed),
            );
            let enc_ms = mean_ms(
                shared.enc_sum_us.swap(0, Ordering::Relaxed),
                shared.enc_n.swap(0, Ordering::Relaxed),
            );
            let rtt_ms = {
                let mut p = shared.p2p.lock().unwrap();
                let v = p.rtt_us.map(|u| u as f32 / 1000.0).unwrap_or(0.0);
                p.rtt_us = None;
                v
            };
            let jb_ms = mean_ms(
                std::mem::take(&mut dec.jb_sum_us),
                std::mem::take(&mut dec.jb_n),
            );
            let inbox = shared.inbox.lock().unwrap();
            if let Some(sb) = inbox.get(&sess) {

                let played = sb.played - win_played;
                let concealed = sb.concealed - win_concealed;
                let rx = sb.recv_count - win_recv;
                win_played = sb.played;
                win_concealed = sb.concealed;
                win_recv = sb.recv_count;
                let loss = if played > 0 { concealed as f32 / played as f32 * 100.0 } else { 0.0 };
                let buf_ms = sb.target_frames as f64 * FRAME_MS as f64;
                let out_ms =
                    out_fill.load(Ordering::Relaxed) as f64 / STEREO_FRAME as f64 * FRAME_MS as f64;
                let rx_pps = (rx as f64 / secs).round() as u64;
                let play_fps = (played as f64 / secs).round() as u64;
                on_event(Event::Stats {
                    jitter_ms: sb.est.mean_abs,
                    target_ms: buf_ms,
                    loss_pct: loss,
                    out_ms,
                    rx_pps,
                    play_fps,
                });

                let path = match shared.p2p.lock().unwrap().phase {
                    P2pPhase::Direct => "direct",
                    P2pPhase::Punching => "punching",
                    P2pPhase::Failed => "failed",
                    P2pPhase::Idle => "idle",
                };
                let late = dec.late_dropped - win_late;
                let resyncs = dec.resyncs - win_resync;
                let expands = sb.expanded - win_expand;
                win_late = dec.late_dropped;
                win_resync = dec.resyncs;
                win_expand = sb.expanded;
                shared.send_ctrl(ClientMsg::Stats {
                    loss_pct: loss,
                    jitter_ms: sb.est.mean_abs as f32,
                    buf_ms: buf_ms as u32,
                    out_ms: out_ms as u32,
                    rx_pps: rx_pps as u32,
                    play_fps: play_fps as u32,
                    late_pps: (late as f64 / secs).round() as u32,
                    resyncs: resyncs as u32,
                    expand_pps: (expands as f64 / secs).round() as u32,
                    tx_pps,
                    mic_db,
                    dev_in_ms,
                    in_ring_ms,
                    enc_ms,
                    rtt_ms,
                    jb_ms,
                    dev_out_ms,
                    tx_path_ms: dev_in_ms + in_ring_ms + enc_ms,
                    rx_path_ms: jb_ms + out_ms as f32 + dev_out_ms,
                    contract_pps: {
                        let c = dec.contracted - win_contract;
                        win_contract = dec.contracted;
                        (c as f64 / secs).round() as u32
                    },
                    path: path.into(),
                });
            } else {

                drop(inbox);
                let path = match shared.p2p.lock().unwrap().phase {
                    P2pPhase::Direct => "direct",
                    P2pPhase::Punching => "punching",
                    P2pPhase::Failed => "failed",
                    P2pPhase::Idle => "idle",
                };
                shared.send_ctrl(ClientMsg::Stats {
                    loss_pct: 0.0,
                    jitter_ms: 0.0,
                    buf_ms: 0,
                    out_ms: 0,
                    rx_pps: 0,
                    play_fps: 0,
                    late_pps: 0,
                    resyncs: 0,
                    expand_pps: 0,
                    tx_pps,
                    mic_db,
                    dev_in_ms,
                    in_ring_ms,
                    enc_ms,
                    rtt_ms,
                    jb_ms: 0.0,
                    dev_out_ms,
                    tx_path_ms: dev_in_ms + in_ring_ms + enc_ms,
                    rx_path_ms: 0.0,
                    contract_pps: 0,
                    path: path.into(),
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
        assert_eq!(FRAME, 240);
        assert_eq!(STEREO_FRAME, 480);
    }

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
