use std::collections::{HashSet, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Mutex;
use std::time::Instant;

use star2_proto::{
    flags, MediaHeader, PunchProbe, SessionId, MEDIA_HEADER_LEN, PUNCH_ACK, PUNCH_HDR_LEN,
    PUNCH_PADDED_LEN, PUNCH_PROBE,
};

use crate::*;

pub(crate) struct MediaRoute {
    pub(crate) dst: Mutex<Option<SocketAddr>>,
    pub(crate) allowed: Mutex<HashSet<SocketAddr>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum P2pPhase {
    Idle,
    Punching,
    Direct,
}

pub(crate) struct P2pState {
    pub(crate) phase: P2pPhase,
    pub(crate) peer_session: Option<SessionId>,
    pub(crate) local_nonce: u64,
    pub(crate) remote_nonce: Option<u64>,
    pub(crate) cands: Vec<SocketAddr>,
    pub(crate) my_cands: Vec<String>,
    pub(crate) my_txids: VecDeque<(u64, Instant)>,
    pub(crate) started: Instant,
    pub(crate) last_peer_rx: Instant,
    pub(crate) rtt_us: Option<u64>,
}

impl P2pState {
    pub(crate) fn new() -> Self {
        let now = Instant::now();
        Self {
            phase: P2pPhase::Idle,
            peer_session: None,
            local_nonce: 0,
            remote_nonce: None,
            cands: Vec::new(),
            my_cands: Vec::new(),
            my_txids: VecDeque::new(),
            started: now,
            last_peer_rx: now,
            rtt_us: None,
        }
    }

    pub(crate) fn new_txid(&mut self) -> u64 {
        let txid = rand_u64();
        if self.my_txids.len() >= P2P_MAX_TXIDS {
            self.my_txids.pop_front();
        }
        self.my_txids.push_back((txid, Instant::now()));
        txid
    }

    pub(crate) fn merge_cand(&mut self, a: SocketAddr) {
        if !self.cands.contains(&a) && self.cands.len() < P2P_MAX_CANDS {
            self.cands.push(a);
        }
    }
}

pub(crate) fn rand_u64() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("os rng");
    u64::from_le_bytes(b)
}

pub(crate) fn lan_ip() -> Option<std::net::IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

pub(crate) fn send_punch(
    sock: &UdpSocket,
    session: SessionId,
    kind: u8,
    nonce: u64,
    txid: u64,
    dst: SocketAddr,
    padded: bool,
) {
    let mut dg = [0u8; MEDIA_HEADER_LEN + PUNCH_PADDED_LEN];
    let len = MEDIA_HEADER_LEN + if padded { PUNCH_PADDED_LEN } else { PUNCH_HDR_LEN };
    MediaHeader::new(session, 0, 0, flags::PUNCH | flags::KEEPALIVE).encode(&mut dg[..MEDIA_HEADER_LEN]);
    PunchProbe { kind, nonce, txid }.encode(&mut dg[MEDIA_HEADER_LEN..]);
    let _ = sock.send_to(&dg[..len], dst);
}

pub(crate) fn handle_punch(
    my_session: SessionId,
    p2p: &Mutex<P2pState>,
    route: &MediaRoute,
    sock: &UdpSocket,
    src: SocketAddr,
    claimed_session: SessionId,
    payload: &[u8],
) -> Option<(SocketAddr, u64)> {
    let pr = PunchProbe::decode(payload)?;
    let mut s = p2p.lock().unwrap();

    if s.peer_session != Some(claimed_session) || pr.nonce != s.local_nonce {
        return None;
    }
    if s.phase == P2pPhase::Idle {
        return None;
    }
    s.last_peer_rx = Instant::now();
    match pr.kind {
        PUNCH_PROBE => {
            s.merge_cand(src);

            let rn = s.remote_nonce?;
            send_punch(sock, my_session, PUNCH_ACK, rn, pr.txid, src, false);
            None
        }
        PUNCH_ACK => {
            if let Some(&(_, sent)) = s.my_txids.iter().find(|(t, _)| *t == pr.txid) {
                s.rtt_us = Some(sent.elapsed().as_micros() as u64);
            }

            if s.phase == P2pPhase::Punching && s.my_txids.iter().any(|(t, _)| *t == pr.txid) {
                route.allowed.lock().unwrap().insert(src);
                *route.dst.lock().unwrap() = Some(src);
                s.phase = P2pPhase::Direct;
                return Some((src, s.started.elapsed().as_millis() as u64));
            }
            None
        }
        _ => None,
    }
}

pub(crate) fn p2p_teardown(p2p: &Mutex<P2pState>, route: &MediaRoute, why: &str) {
    let mut s = p2p.lock().unwrap();

    let my_cands = std::mem::take(&mut s.my_cands);
    *s = P2pState::new();
    s.my_cands = my_cands;
    *route.dst.lock().unwrap() = None;
    route.allowed.lock().unwrap().clear();
    super::debug_line(format!("[engine] direct path torn down ({why})"));
}

pub(crate) enum PunchTimeout {
    AbortAndRetry,
    StandDown,
}

pub(crate) fn timeout_action(me: SessionId, peer: SessionId) -> PunchTimeout {
    if me < peer {
        PunchTimeout::AbortAndRetry
    } else {
        PunchTimeout::StandDown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route() -> MediaRoute {
        MediaRoute { dst: Mutex::new(None), allowed: Mutex::new(HashSet::new()) }
    }

    fn punch_payload(kind: u8, nonce: u64, txid: u64) -> Vec<u8> {
        let mut p = vec![0u8; PUNCH_HDR_LEN];
        PunchProbe { kind, nonce, txid }.encode(&mut p);
        p
    }

    #[test]
    fn ack_with_our_txid_goes_direct() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let r = route();
        let p2p = Mutex::new(P2pState::new());
        let txid = {
            let mut s = p2p.lock().unwrap();
            s.peer_session = Some(7);
            s.local_nonce = 42;
            s.remote_nonce = Some(99);
            s.phase = P2pPhase::Punching;
            s.new_txid()
        };
        let src: SocketAddr = "203.0.113.5:5000".parse().unwrap();
        handle_punch(1, &p2p, &r, &sock, src, 7, &punch_payload(PUNCH_ACK, 42, txid));
        assert_eq!(p2p.lock().unwrap().phase, P2pPhase::Direct);
        assert_eq!(*r.dst.lock().unwrap(), Some(src));
    }

    #[test]
    fn wrong_nonce_is_ignored() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let r = route();
        let p2p = Mutex::new(P2pState::new());
        let txid = {
            let mut s = p2p.lock().unwrap();
            s.peer_session = Some(7);
            s.local_nonce = 42;
            s.remote_nonce = Some(99);
            s.phase = P2pPhase::Punching;
            s.new_txid()
        };
        let src: SocketAddr = "203.0.113.9:6000".parse().unwrap();
        handle_punch(1, &p2p, &r, &sock, src, 7, &punch_payload(PUNCH_ACK, 0xBAD, txid));
        assert_eq!(p2p.lock().unwrap().phase, P2pPhase::Punching);
        assert_eq!(*r.dst.lock().unwrap(), None);
    }

    #[test]
    fn unknown_txid_does_not_promote() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let r = route();
        let p2p = Mutex::new(P2pState::new());
        {
            let mut s = p2p.lock().unwrap();
            s.peer_session = Some(7);
            s.local_nonce = 42;
            s.remote_nonce = Some(99);
            s.phase = P2pPhase::Punching;
        }
        let src: SocketAddr = "203.0.113.9:6000".parse().unwrap();
        handle_punch(1, &p2p, &r, &sock, src, 7, &punch_payload(PUNCH_ACK, 42, 0xDEAD));
        assert_eq!(p2p.lock().unwrap().phase, P2pPhase::Punching);
    }

    fn punching(p2p: &Mutex<P2pState>, peer: SessionId, local: u64, remote: u64) -> u64 {
        let mut s = p2p.lock().unwrap();
        s.peer_session = Some(peer);
        s.local_nonce = local;
        s.remote_nonce = Some(remote);
        s.phase = P2pPhase::Punching;
        s.new_txid()
    }

    #[test]
    fn a_dead_call_returns_to_idle_and_can_dial_again() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let r = route();
        let p2p = Mutex::new(P2pState::new());

        let txid = punching(&p2p, 7, 42, 99);
        let first: SocketAddr = "203.0.113.5:5000".parse().unwrap();
        handle_punch(1, &p2p, &r, &sock, first, 7, &punch_payload(PUNCH_ACK, 42, txid));
        assert_eq!(p2p.lock().unwrap().phase, P2pPhase::Direct);

        p2p_teardown(&p2p, &r, "peer stopped responding");
        {
            let s = p2p.lock().unwrap();
            assert_eq!(s.phase, P2pPhase::Idle, "a dead call left the state machine wedged");
            assert_eq!(s.peer_session, None, "the dead peer is still latched, so a rejoin is ignored");
        }
        assert_eq!(*r.dst.lock().unwrap(), None);
        assert!(r.allowed.lock().unwrap().is_empty(), "the dead peer can still send us media");

        let txid = punching(&p2p, 9, 77, 11);
        let second: SocketAddr = "203.0.113.8:7000".parse().unwrap();
        handle_punch(1, &p2p, &r, &sock, second, 9, &punch_payload(PUNCH_ACK, 77, txid));
        assert_eq!(
            p2p.lock().unwrap().phase,
            P2pPhase::Direct,
            "a second call could not start without restarting the process"
        );
        assert_eq!(*r.dst.lock().unwrap(), Some(second));
    }

    #[test]
    fn teardown_keeps_the_candidates_we_already_discovered() {
        let r = route();
        let p2p = Mutex::new(P2pState::new());
        p2p.lock().unwrap().my_cands = vec!["203.0.113.5:5000".into()];

        p2p_teardown(&p2p, &r, "peer left");
        assert_eq!(
            p2p.lock().unwrap().my_cands,
            vec!["203.0.113.5:5000".to_string()],
            "we forgot our own address and have to rediscover it before the next call"
        );
    }

    #[test]
    fn cands_dedup_and_cap() {
        let mut s = P2pState::new();
        for i in 0..20u16 {
            s.merge_cand(format!("10.0.0.1:{}", 1000 + i).parse().unwrap());
        }
        s.merge_cand("10.0.0.1:1000".parse().unwrap());
        assert_eq!(s.cands.len(), P2P_MAX_CANDS);
    }

    #[test]
    fn only_the_lower_session_id_declares_a_punch_failure() {
        assert!(
            matches!(timeout_action(4, 9), PunchTimeout::AbortAndRetry),
            "the controller (lower session id) did not take the retry decision"
        );
        assert!(
            matches!(timeout_action(9, 4), PunchTimeout::StandDown),
            "the responder also declared failure, so both sides abort and re-offer over each other"
        );
    }

    #[test]
    fn two_states_punch_each_other_over_real_sockets() {
        let a_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b_addr = b_sock.local_addr().unwrap();
        let a_addr = a_sock.local_addr().unwrap();
        let ra = route();
        let rb = route();
        let pa = Mutex::new(P2pState::new());
        let pb = Mutex::new(P2pState::new());
        let mut buf = [0u8; 2048];

        {
            let mut a = pa.lock().unwrap();
            a.peer_session = Some(7);
            a.local_nonce = 42;
            a.remote_nonce = Some(99);
            a.phase = P2pPhase::Punching;
            a.merge_cand(b_addr);
            let txid = a.new_txid();
            send_punch(&a_sock, 1, PUNCH_PROBE, 99, txid, b_addr, true);

            let mut b = pb.lock().unwrap();
            b.peer_session = Some(1);
            b.local_nonce = 99;
            b.remote_nonce = Some(42);
            b.phase = P2pPhase::Punching;
        }

        let (n, src) = b_sock.recv_from(&mut buf).unwrap();
        assert_eq!(
            handle_punch(7, &pb, &rb, &b_sock, src, 1, &buf[MEDIA_HEADER_LEN..n]),
            None,
            "answering a probe was read as a completed punch"
        );
        assert!(
            pb.lock().unwrap().cands.contains(&a_addr),
            "the probe never taught B where A really sends from"
        );

        let (n, src) = a_sock.recv_from(&mut buf).unwrap();
        assert_eq!(
            handle_punch(1, &pa, &ra, &a_sock, src, 7, &buf[MEDIA_HEADER_LEN..n]).map(|(p, _)| p),
            Some(b_addr),
            "A's own probe ACK did not promote the path to direct"
        );
        assert_eq!(pa.lock().unwrap().phase, P2pPhase::Direct);
        assert_eq!(*ra.dst.lock().unwrap(), Some(b_addr));
        assert!(pa.lock().unwrap().rtt_us.is_some(), "a confirmed ACK carried no rtt");

        {
            let mut b = pb.lock().unwrap();
            b.merge_cand(a_addr);
            b.phase = P2pPhase::Punching;
            let txid = b.new_txid();
            send_punch(&b_sock, 7, PUNCH_PROBE, 42, txid, a_addr, true);
        }

        let (n, src) = a_sock.recv_from(&mut buf).unwrap();
        assert_eq!(handle_punch(1, &pa, &ra, &a_sock, src, 7, &buf[MEDIA_HEADER_LEN..n]), None);

        let (n, src) = b_sock.recv_from(&mut buf).unwrap();
        assert_eq!(
            handle_punch(7, &pb, &rb, &b_sock, src, 1, &buf[MEDIA_HEADER_LEN..n]).map(|(p, _)| p),
            Some(a_addr),
            "B's probe ACK never came back or was rejected"
        );
        assert_eq!(pb.lock().unwrap().phase, P2pPhase::Direct);
        assert_eq!(*rb.dst.lock().unwrap(), Some(a_addr));
        assert_ne!(
            *ra.dst.lock().unwrap(),
            *rb.dst.lock().unwrap(),
            "both ends pinned the same destination - one of them is pointing at itself"
        );
    }
}
