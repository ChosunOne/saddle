//! A command's own network namespace: no route out, one door in (D79).
//!
//! A network namespace starts with nothing — not even a working loopback — and no route to
//! anywhere. That is the whole control: `socket(2)` is allowed in a child again because a socket
//! in this namespace can only reach `127.0.0.1`, and `127.0.0.1` here is the child's own.
//!
//! An unprivileged process cannot create one on its own, so this enters a user namespace first,
//! which is where the capability to do the rest comes from. Both steps happen before landlock,
//! because they need to write `/proc/self/uid_map`, which landlock will not allow afterwards.
//!
//! Every step can fail, and failure is not fatal: the caller falls back to the sandbox as it was
//! before D79, with no network at all. The fallback is strictly narrower, so an environment where
//! this does not work is less capable, never less confined.

use std::io;
use std::net::IpAddr;

/// Enters a private network namespace, raises its loopback, and makes the resolver addresses the
/// guest will ask into addresses of that loopback.
///
/// The second part is what makes a name resolve at all. A namespace starts with no route, so a
/// query to the resolver `resolv.conf` names would go nowhere; putting that address on `lo` means
/// the query arrives here instead, where [`crate::dns`] answers it. It gives nothing away: the
/// address becomes local, so there is no route to the real host behind it, which is the point.
pub fn enter(resolvers: &[IpAddr]) -> Result<(), String> {
    user_namespace()?;
    net_namespace()?;
    raise_loopback()?;
    for ip in resolvers {
        // An address already inside 127.0.0.0/8 is on the loopback already.
        if is_loopback(*ip) {
            continue;
        }
        if let IpAddr::V4(v4) = ip {
            // A failure here costs DNS, not confinement: the proxy is still reached by an address
            // the guest was given, and this is reported rather than swallowed.
            add_loopback_address(*v4)?;
        }
    }
    Ok(())
}

fn user_namespace() -> Result<(), String> {
    // SAFETY: unshare(2) takes flags and returns an integer.
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        return Err(format!("CLONE_NEWUSER: {}", io::Error::last_os_error()));
    }
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    // An unprivileged user may not map a group before giving up setgroups(2).
    let _ = std::fs::write("/proc/self/setgroups", "deny");
    // One id, mapped to the same user: files stay owned by them, so nothing on disk changes owner
    // because of the sandbox.
    for (file, line) in [
        ("/proc/self/uid_map", format!("0 {uid} 1\n")),
        ("/proc/self/gid_map", format!("0 {gid} 1\n")),
    ] {
        std::fs::write(file, line).map_err(|e| format!("{file}: {e}"))?;
    }
    Ok(())
}

fn net_namespace() -> Result<(), String> {
    // SAFETY: as above; a single-threaded process may unshare its network namespace.
    if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
        return Err(format!("CLONE_NEWNET: {}", io::Error::last_os_error()));
    }
    Ok(())
}

fn raise_loopback() -> Result<(), String> {
    // SAFETY: a socket used only for interface ioctls, closed before returning.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(format!("socket: {}", io::Error::last_os_error()));
    }
    let result = loopback_ioctl(fd);
    // SAFETY: fd is ours and is not used again.
    unsafe { libc::close(fd) };
    result
}

fn loopback_ioctl(fd: libc::c_int) -> Result<(), String> {
    // SAFETY: the request fills an ifreq, which is all zeroes first so no field is uninitialised.
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (i, byte) in b"lo".iter().enumerate() {
        req.ifr_name[i] = *byte as libc::c_char;
    }
    // SAFETY: SIOCGIFFLAGS reads the name and writes the flags field of the same ifreq.
    if unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut req) } < 0 {
        return Err(format!("SIOCGIFFLAGS: {}", io::Error::last_os_error()));
    }
    // SAFETY: the union's flags field is the one SIOCGIFFLAGS filled.
    let mut flags = unsafe { req.ifr_ifru.ifru_flags };
    flags |= libc::IFF_UP as libc::c_short | libc::IFF_RUNNING as libc::c_short;
    req.ifr_ifru.ifru_flags = flags;
    // SAFETY: SIOCSIFFLAGS consumes the ifreq the previous call filled.
    if unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS, &mut req) } < 0 {
        return Err(format!("SIOCSIFFLAGS: {}", io::Error::last_os_error()));
    }
    Ok(())
}

/// Adds `ip` to the loopback interface, so a query sent to it arrives in this namespace.
fn add_loopback_address(ip: std::net::Ipv4Addr) -> Result<(), String> {
    // SAFETY: the name is a NUL-terminated literal.
    let index = unsafe { libc::if_nametoindex(c"lo".as_ptr()) };
    if index == 0 {
        return Err(format!("if_nametoindex: {}", io::Error::last_os_error()));
    }
    let message = new_address_message(index, ip);
    // SAFETY: a netlink socket used only to add one address, closed before returning.
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
    // SAFETY: fd is ours and is not used again.
    unsafe { libc::close(fd) };
    result
}

