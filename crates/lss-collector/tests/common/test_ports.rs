//! card #323: a port where NOTHING answers, for tests - shared by the collector's unit tests
//! (`#[path]` from main.rs) and the integration tests, like exec_file.rs.
//!
//! The flake (stress run, the full test binary at --test-threads=64): setup_run.rs 'Nothing is
//! listening' and tls.rs:456 failed in the SAME run. `free_port()` bound 127.0.0.1:0 and let the
//! socket go at once; the kernel then handed that number to another test's listener (tls_engine
//! and the Fake servers bind :0 too) before the wizard connected - so "nothing is listening" found
//! a live server, and took that server's one connection from the test waiting on it. Measured with
//! the ephemeral range cut to 256 ports: 164 of 300 runs failed before, 0 at these lines after.
//!
//! So the port stays BOUND for as long as the test relies on it: a socket with no SO_REUSEADDR
//! that never listens. A connect is refused, and neither a bind(:0) nor a rebind (std's listeners
//! set SO_REUSEADDR, which does not share with a socket that lacks it) can be given the port until
//! `Refusing` is dropped. Close-on-exec, so a child spawned by another test thread cannot keep it.
#![allow(dead_code)]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub struct Refusing {
    pub fd: OwnedFd,
    pub port: u16,
}

pub fn refusing_port() -> Refusing {
    #[cfg(target_os = "linux")]
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    #[cfg(not(target_os = "linux"))]
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    assert!(raw >= 0, "socket: {}", std::io::Error::last_os_error());
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    #[cfg(not(target_os = "linux"))]
    unsafe {
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let mut sa = libc::sockaddr_in {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr { s_addr: u32::from(std::net::Ipv4Addr::LOCALHOST).to_be() },
        sin_zero: [0; 8],
    };
    let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let rc = unsafe { libc::bind(fd.as_raw_fd(), &sa as *const libc::sockaddr_in as *const libc::sockaddr, len) };
    assert_eq!(rc, 0, "bind 127.0.0.1:0: {}", std::io::Error::last_os_error());
    let rc = unsafe { libc::getsockname(fd.as_raw_fd(), &mut sa as *mut libc::sockaddr_in as *mut libc::sockaddr, &mut len) };
    assert_eq!(rc, 0, "getsockname: {}", std::io::Error::last_os_error());
    Refusing { fd, port: u16::from_be(sa.sin_port) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_refusing_port_answers_nothing_and_cannot_be_handed_to_another_listener() {
        let held = refusing_port();
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], held.port));
        assert!(std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_err(), "nothing answers on {addr}");
        // what took the port in the stress run: another test's listener (std sets SO_REUSEADDR)
        assert_eq!(std::net::TcpListener::bind(addr).err().map(|e| e.kind()), Some(std::io::ErrorKind::AddrInUse), "{addr} is held while the test runs");
        // and a child spawned meanwhile does not inherit it
        let flags = unsafe { libc::fcntl(held.fd.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0, "close-on-exec");
        drop(held.fd);
    }
}
