//! Mark our media as interactive voice so queues along the path prioritise it.
//!
//! The goal is DSCP **EF** (Expedited Forwarding, 46) in the IP header. Consumer
//! Wi-Fi is where this actually pays: an access point implementing WMM maps EF to
//! the Voice access class (AC_VO), which gets a shorter contention window than
//! bulk traffic. That directly attacks local queueing jitter - which, on a link
//! whose jitter sets our buffer depth, is latency.
//!
//! It does nothing to transit jitter out on the open internet; no host-side marking
//! can, because ISPs rewrite or ignore DSCP at the edge. Treat this as a fix for the
//! first and last hop only.
//!
//! Platform split, and the reason this file exists at all:
//!   * Unix/macOS/Android - `setsockopt(IP_TOS)` works; one call at bind time.
//!   * Windows - `setsockopt(IP_TOS)` is **silently ignored** (since XP SP2, to stop
//!     applications marking their own traffic as important). The supported route is
//!     qWAVE, which needs the destination address, so it can only be applied once
//!     the punch has told us where the peer actually is.

use std::net::{SocketAddr, UdpSocket};

/// DSCP EF (46) sits in the top 6 bits of the TOS byte: 46 << 2 = 0xB8.
#[cfg(not(windows))]
const DSCP_EF_TOS: u32 = 46 << 2;

/// Mark the socket at bind time. Unix only; on Windows this is a no-op and the
/// real work happens in [`mark_peer_flow`] once the peer address is known.
#[cfg(not(windows))]
pub(crate) fn mark_socket(sock: &UdpSocket) {
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    // SAFETY: we rebuild a socket2::Socket over the same fd purely to reach its
    // setter, then leak it back out so the original UdpSocket keeps ownership.
    unsafe {
        let s = socket2::Socket::from_raw_fd(sock.as_raw_fd());
        let _ = s.set_tos(DSCP_EF_TOS);
        let _ = s.into_raw_fd();
    }
}

#[cfg(windows)]
pub(crate) fn mark_socket(_sock: &UdpSocket) {}

#[cfg(not(windows))]
pub(crate) fn mark_peer_flow(_sock: &UdpSocket, _peer: SocketAddr) {}

/// Windows: register the socket with qWAVE as a voice flow to the confirmed peer.
///
/// The QOS handle must outlive the flow, so it is deliberately leaked - the flow
/// lasts as long as the call, and the process exits when the call does.
#[cfg(windows)]
pub(crate) fn mark_peer_flow(sock: &UdpSocket, peer: SocketAddr) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::NetworkManagement::QoS::{
        QOSAddSocketToFlow, QOSCreateHandle, QOSTrafficTypeVoice, QOS_NON_ADAPTIVE_FLOW,
        QOS_VERSION,
    };
    use windows::Win32::Networking::WinSock::{SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKET};

    let version = QOS_VERSION { MajorVersion: 1, MinorVersion: 0 };
    let mut handle = windows::Win32::Foundation::HANDLE::default();
    // qWAVE returns BOOL, not Result: false means the service is unavailable
    // (it is absent on Server SKUs and can be disabled by policy).
    if !unsafe { QOSCreateHandle(&version, &mut handle) }.as_bool() {
        crate::log_line("[qos] qWAVE unavailable; media left unmarked".into());
        return;
    }

    // Build the destination sockaddr qWAVE needs to identify the flow.
    let (addr_ptr, addr_len) = match peer {
        SocketAddr::V4(v4) => {
            let mut sa = SOCKADDR_IN::default();
            sa.sin_family = windows::Win32::Networking::WinSock::AF_INET;
            sa.sin_port = v4.port().to_be();
            sa.sin_addr.S_un.S_addr = u32::from_ne_bytes(v4.ip().octets());
            (
                Box::into_raw(Box::new(sa)) as *const SOCKADDR,
                std::mem::size_of::<SOCKADDR_IN>() as u32,
            )
        }
        SocketAddr::V6(v6) => {
            let mut sa = SOCKADDR_IN6::default();
            sa.sin6_family = windows::Win32::Networking::WinSock::AF_INET6;
            sa.sin6_port = v6.port().to_be();
            sa.sin6_addr.u.Byte = v6.ip().octets();
            (
                Box::into_raw(Box::new(sa)) as *const SOCKADDR,
                std::mem::size_of::<SOCKADDR_IN6>() as u32,
            )
        }
    };

    let mut flow_id: u32 = 0; // 0 = "create a new flow"
    let ok = unsafe {
        QOSAddSocketToFlow(
            handle,
            SOCKET(sock.as_raw_socket() as usize),
            Some(addr_ptr),
            QOSTrafficTypeVoice,
            Some(QOS_NON_ADAPTIVE_FLOW),
            &mut flow_id,
        )
    };
    let _ = addr_len;
    if ok.as_bool() {
        crate::log_line(format!("[qos] media marked DSCP EF (voice) -> {peer}"));
    } else {
        crate::log_line(format!(
            "[qos] qWAVE flow rejected ({}); media unmarked",
            std::io::Error::last_os_error()
        ));
    }
    // `handle` is deliberately never passed to QOSCloseHandle: closing it tears the
    // flow down, and the flow must last as long as the call does.
}
