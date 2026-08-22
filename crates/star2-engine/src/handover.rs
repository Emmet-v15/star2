use std::net::UdpSocket;
use std::os::windows::io::{AsRawSocket, FromRawSocket};
use std::sync::Once;

use anyhow::{bail, Result};
use windows::Win32::Networking::WinSock::{
    WSADuplicateSocketW, WSAGetLastError, WSASocketW, WSAStartup, INVALID_SOCKET, SOCKET, WSADATA,
    WSAPROTOCOL_INFOW, WSA_FLAG_OVERLAPPED,
};

const FROM_PROTOCOL_INFO: i32 = -1;
const WINSOCK_2_2: u16 = 0x0202;

fn start_winsock_if_the_adopt_is_the_first_socket_call() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut data = WSADATA::default();
        unsafe { WSAStartup(WINSOCK_2_2, &mut data) };
    });
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
    let bytes = unsafe {
        std::slice::from_raw_parts(&info as *const _ as *const u8, BLOB_LEN)
    };
    Ok(bytes.to_vec())
}

pub fn import(blob: &[u8]) -> Result<UdpSocket> {
    start_winsock_if_the_adopt_is_the_first_socket_call();
    if blob.len() != BLOB_LEN {
        bail!("call socket handover is {} bytes, expected {BLOB_LEN}", blob.len());
    }
    let mut info = WSAPROTOCOL_INFOW::default();
    unsafe {
        std::ptr::copy_nonoverlapping(
            blob.as_ptr(),
            &mut info as *mut _ as *mut u8,
            BLOB_LEN,
        );
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

#[cfg(test)]
mod tests {
    use super::{export, import};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::UdpSocket;
    use std::process::{Command, Stdio};
    use std::time::Duration;

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

    #[test]
    fn a_freshly_spawned_process_can_carry_the_call_on_the_same_port() {
        if std::env::var(CHILD).is_ok() {
            return adopt_and_report();
        }

        let call = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = call.local_addr().unwrap();

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "handover::tests::a_freshly_spawned_process_can_carry_the_call_on_the_same_port",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();

        let blob = export(&call, child.id()).expect("export to a real child process");
        child.stdin.take().unwrap().write_all(&blob).unwrap();

        let mut out = BufReader::new(child.stdout.take().unwrap());
        let port = read_marked(&mut out, "adopted:");
        assert_eq!(
            port,
            addr.port().to_string(),
            "the new process bound a different port, so the peer's packets go nowhere"
        );

        drop(call);

        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.send_to(b"mid-call", addr).unwrap();

        assert_eq!(
            read_marked(&mut out, "heard:"),
            "mid-call",
            "the old process let go and the packet reached nobody"
        );
        child.wait().unwrap();
    }

    fn adopt_and_report() {
        let mut blob = vec![0u8; super::BLOB_LEN];
        std::io::stdin().read_exact(&mut blob).unwrap();
        let adopted = import(&blob).expect("adopt a socket duplicated by another process");

        println!("adopted:{}", adopted.local_addr().unwrap().port());
        std::io::stdout().flush().unwrap();

        adopted.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut buf = [0u8; 64];
        let (n, _) = adopted.recv_from(&mut buf).unwrap();
        println!("heard:{}", String::from_utf8_lossy(&buf[..n]));
        std::io::stdout().flush().unwrap();
        std::process::exit(0);
    }

    fn read_marked(out: &mut impl BufRead, marker: &str) -> String {
        let mut line = String::new();
        loop {
            line.clear();
            let n = out.read_line(&mut line).unwrap();
            assert!(n > 0, "the new process died before it reported {marker}");
            if let Some(rest) = line.trim_end().strip_prefix(marker) {
                return rest.to_string();
            }
        }
    }

    #[test]
    fn a_truncated_handover_is_refused_instead_of_adopted() {
        let original = UdpSocket::bind("127.0.0.1:0").unwrap();
        let blob = export(&original, std::process::id()).unwrap();
        assert!(import(&blob[..blob.len() - 1]).is_err(), "a short blob was treated as a socket");
        assert!(import(&[]).is_err(), "an empty blob was treated as a socket");
    }
}
