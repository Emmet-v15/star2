//! 1:1 hole punch. Unlike star v1 there is **no relay fallback**: media only ever
//! flows on a bidirectionally-confirmed direct path, and a failed punch ends the call.

use std::collections::{HashSet, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Mutex;
use std::time::Instant;

use star2_proto::{
    flags, MediaHeader, PunchProbe, SessionId, MEDIA_HEADER_LEN, PUNCH_ACK, PUNCH_HDR_LEN,
    PUNCH_PADDED_LEN, PUNCH_PROBE,
};

use crate::*;

/// Where media goes. `dst` is `None` until a punch is confirmed - with no relay there
/// is nowhere else to send, so the encode thread simply holds its frames until then.
/// `allowed` is the source allowlist the recv thread enforces (there is no kernel
/// `connect()` filter, because we don't know the peer address ahead of time).
pub(crate) struct MediaRoute {
    pub(crate) dst: Mutex<Option<SocketAddr>>,
    pub(crate) allowed: Mutex<HashSet<SocketAddr>>,
}

/// Hole-punch FSM phase.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum P2pPhase {
    /// No peer yet (alone in the room).
    Idle,
    Punching,
    Direct,
    /// Punch failed or the direct path died. Terminal for this pairing.
    Failed,
}

pub(crate) struct P2pState {
    pub(crate) phase: P2pPhase,
    /// The 1:1 peer we're negotiating with (`None` = alone).
    pub(crate) peer_session: Option<SessionId>,
    /// Random credential inbound probes must echo; delivered over the signaling TLS.
    pub(crate) local_nonce: u64,
    /// The peer's credential, carried in our outbound probes.
    pub(crate) remote_nonce: Option<u64>,
    /// Peer candidate addresses to probe.
    pub(crate) cands: Vec<SocketAddr>,
    /// OUR candidates (reflexive + LAN), kept so the punch thread can re-offer.
    pub(crate) my_cands: Vec<String>,
    /// txids of probes we've sent; an ACK echoing one proves the path BOTH ways.
    pub(crate) my_txids: VecDeque<u64>,
    /// When the current punch attempt started (timeout basis).
    pub(crate) started: Instant,
    /// Last datagram seen from the peer (stall watchdog basis).
    pub(crate) last_peer_rx: Instant,
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
        }
    }

    /// Mint a probe txid and remember it (bounded ring) for ACK validation.
    pub(crate) fn new_txid(&mut self) -> u64 {
        let txid = rand_u64();
        if self.my_txids.len() >= P2P_MAX_TXIDS {
            self.my_txids.pop_front();
        }
        self.my_txids.push_back(txid);
        txid
    }

    /// Add a peer candidate (dedup, capped).
    pub(crate) fn merge_cand(&mut self, a: SocketAddr) {
        if !self.cands.contains(&a) && self.cands.len() < P2P_MAX_CANDS {
            self.cands.push(a);
        }
    }
}

/// OS-random u64 (punch nonces/txids - must be unguessable off-path).
pub(crate) fn rand_u64() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("os rng");
    u64::from_le_bytes(b)
}

/// The default-route LAN IP via the connect-trick - no interface enumeration, so
/// VPN/virtual adapters don't leak into candidates. `None` if offline.
pub(crate) fn lan_ip() -> Option<std::net::IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// Build and send one PUNCH datagram. `padded` sends the ~1200 B MTU-validating size
/// used while punching; ACKs and keepalive probes stay small.
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

