//! Socket tuning for low latency over USB (adb) and Wi-Fi.

use std::net::TcpStream;
use std::os::fd::AsRawFd;

/// Keeps the *unsent* part of the send queue short (TCP_NOTSENT_LOWAT): blocking writes
/// wait once ~128 KB is queued, while data in flight (bounded by the congestion window)
/// is not limited, so high-latency Wi-Fi still gets full throughput. Also asks for BBR,
/// which doesn't collapse its window after idle periods (only if the system allows it).
pub fn tune_for_latency(sock: &TcpStream) {
    let fd = sock.as_raw_fd();
    let lowat: libc::c_int = 128 * 1024;
    let ok = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NOTSENT_LOWAT,
            (&lowat as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    } == 0;
    if !ok {
        log::warn!("TCP_NOTSENT_LOWAT: {}", std::io::Error::last_os_error());
    }
    let bbr = b"bbr";
    let ok = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_CONGESTION,
            bbr.as_ptr().cast(),
            bbr.len() as libc::socklen_t,
        )
    } == 0;
    log::debug!("TCP congestion control: {}", if ok { "bbr" } else { "system default" });
}

/// Bytes queued on the socket but not sent yet (SIOCOUTQNSD). Data in flight is not
/// counted: on Wi-Fi it's normal to have tens of ms of it waiting for (delayed) ACKs.
pub fn not_sent_bytes(sock: &TcpStream) -> u32 {
    let mut n: libc::c_int = 0;
    let ret = unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCOUTQNSD as _, &mut n) };
    if ret == 0 { n.max(0) as u32 } else { 0 }
}

/// Kernel view of the connection: smoothed round trip (ms) and congestion window
/// (segments), from TCP_INFO.
pub fn tcp_rtt_cwnd(sock: &TcpStream) -> Option<(f32, u32)> {
    // struct tcp_info: 8 bytes of u8 fields, then u32s; rtt is u32 #15 (µs), snd_cwnd #18.
    let mut buf = [0u8; 256];
    let mut len = buf.len() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if ret != 0 || (len as usize) < 8 + 4 * 19 {
        return None;
    }
    let u32_at = |i: usize| u32::from_ne_bytes(buf[8 + 4 * i..12 + 4 * i].try_into().unwrap());
    Some((u32_at(15) as f32 / 1000.0, u32_at(18)))
}

/// Whether Linux collapses a TCP connection's window after ~200 ms without sending
/// (`net.ipv4.tcp_slow_start_after_idle`). With a damage-driven stream that's every
/// pause, and the next frames crawl back over several Wi-Fi round trips.
pub fn slow_start_after_idle() -> bool {
    std::fs::read_to_string("/proc/sys/net/ipv4/tcp_slow_start_after_idle")
        .is_ok_and(|s| s.trim() == "1")
}