/// An `RTM_NEWADDR` for `ip` as a host address of interface `index`, built as bytes because the
/// netlink header and its attributes have no alignment guarantees worth relying on.
fn new_address_message(index: u32, ip: std::net::Ipv4Addr) -> Vec<u8> {
    const RTM_NEWADDR: u16 = 20;
    const NLM_F_REQUEST: u16 = 1;
    const NLM_F_ACK: u16 = 4;
    const NLM_F_CREATE: u16 = 0x400;
    const NLM_F_REPLACE: u16 = 0x100;
    const IFA_ADDRESS: u16 = 1;
    const IFA_LOCAL: u16 = 2;
    // A host address on loopback: reachable from this machine and nothing else.
    const RT_SCOPE_HOST: u8 = 254;

    let mut out = Vec::with_capacity(48);
    // nlmsghdr: len, type, flags, seq, pid. The length is patched at the end.
    out.extend_from_slice(&0u32.to_ne_bytes());
    out.extend_from_slice(&RTM_NEWADDR.to_ne_bytes());
    out.extend_from_slice(
        &(NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_REPLACE).to_ne_bytes(),
    );
    out.extend_from_slice(&1u32.to_ne_bytes());
    out.extend_from_slice(&0u32.to_ne_bytes());
    // ifaddrmsg: family, prefixlen, flags, scope, index.
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
    // SAFETY: a sockaddr_nl of the kernel's address, zeroed first so no field is uninitialised.
    let mut to: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    to.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    // SAFETY: sendto with a valid buffer and length, and a sockaddr of the stated size.
    let sent = unsafe {
        libc::sendto(
            fd,
            message.as_ptr().cast(),
            message.len(),
            0,
            (&raw const to).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if sent < 0 {
        return Err(format!("sendto: {}", io::Error::last_os_error()));
    }
    let mut reply = [0u8; 4096];
    // SAFETY: receiving into a buffer this process owns.
    let got = unsafe { libc::recv(fd, reply.as_mut_ptr().cast(), reply.len(), 0) };
    if got < 16 {
        return Err(format!("netlink reply: {}", io::Error::last_os_error()));
    }
    // A successful add is acknowledged by an `NLMSG_ERROR` with a zero code.
    let error = i32::from_ne_bytes([reply[16], reply[17], reply[18], reply[19]]);
    match error {
        0 => Ok(()),
        // The address being there already is what this wanted.
        e if e == -libc::EEXIST => Ok(()),
        e => Err(format!("RTM_NEWADDR: {}", io::Error::from_raw_os_error(-e))),
    }
}

/// The resolver addresses a client in this namespace will ask.
///
/// A name has to resolve to the relay for the transparent path to work at all, and the only way to
/// answer without rewriting the guest's `/etc/resolv.conf` — which needs a mount namespace, and a
/// mount over a file the user does not own — is to answer on the address that file already names.
/// An IPv4 resolver is made local by [`enter`], whether it was written as loopback or not; an IPv6
/// one is only answered when it is already loopback, because assigning an address to an interface
/// is done for one family here.
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
        // `nameserver 127.0.0.53%lo` is a scoped form, and the scope is not part of the address.
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
        // Any `::1`-only form is loopback; an IPv4-mapped address is decided by its own form.
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_address_message_is_one_the_kernel_can_read() {
        let message = new_address_message(1, std::net::Ipv4Addr::new(192, 168, 1, 1));
        // The header says how long the whole message is, and the type says what it is.
        assert_eq!(
            u32::from_ne_bytes(message[0..4].try_into().unwrap()) as usize,
            message.len()
        );
        assert_eq!(u16::from_ne_bytes(message[4..6].try_into().unwrap()), 20);
        // One address, on loopback, with no prefix to speak of and a host scope.
        assert_eq!(message[16], libc::AF_INET as u8);
        assert_eq!(message[17], 32);
        assert_eq!(message[19], 254);
        assert_eq!(u32::from_ne_bytes(message[20..24].try_into().unwrap()), 1);
        // Two attributes of eight bytes each: IFA_LOCAL then IFA_ADDRESS, then the address in
        // each. Nothing follows them, so the message is exactly forty bytes.
        assert_eq!(&message[24..32], &[8, 0, 2, 0, 192, 168, 1, 1]);
        assert_eq!(&message[32..40], &[8, 0, 1, 0, 192, 168, 1, 1]);
        assert_eq!(message.len(), 40);
    }

    #[test]
    fn a_resolver_is_read_from_the_file_that_names_it() {
        let conf = "search example.com\nnameserver 127.0.0.53\nnameserver 8.8.8.8\n";
        // Both are answered: the first is already loopback, the second is made loopback.
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
        // A namespace has no route, so the address is put on its own loopback first; that is what
        // makes the query arrive here rather than nowhere.
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
