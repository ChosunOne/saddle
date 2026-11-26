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

const READY_TIMEOUT_MS: i32 = 2_000;

pub fn spawn(net: &NetBridge, resolvers: &[IpAddr]) -> Result<(), String> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("pipe: {}", io::Error::last_os_error()));
    }
    let (read_end, write_end) = (fds[0], fds[1]);
    match unsafe { libc::fork() } {
        -1 => Err(format!("fork: {}", io::Error::last_os_error())),
        0 => {
            unsafe { libc::close(read_end) };
            match Relay::start(net, resolvers) {
                Ok(relay) => {
                    let _ = unsafe { libc::write(write_end, b"R".as_ptr().cast(), 1) };
                    unsafe { libc::close(write_end) };
                    relay.serve_forever()
                }
                Err(_) => unsafe {
                    let _ = libc::write(write_end, b"E".as_ptr().cast(), 1);
                    libc::close(write_end);
                    libc::_exit(0)
                },
            }
        }
        _ => {
            unsafe { libc::close(write_end) };
            let ready = wait_for_ready(read_end);
            unsafe { libc::close(read_end) };
            ready
        }
    }
}

fn wait_for_ready(fd: RawFd) -> Result<(), String> {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut poll, 1, READY_TIMEOUT_MS) };
    if n <= 0 {
        return Err("the relay did not start listening".to_owned());
    }
    let mut byte = 0u8;
    let got = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
    match (got, byte) {
        (1, b'R') => Ok(()),
        _ => Err("the relay could not bind the command's loopback".to_owned()),
    }
}

struct Relay {
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
        unsafe { libc::_exit(0) }
    }
}

struct Dialect {
    listener: TcpListener,
    dialect: &'static str,
}

fn listen(port: u16) -> io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
}

fn carry(guest: TcpStream, socket: &Path, dialect: &str) -> io::Result<()> {
    let mut host = UnixStream::connect(socket)?;
    host.write_all(dialect.as_bytes())?;
    host.write_all(b"\n")?;
    host.flush()?;
    let mut from_host = host.try_clone()?;
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
