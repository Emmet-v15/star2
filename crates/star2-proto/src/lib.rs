use serde::{Deserialize, Serialize};

mod room;
pub use room::{is_room_token, new_room_token, room_label};

pub const PROTO_VERSION: u8 = 1;

pub const MEDIA_HEADER_LEN: usize = 12;

pub type SessionId = u32;

pub mod flags {
    pub const REFLEX: u8 = 0b0000_0001;

    pub const RED_WANTED: u8 = 0b0000_0010;

    pub const RED: u8 = 0b0000_0100;

    pub const KEEPALIVE: u8 = 0b0000_1000;

    pub const STEREO: u8 = 0b0100_0000;

    pub const PUNCH: u8 = 0b1000_0000;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ClientMsg {
    Hello { name: String, ver: u32, token: String, #[serde(default)] build: String },

    Join { room: String },

    Stats {
        loss_pct: f32,
        jitter_ms: f32,
        buf_ms: u32,
        out_ms: u32,
        rx_pps: u32,
        play_fps: u32,
        late_pps: u32,
        resyncs: u32,
        expand_pps: u32,
        tx_pps: u32,
        mic_db: f32,
        #[serde(default)]
        dev_in_ms: f32,
        #[serde(default)]
        in_ring_ms: f32,
        #[serde(default)]
        enc_ms: f32,
        #[serde(default)]
        rtt_ms: f32,
        #[serde(default)]
        jb_ms: f32,
        #[serde(default)]
        dev_out_ms: f32,
        #[serde(default)]
        tx_path_ms: f32,
        #[serde(default)]
        rx_path_ms: f32,
        #[serde(default)]
        path: String,
    },

    P2pOffer { to: SessionId, nonce: u64, cands: Vec<String> },

    P2pAnswer { to: SessionId, nonce: u64, cands: Vec<String> },

    P2pCandidate { to: SessionId, cand: String },

    P2pAbort { to: SessionId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ServerMsg {
    Welcome { session: SessionId, reflex: String },

    Room { room: String, members: Vec<Member> },

    Joined { session: SessionId, name: String },

    Left { session: SessionId },

    Error { msg: String },

    P2pOffer { from: SessionId, nonce: u64, cands: Vec<String> },
    P2pAnswer { from: SessionId, nonce: u64, cands: Vec<String> },
    P2pCandidate { from: SessionId, cand: String },
    P2pAbort { from: SessionId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub session: SessionId,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaHeader {
    pub version: u8,
    pub flags: u8,
    pub session: SessionId,
    pub seq: u16,
    pub timestamp: u32,
}

impl MediaHeader {
    pub fn new(session: SessionId, seq: u16, timestamp: u32, flags: u8) -> Self {
        Self { version: PROTO_VERSION, flags, session, seq, timestamp }
    }

    pub fn encode(&self, buf: &mut [u8]) {
        assert!(buf.len() >= MEDIA_HEADER_LEN, "buffer too small for header");
        buf[0] = self.version;
        buf[1] = self.flags;
        buf[2..6].copy_from_slice(&self.session.to_le_bytes());
        buf[6..8].copy_from_slice(&self.seq.to_le_bytes());
        buf[8..12].copy_from_slice(&self.timestamp.to_le_bytes());
    }

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
    pub fn red_wanted(&self) -> bool {
        self.flags & flags::RED_WANTED != 0
    }
    pub fn is_red(&self) -> bool {
        self.flags & flags::RED != 0
    }
}

pub const RED_LEN_BYTES: usize = 2;

pub fn split_red(payload: &[u8]) -> Option<(&[u8], &[u8])> {
    if payload.len() < RED_LEN_BYTES {
        return None;
    }
    let n = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    let rest = &payload[RED_LEN_BYTES..];
    if n == 0 || n >= rest.len() {
        return None;
    }
    Some((&rest[..n], &rest[n..]))
}

pub const PUNCH_MAGIC: [u8; 4] = *b"ST2P";
pub const PUNCH_HDR_LEN: usize = 4 + 1 + 8 + 8;

pub const PUNCH_PADDED_LEN: usize = 1200;
pub const PUNCH_PROBE: u8 = 0;
pub const PUNCH_ACK: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PunchProbe {
    pub kind: u8,
    pub nonce: u64,
    pub txid: u64,
}

impl PunchProbe {
    pub fn encode(&self, buf: &mut [u8]) {
        assert!(buf.len() >= PUNCH_HDR_LEN, "buffer too small for punch probe");
        buf[0..4].copy_from_slice(&PUNCH_MAGIC);
        buf[4] = self.kind;
        buf[5..13].copy_from_slice(&self.nonce.to_le_bytes());
        buf[13..21].copy_from_slice(&self.txid.to_le_bytes());
    }

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
    fn a_hello_without_build_is_a_stale_client() {
        let old = r#"{"t":"Hello","name":"pc","ver":1,"token":"x"}"#;
        let got: ClientMsg = serde_json::from_str(old).unwrap();
        assert_eq!(
            got,
            ClientMsg::Hello {
                name: "pc".into(),
                ver: 1,
                token: "x".into(),
                build: String::new(),
            }
        );
    }

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
        assert_eq!(&buf[2..6], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(&buf[6..8], &[0x05, 0x06]);
        assert_eq!(&buf[8..12], &[0x07, 0x08, 0x09, 0x0A]);
    }

    #[test]
    fn decode_rejects_short() {
        assert!(MediaHeader::decode(&[0u8; MEDIA_HEADER_LEN - 1]).is_none());
    }

    #[test]
    fn red_payload_round_trips() {
        let (prev, cur) = (&[1u8, 2, 3][..], &[9u8, 8][..]);
        let mut buf = (prev.len() as u16).to_be_bytes().to_vec();
        buf.extend_from_slice(prev);
        buf.extend_from_slice(cur);
        assert_eq!(split_red(&buf), Some((prev, cur)));
    }

    #[test]
    fn red_payload_rejects_junk() {
        assert_eq!(split_red(&[]), None);
        assert_eq!(split_red(&[0, 1]), None);
        assert_eq!(split_red(&[0, 0, 7, 7]), None);
        assert_eq!(split_red(&[0, 9, 7, 7]), None);
        assert_eq!(split_red(&[0, 2, 7, 7]), None);
    }

    #[test]
    fn punch_roundtrip_and_magic() {
        let p = PunchProbe { kind: PUNCH_ACK, nonce: 0x1122_3344_5566_7788, txid: 0x99 };
        let mut buf = [0u8; PUNCH_PADDED_LEN];
        p.encode(&mut buf);
        assert_eq!(PunchProbe::decode(&buf), Some(p));

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
