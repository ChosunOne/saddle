use std::fs;
use std::io;
use std::mem;
use std::net::IpAddr;
use std::net::Ipv4Addr;

pub fn enter(resolvers: &[IpAddr]) -> Result<(), String> {
    let ids = Ids {
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
    };
    user_namespace(ids)?;
    net_namespace()?;
    raise_loopback()?;
    for ip in resolvers {
        if is_loopback(*ip) {
            continue;
        }
        if let IpAddr::V4(v4) = ip {
            add_loopback_address(*v4)?;
        }
    }
    Ok(())
}

struct Ids {
    uid: libc::uid_t,
    gid: libc::gid_t,
}

fn user_namespace(ids: Ids) -> Result<(), String> {
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        return Err(format!("CLONE_NEWUSER: {}", io::Error::last_os_error()));
    }
    let _ = fs::write("/proc/self/setgroups", "deny");
    for (file, line) in [
        ("/proc/self/uid_map", format!("0 {} 1\n", ids.uid)),
        ("/proc/self/gid_map", format!("0 {} 1\n", ids.gid)),
    ] {
        fs::write(file, line).map_err(|e| format!("{file}: {e}"))?;
    }
    let mapped = unsafe { libc::getuid() } == 0;
    if !mapped {
        return Err(
            "the user namespace was entered but not mapped, so the command would run unmapped"
                .to_owned(),
        );
    }
    Ok(())
}

fn net_namespace() -> Result<(), String> {
    if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
        return Err(format!("CLONE_NEWNET: {}", io::Error::last_os_error()));
    }
    Ok(())
}

fn raise_loopback() -> Result<(), String> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(format!("socket: {}", io::Error::last_os_error()));
    }
    let result = loopback_ioctl(fd);
    unsafe { libc::close(fd) };
    result
}

fn loopback_ioctl(fd: libc::c_int) -> Result<(), String> {
    let mut req: libc::ifreq = unsafe { mem::zeroed() };
    for (i, byte) in b"lo".iter().enumerate() {
        req.ifr_name[i] = *byte as libc::c_char;
    }
    if unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut req) } < 0 {
        return Err(format!("SIOCGIFFLAGS: {}", io::Error::last_os_error()));
    }
    let mut flags = unsafe { req.ifr_ifru.ifru_flags };
    flags |= libc::IFF_UP as libc::c_short | libc::IFF_RUNNING as libc::c_short;
    req.ifr_ifru.ifru_flags = flags;
    if unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS, &mut req) } < 0 {
        return Err(format!("SIOCSIFFLAGS: {}", io::Error::last_os_error()));
    }
    Ok(())
}

fn add_loopback_address(ip: Ipv4Addr) -> Result<(), String> {
    let index = unsafe { libc::if_nametoindex(c"lo".as_ptr()) };
    if index == 0 {
        return Err(format!("if_nametoindex: {}", io::Error::last_os_error()));
    }
    let message = new_address_message(index, ip);
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    };
    if fd < 0 {
        return Err(format!("netlink socket: {}", io::Error::last_os_error()));
    }
    let result = send_new_address(fd, &message);
    unsafe { libc::close(fd) };
    result
}

fn new_address_message(index: u32, ip: Ipv4Addr) -> Vec<u8> {
    const RTM_NEWADDR: u16 = 20;
    const NLM_F_REQUEST: u16 = 1;
    const NLM_F_ACK: u16 = 4;
    const NLM_F_CREATE: u16 = 0x400;
    const NLM_F_REPLACE: u16 = 0x100;
    const IFA_ADDRESS: u16 = 1;
    const IFA_LOCAL: u16 = 2;
    const RT_SCOPE_HOST: u8 = 254;

    let mut out = Vec::with_capacity(48);
    out.extend_from_slice(&0u32.to_ne_bytes());
    out.extend_from_slice(&RTM_NEWADDR.to_ne_bytes());
    out.extend_from_slice(
        &(NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_REPLACE).to_ne_bytes(),
    );
    out.extend_from_slice(&1u32.to_ne_bytes());
    out.extend_from_slice(&0u32.to_ne_bytes());
    out.push(libc::AF_INET as u8);
    out.push(32);
    out.push(0);
    out.push(RT_SCOPE_HOST);
    out.extend_from_slice(&index.to_ne_bytes());
    for kind in [IFA_LOCAL, IFA_ADDRESS] {
        out.extend_from_slice(&(8u16).to_ne_bytes());
        out.extend_from_slice(&kind.to_ne_bytes());
        out.extend_from_slice(&ip.octets());
    }
    let len = out.len() as u32;
    out[0..4].copy_from_slice(&len.to_ne_bytes());
    out
}

