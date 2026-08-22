use std::collections::{HashMap, HashSet, VecDeque};
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
    PUNCH_PROBE, RED_LEN_BYTES,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

mod audio;
pub mod handover;
mod p2p;
mod playout;

use audio::{buffer_size_for, pick_config, pick_device, Resampler};
use p2p::*;
use playout::*;

pub const SR: u32 = 48_000;
const FRAME_MS: u32 = 5;
const FRAME: usize = (SR as usize / 1000) * FRAME_MS as usize;
const STEREO_FRAME: usize = FRAME * 2;

const OUT_FLOOR_FRAMES: usize = (10 / FRAME_MS) as usize;
const OUT_MAX_FRAMES: usize = (120 / FRAME_MS) as usize;
const OUT_SHRINK_AFTER_S: u64 = 2;

const MAX_FRAMES: usize = (300 / FRAME_MS) as usize;
const SHED_MARGIN: usize = (80 / FRAME_MS) as usize;
const JITTER_K: f64 = 2.0;
const JITTER_MARGIN_MS: f64 = 8.0;
const RESYNC_CONCEAL: u32 = 500 / FRAME_MS;
const RED_HOLD_S: u32 = 10;
const JB_BINS: usize = 64;
const JB_BIN_MS: f64 = 4.0;
const JB_DECAY_EVERY: u32 = 256;
const JB_PCTILE: f64 = 0.97;
const SPIKE_MULT: f64 = 3.0;
const SPIKE_MAX_MS: f64 = 400.0;
const RENDEZVOUS_RETRY: Duration = Duration::from_secs(3);
const RENDEZVOUS_QUIET: Duration = Duration::from_secs(30);
const DIRECT_GRACE: Duration = Duration::from_secs(3);
const SPIKE_DECAY: f64 = 0.985;

const P2P_PROBE_INTERVAL: Duration = Duration::from_millis(200);
const P2P_PUNCH_TIMEOUT: Duration = Duration::from_secs(8);

const P2P_DIRECT_DEAD: Duration = Duration::from_secs(5);
const P2P_MAX_CANDS: usize = 8;
const P2P_MAX_TXIDS: usize = 256;
const PKT_BUF: usize = 4096;

static VERBOSE: AtomicBool = AtomicBool::new(false);
static RENDEZVOUS_DOWN: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

pub fn log_line(line: String) {
    eprintln!("{line}");
}

fn debug_line(line: String) {
    if VERBOSE.load(Ordering::Relaxed) {
        eprintln!("{line}");
    }
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
    pub room_token: String,
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
            room_token: "general".into(),
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

    Direct { peer: SocketAddr, ms: u64 },

    Ended(String),

    Stats { jitter_ms: f64, target_ms: f64, loss_pct: f32, out_ms: f64, rx_pps: u64, play_fps: u64 },
}

pub struct CallHandle {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    shared: Arc<Shared>,
    sock: UdpSocket,

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

    pub fn live_call(&self) -> Option<(&UdpSocket, handover::CallState)> {
        let s = self.shared.p2p.lock().unwrap();
        if s.phase != P2pPhase::Direct {
            return None;
        }
        Some((
            &self.sock,
            handover::CallState {
                socket: String::new(),
                peer_addr: (*self.shared.route.dst.lock().unwrap())?,
                media_session: self.shared.my_session(),
                peer_session: s.peer_session?,
                local_nonce: s.local_nonce,
                remote_nonce: s.remote_nonce?,
            },
        ))
    }

    pub fn stop_receiving(&self) {
        self.shared.rx_stop.store(true, Ordering::Relaxed);
    }

    pub fn hand_off_point(&self) -> handover::Cutover {
        let lead = handover::SEND_OVERLAP.as_millis() as u32 / FRAME_MS;
        handover::Cutover {
            seq: (self.shared.seq.load(Ordering::Relaxed) as u16).wrapping_add(lead as u16),
            ts: self.shared.ts.load(Ordering::Relaxed).wrapping_add(lead * FRAME as u32),
        }
    }

