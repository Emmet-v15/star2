use std::net::{SocketAddr, UdpSocket};
use std::os::windows::io::{AsRawSocket, FromRawSocket};
use std::process::{Child, Command};
use std::sync::Once;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use star2_proto::SessionId;
use windows::Win32::Networking::WinSock::{
    WSADuplicateSocketW, WSAGetLastError, WSASocketW, WSAStartup, INVALID_SOCKET, SOCKET, WSADATA,
    WSAPROTOCOL_INFOW, WSA_FLAG_OVERLAPPED,
};

const FROM_PROTOCOL_INFO: i32 = -1;
const WINSOCK_2_2: u16 = 0x0202;

pub const FLAG: &str = "--handover";

const CTL_ENV: &str = "STAR2_HANDOVER_CTL";
const LOOPBACK: &str = "127.0.0.1:0";
const CTL_BUF: usize = 8192;
const HELLO: &[u8] = b"star2-handover-hello";
const READY: &[u8] = b"star2-handover-ready";
const READY_TIMEOUT: Duration = Duration::from_secs(20);

fn start_winsock_if_the_adopt_is_the_first_socket_call() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut data = WSADATA::default();
        unsafe { WSAStartup(WINSOCK_2_2, &mut data) };
    });
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallState {
    pub socket: String,
    pub peer_addr: SocketAddr,
    pub media_session: SessionId,
    pub peer_session: SessionId,
    pub local_nonce: u64,
    pub remote_nonce: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Cutover {
    pub seq: u16,
    pub ts: u32,
}

pub const BLOB_LEN: usize = std::mem::size_of::<WSAPROTOCOL_INFOW>();

pub fn export(sock: &UdpSocket, target_pid: u32) -> Result<Vec<u8>> {
    let mut info = WSAPROTOCOL_INFOW::default();
    let rc = unsafe {
        WSADuplicateSocketW(
            SOCKET(sock.as_raw_socket() as usize),
            target_pid,
            &mut info as *mut _,
        )
    };
    if rc != 0 {
        bail!("hand the call socket to pid {target_pid}: winsock error {:?}", unsafe {
            WSAGetLastError()
        });
    }
    let bytes = unsafe { std::slice::from_raw_parts(&info as *const _ as *const u8, BLOB_LEN) };
    Ok(bytes.to_vec())
}

pub fn import(blob: &[u8]) -> Result<UdpSocket> {
    start_winsock_if_the_adopt_is_the_first_socket_call();
    if blob.len() != BLOB_LEN {
        bail!("call socket handover is {} bytes, expected {BLOB_LEN}", blob.len());
    }
    let mut info = WSAPROTOCOL_INFOW::default();
    unsafe {
        std::ptr::copy_nonoverlapping(blob.as_ptr(), &mut info as *mut _ as *mut u8, BLOB_LEN);
    }
    let sock = unsafe {
        WSASocketW(
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            Some(&info as *const _),
            0,
            WSA_FLAG_OVERLAPPED,
        )
    };
    let sock = sock.unwrap_or(INVALID_SOCKET);
    if sock == INVALID_SOCKET {
        bail!("adopt the call socket: winsock error {:?}", unsafe { WSAGetLastError() });
    }
    Ok(unsafe { UdpSocket::from_raw_socket(sock.0 as u64) })
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        bail!("call socket handover has an odd number of hex digits");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("call socket handover is not hex"))
        .collect()
}

pub struct Successor {
    child: Child,
    ctl: UdpSocket,
    child_ctl: SocketAddr,
}

pub fn spawn_successor(mut cmd: Command) -> Result<Successor> {
    let ctl = UdpSocket::bind(LOOPBACK)?;
    ctl.set_read_timeout(Some(READY_TIMEOUT))?;
    cmd.env(CTL_ENV, ctl.local_addr()?.port().to_string());

    let child = cmd.spawn().context("start the new build")?;
    let mut buf = [0u8; CTL_BUF];
    let (n, child_ctl) = match ctl.recv_from(&mut buf) {
        Ok(v) => v,
        Err(e) => {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(e).context("the new build never came up");
        }
    };
    if &buf[..n] != HELLO {
        bail!("the new build did not answer the handover");
    }
    Ok(Successor { child, ctl, child_ctl })
}

impl Successor {
    pub fn offer(&mut self, sock: &UdpSocket, mut state: CallState) -> Result<()> {
        state.socket = to_hex(&export(sock, self.child.id())?);
        let json = serde_json::to_vec(&state)?;
        self.ctl.send_to(&json, self.child_ctl).context("offer the call to the new build")?;
        Ok(())
    }

    pub fn wait_until_ready(&mut self) -> Result<()> {
        let mut buf = [0u8; CTL_BUF];
        let (n, _) = self.ctl.recv_from(&mut buf).context("the new build never took the call")?;
        if &buf[..n] != READY {
            bail!("the new build did not take the call");
        }
        Ok(())
    }

    pub fn cut_over(&mut self, at: Cutover) -> Result<()> {
        let json = serde_json::to_vec(&at)?;
        self.ctl.send_to(&json, self.child_ctl).context("release the stream to the new build")?;
        Ok(())
    }