/// Handle an inbound PUNCH datagram. Runs on the recv thread BEFORE the allowlist -
/// authorization is the nonce, not the source address (we're learning the address).
///
/// Returns the peer address only on the *transition* into `Direct`, so the caller can
/// announce the new path once rather than on every keepalive probe that follows.
pub(crate) fn handle_punch(
    my_session: SessionId,
    p2p: &Mutex<P2pState>,
    route: &MediaRoute,
    sock: &UdpSocket,
    src: SocketAddr,
    hdr_session: SessionId,
    payload: &[u8],
) -> Option<SocketAddr> {
    let pr = PunchProbe::decode(payload)?;
    let mut s = p2p.lock().unwrap();
    // Must be the expected 1:1 peer AND know OUR confidentially-delivered secret.
    if s.peer_session != Some(hdr_session) || pr.nonce != s.local_nonce {
        return None;
    }
    if s.phase == P2pPhase::Idle || s.phase == P2pPhase::Failed {
        return None;
    }
    s.last_peer_rx = Instant::now(); // any valid punch from the peer is liveness
    match pr.kind {
        PUNCH_PROBE => {
            // A valid probe proves the peer can reach us from `src`, so `src` is a
            // usable candidate even if it was never advertised (ICE calls this
            // peer-reflexive). Learning it here means the punch still completes
            // when the signalled candidate list was empty or incomplete - without
            // it, a side with nothing to probe can only sit and time out.
            s.merge_cand(src);
            // Reply ACK echoing the txid, carrying the PEER's nonce so it validates.
            let rn = s.remote_nonce?;
            send_punch(sock, my_session, PUNCH_ACK, rn, pr.txid, src, false);
            None
        }
        PUNCH_ACK => {
            // An ACK for a txid WE chose proves our probe reached the peer AND its reply
            // reached us - both directions, on THIS exact src (the peer-reflexive addr).
            if s.phase == P2pPhase::Punching && s.my_txids.contains(&pr.txid) {
                route.allowed.lock().unwrap().insert(src);
                *route.dst.lock().unwrap() = Some(src);
                s.phase = P2pPhase::Direct;
                log_line(format!("[p2p] direct up -> {src} ({}ms)", s.started.elapsed().as_millis()));
                return Some(src);
            }
            None
        }
        _ => None,
    }
}

/// Give up on the direct path. With no relay there is nothing to fall back to, so this
/// is terminal for the pairing; the caller signals the peer so it stops waiting.
pub(crate) fn p2p_fail(s: &mut P2pState, route: &MediaRoute, why: &str) {
    s.phase = P2pPhase::Failed;
    *route.dst.lock().unwrap() = None;
    route.allowed.lock().unwrap().clear();
    log_line(format!("[p2p] failed: {why}"));
}

/// Clear the pairing entirely (peer left / roster changed).
pub(crate) fn p2p_teardown(p2p: &Mutex<P2pState>, route: &MediaRoute, why: &str) {
    let mut s = p2p.lock().unwrap();
    // OUR OWN candidates survive a teardown. They describe this process - its
    // reflexive address and its LAN address - not the pairing, and they are only
    // ever discovered once, at startup. Wiping them here meant every subsequent
    // offer/answer carried an EMPTY candidate list, so the peer had nothing to
    // probe and the punch could only ever time out. Nothing rediscovers them
    // short of restarting the process.
    let my_cands = std::mem::take(&mut s.my_cands);
    *s = P2pState::new();
    s.my_cands = my_cands;
    *route.dst.lock().unwrap() = None;
    route.allowed.lock().unwrap().clear();
    log_line(format!("[p2p] teardown ({why})"));
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

    /// An ACK echoing a txid we minted must promote us to Direct on the observed src.
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

    /// The nonce is the authorization: a probe that doesn't know it changes nothing,
    /// so an off-path attacker cannot hijack the media destination.
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

    /// An ACK for a txid we never sent proves nothing about the return path.
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

    #[test]
    fn cands_dedup_and_cap() {
        let mut s = P2pState::new();
        for i in 0..20u16 {
            s.merge_cand(format!("10.0.0.1:{}", 1000 + i).parse().unwrap());
        }
        s.merge_cand("10.0.0.1:1000".parse().unwrap()); // dup
        assert_eq!(s.cands.len(), P2P_MAX_CANDS);
    }
}
