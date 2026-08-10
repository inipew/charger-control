use charger_core::error::ChargerError;

struct NetlinkFd(libc::c_int);

impl Drop for NetlinkFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe {
                libc::close(self.0);
            }
        }
    }
}

pub fn run_uevent_dumper() -> Result<(), ChargerError> {
    println!("=== UEVENT DUMPER ===");
    println!("Listening for netlink broadcast (uevent) messages...");
    println!("Please plug or unplug your charger to see hardware events.");
    println!("Press Ctrl+C to stop.\n");

    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(ChargerError::ParseError("Failed to create netlink socket"));
    }

    let _guard = NetlinkFd(fd);

    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_pid = std::process::id() as u32;
    addr.nl_groups = 1;

    let ret = unsafe {
        libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    };

    if ret < 0 {
        return Err(ChargerError::ParseError(
            "Failed to bind netlink socket (run as root?)",
        ));
    }

    let mut buf = [0u8; 8192];
    loop {
        let res = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if res > 0 {
            let data = &buf[..res as usize];
            let s = String::from_utf8_lossy(data);

            // Only print if it looks like a power supply or battery event to avoid huge noise
            if s.contains("power_supply") || s.contains("battery") {
                let parts: Vec<&str> = s.split('\0').collect();
                println!("--- UEVENT KERNEL BROADCAST ---");
                for part in parts {
                    if !part.is_empty() {
                        println!("  {}", part);
                    }
                }
                println!("-------------------------------\n");
            }
        }
    }
}
