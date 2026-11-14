//! The relay: the only thing a command's network can reach (D79).
//!
//! It listens on the namespace's own loopback, says one line to the proxy in the host process
//! naming the dialect it accepted, and then carries bytes. It holds no authority and makes no
//! decision — it understands nothing about the greens, which is why a new protocol needs no new
//! code here.
//!
//! It is a forked child of the sandbox helper, because a command's own `socket(2)` is still
//! filtered for `AF_UNIX` (that is how a command is kept away from `docker.sock`, `gpg-agent` and
//! the rest); the relay needs `AF_UNIX` for the one socket that leaves the namespace, so it is
//! started before the filter applies and the command never sees it.

use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::net::TcpStream;
use std::net::{IpAddr, SocketAddr, TcpListener, UdpSocket};
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::thread;

use saddle_cap::sandbox::NetBridge;

use crate::dns;

/// How long the helper waits for the relay to have bound everything, in milliseconds. A relay that
/// cannot listen is not fatal: the command still runs, with a network that reaches nothing.
const READY_TIMEOUT_MS: i32 = 2_000;

/// Starts the relay in a forked child and returns once it is listening (or has failed).
pub fn spawn(net: &NetBridge, resolvers: &[IpAddr]) -> Result<(), String> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe(2) fills two descriptors it is given.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("pipe: {}", io::Error::last_os_error()));
    }
    let (read_end, write_end) = (fds[0], fds[1]);
    // SAFETY: fork(2) has no arguments. The child takes the read end away; the parent the write
    // end. No thread has been started in this process, so the child sees one thread.
    match unsafe { libc::fork() } {
        -1 => Err(format!("fork: {}", io::Error::last_os_error())),
        0 => {
            // SAFETY: both descriptors are owned by this process.
            unsafe { libc::close(read_end) };
            match Relay::start(net, resolvers) {
                Ok(relay) => {
                    // One byte means "listening", so the command cannot start before it does.
                    // SAFETY: writing one byte to our own pipe.
                    let _ = unsafe { libc::write(write_end, b"R".as_ptr().cast(), 1) };
                    // SAFETY: the parent's end is not needed here.
                    unsafe { libc::close(write_end) };
                    relay.serve_forever()
                }
                Err(_) => {
                    // SAFETY: as above.
                    unsafe {
                        let _ = libc::write(write_end, b"E".as_ptr().cast(), 1);
                        libc::close(write_end);
                        libc::_exit(0)
                    }
                }
            }
        }
        _ => {
            // SAFETY: the child owns the write end now.
            unsafe { libc::close(write_end) };
            let ready = wait_for_ready(read_end);
            // SAFETY: read end is ours and is closed once.
            unsafe { libc::close(read_end) };
            ready
        }
    }
}

/// Waits for the relay's one byte. A timeout is a failure to listen, not a reason to refuse.
fn wait_for_ready(fd: RawFd) -> Result<(), String> {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd, a millisecond timeout, no signals handled here.
    let n = unsafe { libc::poll(&mut poll, 1, READY_TIMEOUT_MS) };
    if n <= 0 {
        return Err("the relay did not start listening".to_owned());
    }
    let mut byte = 0u8;
    // SAFETY: reading one byte into a local.
    let got = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
    match (got, byte) {
        (1, b'R') => Ok(()),
        _ => Err("the relay could not bind the command's loopback".to_owned()),
    }
}

struct Relay {
    /// The socket in the host process every accepted connection is carried to.
    socket: PathBuf,
    tunnels: Vec<Dialect>,
    resolver: Vec<UdpSocket>,
}

impl Relay {
    fn start(net: &NetBridge, resolvers: &[IpAddr]) -> Result<Self, String> {
        let socket = net.socket.clone();
        if !socket.exists() {
            return Err(format!("{}: no proxy socket", socket.display()));
        }
        // The three dialects, each on the port a client reaches it by. These are the ports a name
        // resolves onto, so they have to be the defaults: anything else is reached through the
        // proxy and named in a request line.
        let tunnels = [(443u16, "tls"), (80, "http"), (net.proxy_port, "proxy")]
            .into_iter()
            .filter_map(|(port, dialect)| {
                listen(port)
                    .ok()
                    .map(|listener| Dialect { listener, dialect })
            })
            .collect::<Vec<_>>();
        if tunnels.is_empty() {
            return Err("nothing could be bound on the command's loopback".to_owned());
        }
        // A resolver on loopback is where `resolv.conf` already points, so answering there needs
        // no change to the guest's files. One that cannot be bound is simply not answered.
        let mut resolver = Vec::new();
        for ip in resolvers {
            if let Ok(sock) = UdpSocket::bind(SocketAddr::new(*ip, 53)) {
                resolver.push(sock);
            }
        }
        Ok(Self {
            socket,
            tunnels,
            resolver,
        })
    }

    fn serve_forever(self) -> ! {
        let mut handles = Vec::new();
        for socket in self.resolver {
            handles.push(thread::spawn(move || serve_dns(&socket)));
        }
        for Dialect { listener, dialect } in self.tunnels {
            let socket = self.socket.clone();
            handles.push(thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let socket = socket.clone();
                    thread::spawn(move || {
                        let _ = carry(stream, &socket, dialect);
                    });
                }
            }));
        }
        for handle in handles {
            let _ = handle.join();
        }
        // The proxy is gone, so there is nothing left to serve.
        // SAFETY: _exit does not run destructors, which is what a forked child wants.
        unsafe { libc::_exit(0) }
    }
}

struct Dialect {
    listener: TcpListener,
    /// The line the proxy is told before the bytes, so it knows how to read them.
    dialect: &'static str,
}

fn listen(port: u16) -> io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
}

/// Carries one guest connection to the proxy, saying which dialect it accepted first.
fn carry(guest: TcpStream, socket: &Path, dialect: &str) -> io::Result<()> {
    let mut host = UnixStream::connect(socket)?;
    host.write_all(dialect.as_bytes())?;
    host.write_all(b"\n")?;
    host.flush()?;
    let mut from_host = host.try_clone()?;
    // The guest is one stream read and written in two directions, so it is split by handle: the
    // copy to the proxy keeps the original and the copy back gets a clone.
    let back = guest.try_clone()?;
    thread::scope(|s| {
        s.spawn(move || {
            let mut back = back;
            let _ = io::copy(&mut from_host, &mut back);
        });
        let mut guest = guest;
        let _ = io::copy(&mut guest, &mut host);
    });
    Ok(())
}

fn serve_dns(socket: &UdpSocket) {
    let mut buf = [0u8; 1500];
    loop {
        let Ok((n, from)) = socket.recv_from(&mut buf) else {
            return;
        };
        if let Some(answer) = dns::answer(&buf[..n]) {
            let _ = socket.send_to(&answer, from);
        }
    }
}
