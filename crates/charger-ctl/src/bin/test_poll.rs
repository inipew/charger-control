use std::mem;

fn main() {
    println!("[*] Menguji Kernel Netlink UEVENT (NETLINK_KOBJECT_UEVENT)...");
    println!("[*] Tekan Ctrl+C untuk berhenti.");
    println!("[*] Silakan cabut/colok charger Anda sekarang...\n");

    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        eprintln!(
            "[-] Gagal membuat socket netlink: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(1);
    }

    let mut addr: libc::sockaddr_nl = unsafe { mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_pid = std::process::id() as u32;
    addr.nl_groups = 1; // Mendengarkan broadcast grup uevent (1)

    let ret = unsafe {
        libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    };

    if ret < 0 {
        eprintln!(
            "[-] Gagal bind socket netlink: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(1);
    }

    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };

    let mut buf = [0u8; 4096];

    loop {
        let ret = unsafe { libc::poll(&mut pfd, 1, 10000) };
        if ret > 0 {
            let len =
                unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
            if len > 0 {
                let s = String::from_utf8_lossy(&buf[..len as usize]);
                // Filter hanya event dari subsystem power_supply
                if s.contains("SUBSYSTEM=power_supply") {
                    println!("\n[+] EVENT POWER_SUPPLY TERDETEKSI!");
                    // Parse string yang terpisah oleh null byte (\0)
                    for part in s.split('\0').filter(|&x| !x.is_empty()) {
                        if part.starts_with("ACTION=")
                            || part.starts_with("SUBSYSTEM=")
                            || part.starts_with("POWER_SUPPLY_NAME=")
                            || part.starts_with("POWER_SUPPLY_STATUS=")
                        {
                            println!("    -> {}", part);
                        }
                    }
                    println!("[*] Menunggu event berikutnya...\n");
                }
            }
        } else if ret == 0 {
            println!("[-] 10 detik berlalu, tidak ada event uevent...");
        } else {
            eprintln!("[-] Error poll(): {}", std::io::Error::last_os_error());
            break;
        }
    }
}