    pub fn resume_from(&self, at: handover::Cutover) {
        self.shared.seq.store(at.seq as u32, Ordering::Relaxed);
        self.shared.ts.store(at.ts, Ordering::Relaxed);
        self.shared.paused.store(false, Ordering::Release);
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

    peer_wants_red: AtomicBool,
    want_red: AtomicBool,
    stop: Arc<AtomicBool>,

    inherited_call: bool,
    paused: AtomicBool,
    rx_stop: AtomicBool,
    seq: AtomicU32,
    ts: AtomicU32,
}

fn go_idle(shared: &Arc<Shared>, why: &str) {
    p2p_teardown(&shared.p2p, &shared.route, why);
    shared.inbox.lock().unwrap().clear();
}

fn abandon(
    shared: &Arc<Shared>,
    on_event: &Arc<dyn Fn(Event) + Send + Sync>,
    peer: Option<SessionId>,
    why: &str,
) {
    if let Some(p) = peer {
        shared.send_ctrl(ClientMsg::P2pAbort { to: p });
    }
    go_idle(shared, why);
    on_event(Event::Status(format!("idle {why}")));

    let members = shared.members.lock().unwrap().clone();
    on_membership(shared, &members, on_event);
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

pub fn start_call<F>(
    cfg: CallConfig,
    inherited: Option<handover::Inherited>,
    on_event: F,
) -> Result<CallHandle>
where
    F: Fn(Event) + Send + Sync + 'static,
{
    sharpen_timer();
    let on_event: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(on_event);
    let stop = Arc::new(AtomicBool::new(false));
    let send_ch = if cfg.stereo { 2 } else { 1 };

    let taken = inherited.as_ref().map(|i| i.call.clone());
    let sock = match inherited {
        Some(i) => i.sock,
        None => UdpSocket::bind("0.0.0.0:0")?,
    };
    sock.set_read_timeout(Some(Duration::from_millis(100)))?;
    let local_port = sock.local_addr()?.port();

    let shared = Arc::new(Shared {
        session: Mutex::new(taken.as_ref().map(|c| c.media_session)),
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
        peer_wants_red: AtomicBool::new(false),
        want_red: AtomicBool::new(true),
        stop: stop.clone(),
        inherited_call: taken.is_some(),
        paused: AtomicBool::new(taken.is_some()),
        rx_stop: AtomicBool::new(false),
        seq: AtomicU32::new(0),
        ts: AtomicU32::new(0),
    });

    if let Some(c) = &taken {
        let mut s = shared.p2p.lock().unwrap();
        s.phase = P2pPhase::Direct;
        s.peer_session = Some(c.peer_session);
        s.local_nonce = c.local_nonce;
        s.remote_nonce = Some(c.remote_nonce);
        s.last_peer_rx = Instant::now();
        s.merge_cand(c.peer_addr);
        drop(s);
        *shared.route.dst.lock().unwrap() = Some(c.peer_addr);
        shared.route.allowed.lock().unwrap().insert(c.peer_addr);
    }

    let host = cpal::default_host();
    let input = pick_device(&host, &cfg.input, true)?;
    let output = pick_device(&host, &cfg.output, false)?;
    let in_cfg = pick_config(&input, true)?;
    let out_cfg = pick_config(&output, false)?;
    debug_line(format!(
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

    let in_ch = in_cfg.channels() as usize;
    let out_ch = out_cfg.channels() as usize;

    let underruns = Arc::new(AtomicU64::new(0));
    let out_target = Arc::new(AtomicUsize::new(OUT_FLOOR_FRAMES * STEREO_FRAME));
    let out_fill = Arc::new(AtomicUsize::new(0));

    let (mut in_prod, mut in_cons) = HeapRb::<f32>::new(SR as usize * 2).split();
    let (mut master_prod, mut master_cons) = HeapRb::<f32>::new(SR as usize * 2).split();

    let err_fn = |e| log_line(format!("audio {e}"));
    if in_rate != SR {
        debug_line(format!("[engine] resampling mic {in_rate}Hz -> {SR}Hz"));
    }
    if out_rate != SR {
        debug_line(format!("[engine] resampling playout {SR}Hz -> {out_rate}Hz"));
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
        let mut resamp = Resampler::new(SR, out_rate, 2);
        let mut block: Vec<f32> = Vec::with_capacity(STEREO_FRAME);
        let mut resampled: Vec<f32> = Vec::with_capacity(STEREO_FRAME * 2);
        let mut ready: VecDeque<f32> = VecDeque::with_capacity(STEREO_FRAME * 4);
        output.build_output_stream(
            &cfg2,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                let t = info.timestamp();
                if let Some(d) = t.playback.duration_since(&t.callback) {
                    sh_out.dev_out_us.store(d.as_micros() as u32, Ordering::Relaxed);
                }
                let want = data.len() / out_ch * 2;
                while ready.len() < want {
                    block.clear();
                    for _ in 0..FRAME {
                        let l = match master_cons.try_pop() {
                            Some(v) => v,
                            None => {
                                underruns.fetch_add(1, Ordering::Relaxed);
                                0.0
                            }
                        };
                        block.push(l);
                        block.push(master_cons.try_pop().unwrap_or(0.0));
                    }
                    resampled.clear();
                    resamp.process(&block, &mut resampled);
                    ready.extend(resampled.iter().copied());
                }
                let mut i = 0;
                while i + out_ch <= data.len() {
                    let l = ready.pop_front().unwrap_or(0.0);
                    let r = ready.pop_front().unwrap_or(0.0);
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
            loop {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                let Err(e) = rt.block_on(control_loop(&cfg, &shared, &sock, local_port, &on_event))
                else {
                    return;
                };
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }

                if !RENDEZVOUS_DOWN.swap(true, Ordering::Relaxed) {
                    if shared.p2p.lock().unwrap().phase == P2pPhase::Direct {
                        on_event(Event::Status("rendezvous down, call unaffected".into()));
                    } else {
                        on_event(Event::Status(format!("rendezvous down: {e}")));
                    }
                }
                std::thread::sleep(RENDEZVOUS_RETRY);
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

    Ok(CallHandle { stop, threads, shared, sock, _streams: (in_stream, out_stream) })
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
    if RENDEZVOUS_DOWN.swap(false, Ordering::Relaxed) {
        on_event(Event::Status("rendezvous up".into()));
    }
    let (mut tx_ws, mut rx_ws) = ws.split();
    let (tx, mut rx) = unbounded_channel::<ClientMsg>();
    *shared.ctrl_tx.lock().unwrap() = Some(tx.clone());

    tx.send(ClientMsg::Hello {
        name: cfg.name.clone(),
        ver: PROTO_VERSION as u32,
        token: cfg.token.clone(),
        build: env!("CARGO_PKG_VERSION").into(),
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
            msg = tokio::time::timeout(RENDEZVOUS_QUIET, rx_ws.next()) => {
                let Ok(msg) = msg else { bail!("rendezvous went quiet") };
                let Some(msg) = msg else { bail!("rendezvous closed") };
                let tokio_tungstenite::tungstenite::Message::Text(txt) = msg? else { continue };
                let Ok(sm) = serde_json::from_str::<ServerMsg>(&txt) else { continue };
                match sm {
                    ServerMsg::Welcome { session, reflex } => {
                        if shared.inherited_call {
                            debug_line(format!(
                                "[engine] rendezvous session {session}, call carries on as {}",
                                shared.my_session()
                            ));
                            continue;
                        }
                        *shared.session.lock().unwrap() = Some(session);
                        debug_line(format!("[engine] session {session}, reflex via {reflex}"));

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
                        debug_line(format!("[engine] candidates: {}", cands.join(", ")));
                        shared.p2p.lock().unwrap().my_cands = cands;
                        tx.send(ClientMsg::Join { room: cfg.room_token.clone() })?;
                        joined = true;
                    }
                    ServerMsg::Room { members, .. } => {
                        on_event(Event::Status(
                            if members.len() < 2 { "wait" } else { "punch" }.into(),
                        ));
                        let ids: HashSet<SessionId> = members.iter().map(|m| m.session).collect();
                        on_membership(shared, &ids, on_event);
                    }
                    ServerMsg::Joined { session, name } => {
                        on_event(Event::Status(format!("peer {name}")));

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
                            on_event(Event::Status("peer left rendezvous, direct path up".into()));
                        } else if peer == Some(session) {

                            go_idle(&shared, "peer left");
                            on_event(Event::Status("idle peer left".into()));
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
                        debug_line("[engine] punching (answered offer)".into());
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
                            on_event(Event::Status("idle peer aborted".into()));
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
    log_line("rendezvous no reflexive address (UDP unreachable?)".into());
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
        if s.phase == P2pPhase::Direct && s.last_peer_rx.elapsed() < DIRECT_GRACE {
            s.peer_session = Some(peer);
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
            debug_line(format!("[engine] punching peer {peer} (controller)"));
        }
    } else if members.len() > 2 {

        go_idle(shared, "room has more than 2 members");
        on_event(Event::Status(format!("idle room has {} members, 1:1 only", members.len())));
    }
}

fn recv_loop(shared: &Arc<Shared>, sock: &UdpSocket, on_event: &Arc<dyn Fn(Event) + Send + Sync>) {
    let mut buf = vec![0u8; PKT_BUF];
    let base = Instant::now();
    while !shared.stop.load(Ordering::Relaxed) && !shared.rx_stop.load(Ordering::Relaxed) {
        if shared.paused.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        let (n, src) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(header) = MediaHeader::decode(&buf[..n]) else { continue };
        if header.version != PROTO_VERSION {
            continue;
        }
        let payload = &buf[MEDIA_HEADER_LEN..n];

        if header.is_reflex() {
            if let Ok(addr) = std::str::from_utf8(payload) {
                let mut slot = shared.reflex.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(addr.to_string());
                    debug_line(format!("[engine] reflexive addr {addr}"));
                }
            }
            continue;
        }

        if header.is_punch() {

            if let Some((peer, ms)) =
                handle_punch(shared.my_session(), &shared.p2p, &shared.route, sock, src, header.session, payload)
            {
                on_event(Event::Direct { peer, ms });
            }
            continue;
        }

        if !shared.route.allowed.lock().unwrap().contains(&src) {
            continue;
        }
        shared.p2p.lock().unwrap().last_peer_rx = Instant::now();
        if header.is_keepalive() {
            continue;
        }

        shared.peer_wants_red.store(header.red_wanted(), Ordering::Relaxed);

        let (red, data) = match header.is_red() {
            true => match star2_proto::split_red(payload) {
                Some((prev, cur)) => (Some(prev.to_vec()), cur),
                None => continue,
            },
            false => (None, payload),
        };

        let arr_ms = base.elapsed().as_secs_f64() * 1000.0;
        let mut inbox = shared.inbox.lock().unwrap();
        let sb = inbox.entry(header.session).or_default();
        if let (Some(la), Some(lt)) = (sb.last_arr_ms, sb.last_ts) {

            sb.est.observe(arrival_skew_ms(arr_ms - la, header.timestamp, lt));
        }
        sb.last_arr_ms = Some(arr_ms);
        sb.last_ts = Some(header.timestamp);
        sb.recv_count += 1;
        sb.insert_capped(
            header.seq,
            AudioPkt { flags: header.flags, data: data.to_vec(), red, arr: Instant::now() },
        );
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
                    drop(s);
                    abandon(
                        shared,
                        on_event,
                        peer,
                        "punch failed, no direct path (symmetric NAT/CGNAT?)",
                    );
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
                    drop(s);
                    abandon(shared, on_event, peer, "peer silent");
                    continue;
                }

                if let (Some(rn), Some(dst)) = (s.remote_nonce, *shared.route.dst.lock().unwrap()) {
                    let txid = s.new_txid();
                    send_punch(sock, me, PUNCH_PROBE, rn, txid, dst, false);
                }
            }

            P2pPhase::Idle => {}
        }
    }
}

fn arrival_skew_ms(d_arr_ms: f64, ts: u32, last_ts: u32) -> f64 {
    let d_ts = (ts.wrapping_sub(last_ts) as i32) as f64 / (SR as f64 / 1000.0);
    (d_arr_ms - d_ts).abs()
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

    let need = FRAME * send_ch;
    let mut pcm_f = vec![0.0f32; need];
    let mut pcm_i16 = vec![0i16; need];
    let mut dg = vec![0u8; MEDIA_HEADER_LEN + PKT_BUF];
    let mut enc_buf = vec![0u8; PKT_BUF];
    let mut red_prev: Vec<u8> = Vec::new();
    let base_flags = if send_ch == 2 { flags::STEREO } else { 0 };
    let (mut seq, mut ts) = (0u16, 0u32);
    let mut resuming = false;

    while !shared.stop.load(Ordering::Relaxed) {
        if in_cons.occupied_len() < need {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        if shared.paused.load(Ordering::Acquire) {
            for _ in 0..need {
                let _ = in_cons.try_pop();
            }
            resuming = true;
            continue;
        }
        if resuming {
            resuming = false;
            seq = shared.seq.load(Ordering::Relaxed) as u16;
            ts = shared.ts.load(Ordering::Relaxed);
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
            publish_cursor(shared, seq, ts);
            continue;
        };
        for (i, &s) in pcm_f.iter().enumerate() {
            pcm_i16[i] = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        let session = shared.my_session();
        let mut header_flags = base_flags;
        if shared.want_red.load(Ordering::Relaxed) {
            header_flags |= flags::RED_WANTED;
        }
        if let Ok(len) = enc.encode(&pcm_i16, &mut enc_buf) {
            let carry = shared.peer_wants_red.load(Ordering::Relaxed)
                && !red_prev.is_empty()
                && MEDIA_HEADER_LEN + RED_LEN_BYTES + red_prev.len() + len <= PKT_BUF;
            let end = match carry {
                true => {
                    header_flags |= flags::RED;
                    let at = MEDIA_HEADER_LEN;
                    let n = red_prev.len();
                    dg[at..at + RED_LEN_BYTES].copy_from_slice(&(n as u16).to_be_bytes());
                    let at = at + RED_LEN_BYTES;
                    dg[at..at + n].copy_from_slice(&red_prev);
                    let at = at + n;
                    dg[at..at + len].copy_from_slice(&enc_buf[..len]);
                    at + len
                }
                false => {
                    dg[MEDIA_HEADER_LEN..MEDIA_HEADER_LEN + len].copy_from_slice(&enc_buf[..len]);
                    MEDIA_HEADER_LEN + len
                }
            };
            red_prev.clear();
            red_prev.extend_from_slice(&enc_buf[..len]);
            MediaHeader::new(session, seq, ts, header_flags).encode(&mut dg[..MEDIA_HEADER_LEN]);
            if sock.send_to(&dg[..end], dst).is_ok() {
                shared.tx_pkts.fetch_add(1, Ordering::Relaxed);
                shared.enc_sum_us.fetch_add(enc_start.elapsed().as_micros() as u64, Ordering::Relaxed);
                shared.enc_n.fetch_add(1, Ordering::Relaxed);
            }
        }
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(FRAME as u32);
        publish_cursor(shared, seq, ts);
    }
    Ok(())
}

fn publish_cursor(shared: &Arc<Shared>, seq: u16, ts: u32) {
    shared.seq.store(seq as u32, Ordering::Relaxed);
    shared.ts.store(ts, Ordering::Relaxed);
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
    let mut cur_sess: Option<SessionId> = None;
    let mut frame = vec![0.0f32; STEREO_FRAME];
    let mut scratch = vec![0i16; FRAME * 2];
    let mut last_under = underruns.load(Ordering::Relaxed);
    let mut clean_since = Instant::now();
    let mut clean_needed = OUT_SHRINK_AFTER_S;
    let mut last_stats = Instant::now();

    let (mut win_played, mut win_concealed, mut win_recv) = (0u64, 0u64, 0u64);
    let (mut win_late, mut win_resync, mut win_expand) = (0u64, 0u64, 0u64);
    let (mut win_recovered, mut red_clean) = (0u64, 0u32);

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

            if cur_sess != Some(sess) {
                if cur_sess.is_some() {
                    debug_line(format!("[engine] sender changed -> resetting sequencer for session {sess}"));
                }
                dec.resync(&mut sb.pkts);
                dec.dead = 0;
                dec.dead_recv = 0;
                cur_sess = Some(sess);
            }
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
                        debug_line("[engine] jitter buffer desync - resyncing".into());
                        dec.resync(&mut sb.pkts);
                        dec.dead = 0;
                    }
                }

                Playout::Expanded => {
                    sb.expanded += 1;
                    dec.dead += 1;
                    if dec.dead > RESYNC_CONCEAL && sb.recv_count > dec.dead_recv {
                        debug_line("[engine] jitter buffer stalled - resyncing".into());
                        dec.resync(&mut sb.pkts);
                        dec.dead = 0;
                    }
                }
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
                debug_line(format!("[engine] output buffer -> {} ms (under-runs)", tgt / STEREO_FRAME * FRAME_MS as usize));
            }
            clean_since = Instant::now();
        } else if clean_since.elapsed().as_secs() >= clean_needed
            && tgt > OUT_FLOOR_FRAMES * STEREO_FRAME
        {
            tgt -= STEREO_FRAME;
            out_target.store(tgt, Ordering::Relaxed);
            clean_since = Instant::now();

            clean_needed = (clean_needed / 2).max(OUT_SHRINK_AFTER_S);
            debug_line(format!(
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
                    P2pPhase::Idle => "idle",
                };
                let late = dec.late_dropped - win_late;
                let resyncs = dec.resyncs - win_resync;
                let expands = sb.expanded - win_expand;
                win_late = dec.late_dropped;
                win_resync = dec.resyncs;
                win_expand = sb.expanded;

                if concealed + expands + (dec.recovered - win_recovered) > 0 {
                    red_clean = 0;
                } else {
                    red_clean += 1;
                }
                win_recovered = dec.recovered;
                shared.want_red.store(red_clean < RED_HOLD_S, Ordering::Relaxed);

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
                    path: path.into(),
                });
            } else {

                drop(inbox);
                let path = match shared.p2p.lock().unwrap().phase {
                    P2pPhase::Direct => "direct",
                    P2pPhase::Punching => "punching",
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
    fn a_rewound_timestamp_is_read_as_a_small_step_not_a_lifetime_of_jitter() {
        let frame = FRAME as u32;
        let steady = arrival_skew_ms(5.0, 100 * frame, 99 * frame);
        let rewound = arrival_skew_ms(5.0, 99 * frame, 100 * frame);
        assert!(steady < 1.0, "a frame arriving on time read as {steady}ms of skew");
        assert!(
            rewound < 20.0,
            "a timestamp one frame behind read as {rewound}ms of jitter, which pins the peer's              buffer at its ceiling and stalls playout"
        );
    }

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