    pub fn abandon(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Inherited {
    pub sock: UdpSocket,
    pub call: CallState,
}

pub struct Predecessor {
    ctl: UdpSocket,
    parent_ctl: SocketAddr,
}

pub fn answer_predecessor() -> Result<Predecessor> {
    let port: u16 = std::env::var(CTL_ENV)
        .context("started with --handover but the old build set no control port")?
        .parse()
        .context("the old build set an unreadable control port")?;
    let parent_ctl = SocketAddr::from(([127, 0, 0, 1], port));
    let ctl = UdpSocket::bind(LOOPBACK)?;
    ctl.set_read_timeout(Some(READY_TIMEOUT))?;
    ctl.send_to(HELLO, parent_ctl).context("answer the old build")?;
    Ok(Predecessor { ctl, parent_ctl })
}

impl Predecessor {
    pub fn inherit_call(&mut self) -> Result<Inherited> {
        let mut buf = vec![0u8; CTL_BUF];
        let (n, _) = self.ctl.recv_from(&mut buf).context("wait for the offered call")?;
        let call: CallState =
            serde_json::from_slice(&buf[..n]).context("parse the offered call")?;
        let sock = import(&from_hex(&call.socket)?)?;
        Ok(Inherited { sock, call })
    }

    pub fn announce_ready(&mut self) -> Result<()> {
        self.ctl.send_to(READY, self.parent_ctl).context("tell the old build we have the call")?;
        Ok(())
    }

    pub fn wait_for_cutover(&mut self) -> Result<Cutover> {
        let mut buf = [0u8; CTL_BUF];
        let (n, _) = self.ctl.recv_from(&mut buf).context("wait for the stream")?;
        serde_json::from_slice(&buf[..n]).context("parse the stream handover")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn an_adopted_socket_keeps_the_port_the_peer_is_sending_to() {
        let original = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = original.local_addr().unwrap();

        let blob = export(&original, std::process::id()).unwrap();
        let adopted = import(&blob).unwrap();

        assert_eq!(
            adopted.local_addr().unwrap(),
            addr,
            "the new build bound a different port, so the peer's packets go nowhere"
        );

        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.send_to(b"mid-call", addr).unwrap();

        adopted.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 16];
        let (n, _) = adopted
            .recv_from(&mut buf)
            .expect("a packet sent to the old port never reached the new build");
        assert_eq!(&buf[..n], b"mid-call");
    }

    #[test]
    fn the_old_build_can_still_carry_the_call_after_handing_it_over() {
        let original = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = original.local_addr().unwrap();

        let blob = export(&original, std::process::id()).unwrap();
        let adopted = import(&blob).unwrap();
        drop(adopted);

        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.send_to(b"rollback", addr).unwrap();

        original.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 16];
        let (n, _) = original
            .recv_from(&mut buf)
            .expect("a failed update killed the call it was supposed to survive");
        assert_eq!(&buf[..n], b"rollback");
    }

    const CHILD: &str = "STAR2_HANDOVER_CHILD";
    const ECHO: &str = "a_freshly_spawned_process_can_receive_on_the_inherited_port";

    fn successor_running(test: &str) -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", &format!("handover::tests::{test}"), "--nocapture"]).env(CHILD, "1");
        cmd
    }

    #[test]
    fn a_freshly_spawned_process_can_receive_on_the_inherited_port() {
        if std::env::var(CHILD).is_ok() {
            return echo_duties();
        }

        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let call = UdpSocket::bind("127.0.0.1:0").unwrap();
        let call_addr = call.local_addr().unwrap();

        let mut successor = spawn_successor(successor_running(ECHO)).unwrap();
        successor
            .offer(
                &call,
                CallState {
                    socket: String::new(),
                    peer_addr,
                    media_session: 7,
                    peer_session: 9,
                    local_nonce: 11,
                    remote_nonce: 13,
                },
            )
            .unwrap();
        successor.wait_until_ready().expect("the new process never took the call");
        successor.cut_over(Cutover { seq: 0, ts: 0 }).unwrap();

        drop(call);
        peer.send_to(b"mid-call", call_addr).unwrap();

        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 64];
        let (n, src) = peer
            .recv_from(&mut buf)
            .expect("the old build let go and the peer's packet reached nobody");
        successor.abandon();

