//! Sockets and the one way every blocking thread in here is told to stop.

use crate::hub::Msg;
use std::net::UdpSocket;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

/// An eventfd that, once raised, stays readable: every thread sleeping in
/// [`StopSignal::wait_readable`] wakes up and sees it, now or later. This is what
/// lets the accept and receive threads sleep without a timeout.
pub(crate) struct StopSignal(OwnedFd);

impl StopSignal {
    pub fn new() -> std::io::Result<Self> {
        // SAFETY: plain eventfd(2); the result is checked and owned from here on.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor nobody else owns.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn raise(&self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: writes 8 bytes from a valid buffer to an eventfd. It cannot fail in a
        // way that matters: a full counter is still readable.
        unsafe { libc::write(self.0.as_raw_fd(), one.as_ptr().cast(), one.len()) };
    }

    /// Sleeps until `fd` has something to read (`true`) or the signal is raised
    /// (`false`).
    pub fn wait_readable(&self, fd: RawFd) -> bool {
        loop {
            let mut fds = [
                libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.0.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: two valid pollfds; no timeout, so the thread costs nothing asleep.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if ready < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    tracing::warn!(error = %e, "poll failed");
                    std::thread::sleep(Duration::from_millis(100));
                }
                continue;
            }
            if fds[1].revents != 0 {
                return false;
            }
            if fds[0].revents != 0 {
                return true;
            }
        }
    }
}

/// A keyframe leaves as a burst of a few hundred datagrams per viewer; make room.
pub(crate) fn tune_media_socket(socket: &UdpSocket) {
    let size: libc::c_int = 2 << 20;
    // SAFETY: plain setsockopt with a valid int.
    let r = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&raw const size).cast(),
            size_of_val(&size) as libc::socklen_t,
        )
    };
    if r != 0 {
        tracing::debug!(error = %std::io::Error::last_os_error(), "SO_SNDBUF not raised");
    }
}

/// Forwards every datagram to the hub. `socket` must be non-blocking.
pub(crate) fn udp_receiver(socket: UdpSocket, stop: Arc<StopSignal>, tx: Sender<Msg>) {
    let mut buf = [0u8; 2000];
    while stop.wait_readable(socket.as_raw_fd()) {
        // Take everything that has arrived before sleeping again.
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, source)) => {
                    if tx.send(Msg::Udp(buf[..n].to_vec(), source)).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    // E.g. an ICMP error for a viewer that is gone; nothing to act on.
                    tracing::debug!(error = %e, "UDP receive failed");
                    break;
                }
            }
        }
    }
}
