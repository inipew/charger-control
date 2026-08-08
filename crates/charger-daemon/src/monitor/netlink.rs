use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

const NETLINK_RECONNECT_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const NETLINK_RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(60);
const NETLINK_DEBOUNCE: Duration = Duration::from_millis(250);

pub struct NetlinkMonitor {
    socket: Option<OwnedFd>,
    reconnect_at: Option<Instant>,
    backoff: Duration,
    debounce_target: Option<Instant>,
}

impl NetlinkMonitor {
    pub fn new() -> Self {
        let mut monitor = Self {
            socket: None,
            reconnect_at: None,
            backoff: NETLINK_RECONNECT_INITIAL_BACKOFF,
            debounce_target: None,
        };
        monitor.try_reconnect(Instant::now());
        monitor
    }

    pub fn is_connected(&self) -> bool {
        self.socket.is_some()
    }

    pub fn as_raw_fd(&self) -> Option<i32> {
        self.socket.as_ref().map(|s| s.as_raw_fd())
    }

    pub fn disconnect(&mut self) {
        self.socket = None;
    }

    pub fn schedule_reconnect(&mut self, now: Instant) {
        self.reconnect_at = Some(now + self.backoff);
        self.backoff = (self.backoff * 2).min(NETLINK_RECONNECT_MAX_BACKOFF);
    }

    pub fn should_reconnect(&self, now: Instant) -> bool {
        if self.socket.is_some() {
            return false;
        }
        if let Some(target) = self.reconnect_at {
            now >= target
        } else {
            true // If no socket and no reconnect target, it should reconnect immediately
        }
    }

    pub fn try_reconnect(&mut self, now: Instant) -> bool {
        match Self::create_netlink_socket() {
            Ok(sock) => {
                tracing::info!("Netlink socket connected successfully");
                self.socket = Some(sock);
                self.reconnect_at = None;
                self.backoff = NETLINK_RECONNECT_INITIAL_BACKOFF;
                true
            }
            Err(e) => {
                tracing::warn!("Netlink reconnect failed ({}).", e);
                self.schedule_reconnect(now);
                false
            }
        }
    }

    fn create_netlink_socket() -> std::io::Result<OwnedFd> {
        let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_KOBJECT_UEVENT) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_pid = 0; // Let kernel assign PID
        addr.nl_groups = 1;
        let ret = unsafe {
            libc::bind(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as u32,
            )
        };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(err);
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn handle_events(&mut self, now: Instant) -> bool {
        let Some(raw_fd) = self.as_raw_fd() else {
            return false;
        };

        let mut buf = [0u8; 4096];
        let mut found = false;
        loop {
            let n = unsafe {
                libc::recv(
                    raw_fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n <= 0 {
                break;
            }
            let buf_slice = &buf[..n as usize];

            if Self::contains_subslice(buf_slice, b"SUBSYSTEM=power_supply")
                && Self::contains_subslice(buf_slice, b"ACTION=change")
            {
                found = true;
            }
        }

        if found && self.debounce_target.is_none() {
            self.debounce_target = Some(now + NETLINK_DEBOUNCE);
        }

        false
    }

    pub fn debounce_due(&mut self, now: Instant) -> bool {
        if let Some(target) = self.debounce_target {
            if now >= target {
                self.debounce_target = None;
                return true;
            }
        }
        false
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        if let Some(dt) = self.debounce_target {
            if let Some(rt) = self.reconnect_at {
                Some(dt.min(rt))
            } else {
                Some(dt)
            }
        } else {
            self.reconnect_at
        }
    }

    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