        assert_eq!(&buf[..n], b"mid-call", "the new build garbled what the peer sent");
        assert_eq!(
            src, call_addr,
            "the new build answered from a different port, so a real NAT would have dropped it"
        );
    }

    fn echo_duties() {
        let mut predecessor = answer_predecessor().expect("answer the old build");
        let inherited = predecessor.inherit_call().expect("inherit the call");
        predecessor.announce_ready().unwrap();
        predecessor.wait_for_cutover().expect("wait for the stream");

        inherited.sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut buf = [0u8; 64];
        let (n, _) = inherited.sock.recv_from(&mut buf).expect("hear the peer on the old port");
        let _ = inherited.sock.send_to(&buf[..n], inherited.call.peer_addr);
        std::process::exit(0);
    }

    const TAKEOVER: &str = "the_peer_hears_one_unbroken_stream_across_a_process_handover";
    const TICK: Duration = Duration::from_millis(2);
    const SUCCESSOR_PACKETS: u16 = 200;

    #[test]
    fn the_peer_hears_one_unbroken_stream_across_a_process_handover() {
        if std::env::var(CHILD).is_ok() {
            return successor_duties();
        }

        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let call = UdpSocket::bind("127.0.0.1:0").unwrap();
        let call_addr = call.local_addr().unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let cursor = Arc::new(AtomicU32::new(0));
        let streaming = {
            let (sock, stop, cursor) = (call.try_clone().unwrap(), stop.clone(), cursor.clone());
            std::thread::spawn(move || {
                let mut seq = 0u16;
                while !stop.load(Ordering::Relaxed) {
                    let _ = sock.send_to(&stamp(OLD_BUILD, seq), peer_addr);
                    seq = seq.wrapping_add(1);
                    cursor.store(seq as u32, Ordering::Relaxed);
                    std::thread::sleep(TICK);
                }
            })
        };
        while cursor.load(Ordering::Relaxed) < 20 {
            std::thread::sleep(TICK);
        }

        let mut successor = spawn_successor(successor_running(TAKEOVER)).unwrap();
        successor
            .offer(
                &call,
                CallState {
                    socket: String::new(),
                    peer_addr,
                    media_session: 7,
                    peer_session: 9,
                    local_nonce: 11,
                    remote_nonce: 13,
                },
            )
            .unwrap();
        successor.wait_until_ready().expect("the new process never took the call");

        stop.store(true, Ordering::Relaxed);
        streaming.join().unwrap();

        let at = Cutover { seq: cursor.load(Ordering::Relaxed) as u16, ts: 0 };
        successor.cut_over(at).unwrap();
        drop(call);

        let mut heard: HashSet<u16> = HashSet::new();
        let mut from_old: HashSet<u16> = HashSet::new();
        let mut from_new: HashSet<u16> = HashSet::new();
        let mut sources = HashSet::new();
        peer.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let mut buf = [0u8; 8];
        while let Ok((n, src)) = peer.recv_from(&mut buf) {
            if n != 3 {
                continue;
            }
            let seq = u16::from_be_bytes([buf[1], buf[2]]);
            heard.insert(seq);
            sources.insert(src);
            match buf[0] {
                OLD_BUILD => from_old.insert(seq),
                _ => from_new.insert(seq),
            };
        }
        successor.abandon();

        let top = *heard.iter().max().expect("the peer heard nothing at all");
        let missing: Vec<u16> = (0..=top).filter(|s| !heard.contains(s)).collect();
        assert!(
            missing.is_empty(),
            "the peer heard a hole at seq {:?} - the cutover dropped audio",
            &missing[..missing.len().min(12)]
        );

        let last_old = *from_old.iter().max().expect("the old build never streamed");
        let first_new = *from_new.iter().min().expect("the new build never streamed");
        let last_new = *from_new.iter().max().unwrap();
        assert!(
            first_new >= at.seq,
            "the new build rewound the stream to seq {first_new} after taking it over at \
             {} - the peer would read that as a 65k-frame reorder and stall",
            at.seq
        );
        let doubled: Vec<u16> = {
            let mut d: Vec<u16> = from_old.intersection(&from_new).copied().collect();
            d.sort_unstable();
            d
        };
        assert!(
            doubled.is_empty(),
            "both builds sent seq {:?}, so the peer receives every frame in the seam twice and \
             its playout starves for about 0.4s",
            &doubled[..doubled.len().min(12)]
        );
        assert!(
            last_new > last_old,
            "the new build stopped at {last_new} before the old build's {last_old}, so it never \
             really took the stream over"
        );
        assert_eq!(
            sources,
            HashSet::from([call_addr]),
            "the peer saw the stream move to a new port, so a real NAT would have dropped it"
        );
    }

    const OLD_BUILD: u8 = 0;
    const NEW_BUILD: u8 = 1;

    fn stamp(build: u8, seq: u16) -> [u8; 3] {
        let [hi, lo] = seq.to_be_bytes();
        [build, hi, lo]
    }

    fn successor_duties() {
        let mut predecessor = answer_predecessor().expect("answer the old build");
        let inherited = predecessor.inherit_call().expect("inherit the call");
        predecessor.announce_ready().unwrap();
        let at = predecessor.wait_for_cutover().expect("wait for the stream");

        let mut seq = at.seq;
        for _ in 0..SUCCESSOR_PACKETS {
            let _ = inherited.sock.send_to(&stamp(NEW_BUILD, seq), inherited.call.peer_addr);
            seq = seq.wrapping_add(1);
            std::thread::sleep(TICK);
        }
        std::process::exit(0);
    }

    #[test]
    fn the_socket_survives_the_hex_round_trip_the_pipe_forces() {
        let original = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = original.local_addr().unwrap();
        let hex = to_hex(&export(&original, std::process::id()).unwrap());
        let adopted = import(&from_hex(&hex).unwrap()).unwrap();
        assert_eq!(adopted.local_addr().unwrap(), addr, "the blob did not survive the pipe");
    }
}
