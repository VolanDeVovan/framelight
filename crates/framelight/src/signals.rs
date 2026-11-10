//! SIGINT, SIGTERM and SIGUSR1 as a descriptor to sleep on.
//!
//! The signals are blocked and read from a signalfd, so a thread can wait for them
//! together with a socket in one `poll`, without a timer and without the window between
//! "checked the flag" and "went to sleep" that a flag set by a handler leaves open.

use anyhow::{Result, bail};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// SIGINT or SIGTERM: leave.
    Stop,
    /// SIGUSR1: press the app's button.
    Toggle,
}

pub struct Signals(OwnedFd);

impl Signals {
    /// Call before any thread is started: the block mask is inherited by the threads
    /// started afterwards, which is what keeps the signals away from all of them.
    pub fn install() -> Result<Self> {
        // SAFETY: plain libc calls on a locally owned signal set; the descriptor is
        // checked and owned from here on.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for s in [libc::SIGINT, libc::SIGTERM, libc::SIGUSR1] {
                libc::sigaddset(&mut set, s);
            }
            let e = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            if e != 0 {
                bail!("pthread_sigmask: {}", std::io::Error::from_raw_os_error(e));
            }
            let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
            if fd < 0 {
                bail!("signalfd: {}", std::io::Error::last_os_error());
            }
            Ok(Self(OwnedFd::from_raw_fd(fd)))
        }
    }

    /// For `poll`: readable when a signal is pending.
    pub fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    /// A pending signal, without waiting.
    pub fn try_next(&self) -> Option<Signal> {
        // SAFETY: reads one signalfd_siginfo into a zeroed, correctly sized struct.
        let signo = unsafe {
            let mut info: libc::signalfd_siginfo = std::mem::zeroed();
            let n = libc::read(
                self.fd(),
                (&raw mut info).cast(),
                size_of::<libc::signalfd_siginfo>(),
            );
            if n != size_of::<libc::signalfd_siginfo>() as isize {
                return None;
            }
            info.ssi_signo as i32
        };
        Some(if signo == libc::SIGUSR1 {
            Signal::Toggle
        } else {
            Signal::Stop
        })
    }

    /// Sleeps until SIGINT or SIGTERM arrives.
    pub fn wait_for_stop(&self) {
        loop {
            while let Some(signal) = self.try_next() {
                if signal == Signal::Stop {
                    return;
                }
            }
            let mut pfd = libc::pollfd {
                fd: self.fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd; no timeout.
            unsafe { libc::poll(&mut pfd, 1, -1) };
        }
    }
}