fn send_new_address(fd: libc::c_int, message: &[u8]) -> Result<(), String> {
    let mut to: libc::sockaddr_nl = unsafe { mem::zeroed() };
    to.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    let sent = unsafe {
        libc::sendto(
            fd,
            message.as_ptr().cast(),
            message.len(),
            0,
            (&raw const to).cast(),
            mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if sent < 0 {
        return Err(format!("sendto: {}", io::Error::last_os_error()));
    }
    let mut reply = [0u8; 4096];
    let got = unsafe { libc::recv(fd, reply.as_mut_ptr().cast(), reply.len(), 0) };
    if got < 16 {
        return Err(format!("netlink reply: {}", io::Error::last_os_error()));
    }
    let error = i32::from_ne_bytes([reply[16], reply[17], reply[18], reply[19]]);
    match error {
        0 => Ok(()),
        e if e == -libc::EEXIST => Ok(()),
        e => Err(format!("RTM_NEWADDR: {}", io::Error::from_raw_os_error(-e))),
    }
}

pub fn resolvers(text: &str) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("nameserver") {
            continue;
        }
        let Some(address) = line.split_whitespace().nth(1) else {
            continue;
        };
        let address = address.split('%').next().unwrap_or(address);
        if let Ok(ip) = address.parse::<IpAddr>()
            && answerable(ip)
            && !out.contains(&ip)
        {
            out.push(ip);
        }
    }
    out
}

fn answerable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(_) => true,
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn a_new_address_message_is_one_the_kernel_can_read() {
        let message = new_address_message(1, Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(
            u32::from_ne_bytes(message[0..4].try_into().unwrap()) as usize,
            message.len()
        );
        assert_eq!(u16::from_ne_bytes(message[4..6].try_into().unwrap()), 20);
        assert_eq!(message[16], libc::AF_INET as u8);
        assert_eq!(message[17], 32);
        assert_eq!(message[19], 254);
        assert_eq!(u32::from_ne_bytes(message[20..24].try_into().unwrap()), 1);
        assert_eq!(&message[24..32], &[8, 0, 2, 0, 192, 168, 1, 1]);
        assert_eq!(&message[32..40], &[8, 0, 1, 0, 192, 168, 1, 1]);
        assert_eq!(message.len(), 40);
    }

    #[test]
    fn a_resolver_is_read_from_the_file_that_names_it() {
        let conf = "search example.com\nnameserver 127.0.0.53\nnameserver 8.8.8.8\n";
        assert_eq!(
            resolvers(conf),
            [
                "127.0.0.53".parse::<IpAddr>().unwrap(),
                "8.8.8.8".parse::<IpAddr>().unwrap()
            ]
        );
    }

    #[test]
    fn an_ipv4_resolver_is_answered_wherever_it_points() {
        assert_eq!(
            resolvers("nameserver 192.168.1.1\n"),
            ["192.168.1.1".parse::<IpAddr>().unwrap()]
        );
    }

    #[test]
    fn an_ipv6_resolver_is_only_answered_when_it_is_already_loopback() {
        assert!(resolvers("nameserver 2001:4860:4860::8888\n").is_empty());
        assert_eq!(
            resolvers("nameserver ::1\n"),
            ["::1".parse::<IpAddr>().unwrap()]
        );
    }

    #[test]
    fn a_scoped_or_duplicated_resolver_is_read_once() {
        let conf = "nameserver 127.0.0.53%lo\nnameserver 127.0.0.53\nnameserver ::1\n";
        assert_eq!(
            resolvers(conf),
            [
                "127.0.0.53".parse::<IpAddr>().unwrap(),
                "::1".parse::<IpAddr>().unwrap()
            ]
        );
    }

    #[test]
    fn a_line_that_is_not_a_nameserver_is_ignored() {
        let conf = "# nameserver 127.0.0.1\noptions timeout:1\nnameserver\nnameserver not-an-ip\n";
        assert!(resolvers(conf).is_empty());
    }
}
