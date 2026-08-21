//! star2 wire protocol - shared by the signal server, the engine, and test clients.
//!
//! Two planes:
//!   * **Signaling** - reliable, JSON over WebSocket. [`ClientMsg`] / [`ServerMsg`].
//!   * **Media**     - unreliable, binary over UDP. [`MediaHeader`] + Opus payload.
//!
//! Unlike star v1, the server is **signaling-only**: it brokers the candidate exchange
//! and answers reflexive-address probes, but audio never traverses it. A call that
//! fails to punch does not fall back - it fails. See [`ClientMsg::P2pAbort`].

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible wire change. Peers drop mismatched media packets.
pub const PROTO_VERSION: u8 = 1;

/// Fixed media header length, in bytes. Payload follows immediately after.
pub const MEDIA_HEADER_LEN: usize = 12;

/// Identifies a connected client for the lifetime of its signaling session.
/// Carried in every media datagram so a peer can attribute packets to a sender.
pub type SessionId = u32;

/// Media header flag bits (the `flags` byte).
pub mod flags {
    /// Reflexive-address probe to/from the signal server's UDP port (our STUN).
    /// Request has an empty payload; the reply's payload is the observed `ip:port`
    /// as UTF-8. Always OR'd with [`KEEPALIVE`] so it never reaches the media path.
    pub const REFLEX: u8 = 0b0000_0001;
    /// No audio payload - NAT keepalive only. Never forwarded, never played.
    pub const KEEPALIVE: u8 = 0b0000_1000;
    /// Audio payload has 2 interleaved channels (clear = 1 / mono); the receiver
    /// sizes its decoder from this, so mono and stereo peers interoperate.
    pub const STEREO: u8 = 0b0100_0000;
    /// P2P hole-punch probe/ACK ([`crate::PunchProbe`]), NOT audio. Always OR'd with
    /// [`KEEPALIVE`] so a misrouted probe is swallowed instead of reaching playout.
    pub const PUNCH: u8 = 0b1000_0000;
}

// ---------------------------------------------------------------------------
// Signaling plane
// ---------------------------------------------------------------------------

/// Messages sent client -> signal server over the WebSocket.
// No `Eq`: the Stats variant carries f32s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ClientMsg {
    /// First message on every connection. `ver` is [`PROTO_VERSION`].
    /// Auth is a static shared secret - this is a two-person MVP, not a product.
    Hello { name: String, ver: u32, token: String },
    /// Join (or switch to) a room. Leaves any current room first.
    Join { room: String },
    /// Leave the current room (stay connected).
    Leave,
    /// 1 Hz health report, so call quality can be observed from the server without
    /// access to either machine. Purely diagnostic: the server logs it and does
    /// nothing else with it, and media never depends on it arriving.
    Stats {
        loss_pct: f32,
        jitter_ms: f32,
        buf_ms: u32,
        out_ms: u32,
        rx_pps: u32,
        play_fps: u32,
        /// Packets that arrived too late to play, per second. Non-zero here means
        /// the jitter buffer is too SHALLOW; genuine network loss leaves it at zero.
        late_pps: u32,
        /// Desync re-latches in this window. Each one dumps a whole buffer, so even
        /// one per second is a large share of the concealment.
        resyncs: u32,
        /// "direct" once punched, otherwise the phase we're stuck in.
        path: String,
    },
    // --- P2P signaling: forwarded same-room-only, unicast to `to` ---
    /// Controller -> answerer: offer a direct path. `nonce` is the credential inbound
    /// probes to *this* sender must echo; `cands` are its candidate `ip:port`s.
    P2pOffer { to: SessionId, nonce: u64, cands: Vec<String> },
    /// Answerer -> controller reply, same shape.
    P2pAnswer { to: SessionId, nonce: u64, cands: Vec<String> },
    /// Trickle a late candidate (e.g. a reflexive addr that arrived after the offer).
    P2pCandidate { to: SessionId, cand: String },
    /// Give up on the direct path. With no relay fallback this ends the call; it
    /// exists so the peer tears down immediately instead of waiting for a timeout.
    P2pAbort { to: SessionId },
}

