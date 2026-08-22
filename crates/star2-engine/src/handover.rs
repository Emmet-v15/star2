use std::net::UdpSocket;
use std::os::windows::io::{AsRawSocket, FromRawSocket};

use anyhow::{bail, Result};
use windows::Win32::Networking::WinSock::{
    WSADuplicateSocketW, WSAGetLastError, WSASocketW, INVALID_SOCKET, SOCKET, WSAPROTOCOL_INFOW,
    WSA_FLAG_OVERLAPPED,
};

const FROM_PROTOCOL_INFO: i32 = -1;

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
    use std::net::UdpSocket;
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

    #[test]
    fn a_truncated_handover_is_refused_instead_of_adopted() {
        let original = UdpSocket::bind("127.0.0.1:0").unwrap();
        let blob = export(&original, std::process::id()).unwrap();
        assert!(import(&blob[..blob.len() - 1]).is_err(), "a short blob was treated as a socket");
        assert!(import(&[]).is_err(), "an empty blob was treated as a socket");
    }
}