/// Messages sent signal server -> client over the WebSocket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ServerMsg {
    /// Sent once after a valid [`ClientMsg::Hello`]. `reflex` is the server's
    /// `host:port` UDP endpoint that answers [`flags::REFLEX`] probes.
    Welcome { session: SessionId, reflex: String },
    /// Full roster snapshot for `room`, sent to a client right after it joins.
    Room { room: String, members: Vec<Member> },
    /// A member joined the caller's current room.
    Joined { session: SessionId, name: String },
    /// A member left the caller's current room (or disconnected).
    Left { session: SessionId },
    /// Fatal handshake/protocol error; the server closes the connection after this.
    Error { msg: String },
    // --- P2P: the server forwards the matching ClientMsg, stamping `from` ---
    P2pOffer { from: SessionId, nonce: u64, cands: Vec<String> },
    P2pAnswer { from: SessionId, nonce: u64, cands: Vec<String> },
    P2pCandidate { from: SessionId, cand: String },
    P2pAbort { from: SessionId },
}

/// One room participant in a [`ServerMsg::Room`] roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub session: SessionId,
    pub name: String,
}

// ---------------------------------------------------------------------------
// Media plane
// ---------------------------------------------------------------------------

/// Fixed 12-byte media datagram header (little-endian on the wire).
///
/// Layout: `version[1] flags[1] session[4] seq[2] timestamp[4]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaHeader {
    pub version: u8,
    pub flags: u8,
    /// Sender's [`SessionId`], so a receiver can attribute the stream.
    pub session: SessionId,
    /// Per-sender sequence number (wraps); used for reorder/dedup/PLC.
    pub seq: u16,
    /// 48 kHz sample-clock timestamp; used for jitter buffering.
    pub timestamp: u32,
}

impl MediaHeader {
    /// Build a header at the current [`PROTO_VERSION`].
    pub fn new(session: SessionId, seq: u16, timestamp: u32, flags: u8) -> Self {
        Self { version: PROTO_VERSION, flags, session, seq, timestamp }
    }

    /// Write the header into the first [`MEDIA_HEADER_LEN`] bytes of `buf`.
    ///
    /// # Panics
    /// Panics if `buf.len() < MEDIA_HEADER_LEN`.
    pub fn encode(&self, buf: &mut [u8]) {
        assert!(buf.len() >= MEDIA_HEADER_LEN, "buffer too small for header");
        buf[0] = self.version;
        buf[1] = self.flags;
        buf[2..6].copy_from_slice(&self.session.to_le_bytes());
        buf[6..8].copy_from_slice(&self.seq.to_le_bytes());
        buf[8..12].copy_from_slice(&self.timestamp.to_le_bytes());
    }

    /// Parse a header from the front of `buf`, or `None` if `buf` is too short.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < MEDIA_HEADER_LEN {
            return None;
        }
        Some(Self {
            version: buf[0],
            flags: buf[1],
            session: u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]),
            seq: u16::from_le_bytes([buf[6], buf[7]]),
            timestamp: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
        })
    }

    pub fn is_keepalive(&self) -> bool {
        self.flags & flags::KEEPALIVE != 0
    }
    pub fn is_punch(&self) -> bool {
        self.flags & flags::PUNCH != 0
    }
    pub fn is_reflex(&self) -> bool {
        self.flags & flags::REFLEX != 0
    }
    pub fn is_stereo(&self) -> bool {
        self.flags & flags::STEREO != 0
    }
}

// ---------------------------------------------------------------------------
// Hole-punch probe (payload when `flags::PUNCH` is set)
// ---------------------------------------------------------------------------

/// `nonce` authorizes the probe (must match the receiver's `local_nonce`, delivered
/// confidentially over the signaling TLS channel); `txid` round-trips so the sender can
/// confirm a bidirectional path on the exact source address the ACK arrives from.
pub const PUNCH_MAGIC: [u8; 4] = *b"ST2P";
pub const PUNCH_HDR_LEN: usize = 4 + 1 + 8 + 8; // magic | kind | nonce_le | txid_le = 21
/// MTU-validating probe size: a nominated path must carry a full-size datagram.
pub const PUNCH_PADDED_LEN: usize = 1200;
pub const PUNCH_PROBE: u8 = 0;
pub const PUNCH_ACK: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PunchProbe {
    pub kind: u8, // PUNCH_PROBE | PUNCH_ACK
    pub nonce: u64,
    pub txid: u64,
}

impl PunchProbe {
    /// Write the probe into the first [`PUNCH_HDR_LEN`] bytes of `buf`.
    pub fn encode(&self, buf: &mut [u8]) {
        assert!(buf.len() >= PUNCH_HDR_LEN, "buffer too small for punch probe");
        buf[0..4].copy_from_slice(&PUNCH_MAGIC);
        buf[4] = self.kind;
        buf[5..13].copy_from_slice(&self.nonce.to_le_bytes());
        buf[13..21].copy_from_slice(&self.txid.to_le_bytes());
    }

    /// Parse a probe, or `None` if too short / bad magic.
    pub fn decode(p: &[u8]) -> Option<Self> {
        if p.len() < PUNCH_HDR_LEN || p[0..4] != PUNCH_MAGIC {
            return None;
        }
        Some(Self {
            kind: p[4],
            nonce: u64::from_le_bytes(p[5..13].try_into().ok()?),
            txid: u64::from_le_bytes(p[13..21].try_into().ok()?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_header_roundtrip() {
        let h = MediaHeader::new(0xDEAD_BEEF, 0x1234, 0x89AB_CDEF, flags::STEREO);
        let mut buf = [0u8; 32];
        h.encode(&mut buf);
        let got = MediaHeader::decode(&buf).unwrap();
        assert_eq!(h, got);
        assert_eq!(got.version, PROTO_VERSION);
        assert!(got.is_stereo());
        assert!(!got.is_keepalive());
    }

    #[test]
    fn media_header_wire_layout() {
        let h = MediaHeader::new(0x04030201, 0x0605, 0x0A090807, 0);
        let mut buf = [0u8; MEDIA_HEADER_LEN];
        h.encode(&mut buf);
        assert_eq!(buf[0], PROTO_VERSION);
        assert_eq!(buf[1], 0);
        assert_eq!(&buf[2..6], &[0x01, 0x02, 0x03, 0x04]); // session LE
        assert_eq!(&buf[6..8], &[0x05, 0x06]); // seq LE
        assert_eq!(&buf[8..12], &[0x07, 0x08, 0x09, 0x0A]); // ts LE
    }

    #[test]
    fn decode_rejects_short() {
        assert!(MediaHeader::decode(&[0u8; MEDIA_HEADER_LEN - 1]).is_none());
    }

    #[test]
    fn punch_roundtrip_and_magic() {
        let p = PunchProbe { kind: PUNCH_ACK, nonce: 0x1122_3344_5566_7788, txid: 0x99 };
        let mut buf = [0u8; PUNCH_PADDED_LEN];
        p.encode(&mut buf);
        assert_eq!(PunchProbe::decode(&buf), Some(p));
        // A datagram that isn't ours must be rejected, not misparsed.
        let mut bad = buf;
        bad[0] = b'X';
        assert!(PunchProbe::decode(&bad).is_none());
    }

    #[test]
    fn control_json_tagged() {
        let m = ClientMsg::Join { room: "general".into() };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"t":"Join","room":"general"}"#);
        assert_eq!(serde_json::from_str::<ClientMsg>(&s).unwrap(), m);
    }

    #[test]
    fn welcome_json_roundtrip() {
        let m = ServerMsg::Welcome { session: 42, reflex: "empire:40001".into() };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<ServerMsg>(&s).unwrap(), m);
    }
}
