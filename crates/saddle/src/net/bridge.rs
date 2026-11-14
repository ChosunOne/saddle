//! The bridge between a command's relay and this process: decisions in, connections out (D79).
//!
//! The proxy runs in its own threads, because carrying a connection means blocking on it. The
//! policy engine and the audit log do not travel with it: a thread asks, over a channel, and the
//! session thread answers while it is waiting for the command. That keeps one engine and one audit
//! log for the whole program, which is the property the third lowering exists to protect — a second
//! copy of the policy would be a second authority.
//!
//! Resolution happens on the session thread, with the decision, and the proxy connects to an
//! address that was checked rather than to the name again, so a name cannot be swapped underneath
//! the decision (D28).

use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use saddle_cap::sandbox::{NetBridge, PROXY_PORT};
use saddle_cap::{Principal, Scope};
use saddle_policy::{Kind, Request};

use super::proxy::{self, Mode, Peer, Upstream};
use crate::session::Session;
use crate::tools::fetch::{self, Target};

/// What the proxy thread wants from the session thread.
enum Ask {
    /// Decide this request, and say which addresses it may connect to.
    Decide {
        net: Net,
        reply: Sender<Result<Vec<SocketAddr>, String>>,
    },
    /// Resolve a host whose decision has already been made.
    Resolve {
        host: String,
        port: u16,
        reply: Sender<Result<Vec<SocketAddr>, String>>,
    },
}

/// A `net` request as an owned value, because it has to cross a channel to reach the engine.
struct Net {
    method: String,
    scheme: String,
    host: String,
    port: u16,
    path: String,
    headers: Vec<String>,
}

/// The proxy's side: it asks the session thread, and it makes the connections.
struct Remote {
    asks: Sender<Ask>,
}

impl Remote {
    fn ask(
        &self,
        make: impl FnOnce(Sender<Result<Vec<SocketAddr>, String>>) -> Ask,
    ) -> io::Result<Vec<SocketAddr>> {
        let (reply, answer) = mpsc::channel();
        self.asks
            .send(make(reply))
            .map_err(|_| io::Error::other("the session ended"))?;
        answer
            .recv()
            .map_err(|_| io::Error::other("the session ended"))?
            .map_err(io::Error::other)
    }
}

impl Peer for Remote {
    fn decide(&mut self, req: &Request<'_>) -> Result<(), String> {
        let Request::Net {
            method,
            scheme,
            host,
            port,
            path,
            headers,
        } = req
        else {
            return Err("the proxy asked about something that is not a request".to_owned());
        };
        let net = Net {
            method: (*method).to_owned(),
            scheme: (*scheme).to_owned(),
            host: (*host).to_owned(),
            port: *port,
            path: (*path).to_owned(),
            headers: headers.to_vec(),
        };
        let (reply, answer) = mpsc::channel();
        self.asks
            .send(Ask::Decide { net, reply })
            .map_err(|_| "the session ended".to_owned())?;
        answer
            .recv()
            .map_err(|_| "the session ended".to_owned())?
            .map(|_| ())
    }

    fn dial(&mut self, host: &str, port: u16) -> io::Result<Upstream> {
        // The addresses were chosen when the decision was made on the session thread, so dialling
        // the name again here would let DNS answer differently the second time.
        let addrs = self.ask(|reply| Ask::Resolve {
            host: host.to_owned(),
            port,
            reply,
        })?;
        let mut last = io::Error::other("no address to connect to");
        for addr in addrs {
            match TcpStream::connect(addr) {
                Ok(stream) => {
                    let reader = Box::new(stream.try_clone()?) as Box<dyn Read + Send>;
                    return Ok(Upstream {
                        reader,
                        writer: Box::new(stream),
                    });
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

/// One bridge per `exec`, holding the socket the relay connects to in the command's namespace.
pub struct Bridge {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    asks: Receiver<Ask>,
    denials: Receiver<String>,
    listener: Option<thread::JoinHandle<()>>,
}

impl Bridge {
    /// Binds the socket the relay will connect to. `dir` is the session's temporary root, which is
    /// writable by this process and reachable by the relay, which is not confined.
    pub fn start(dir: &Path) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("net-{}-{n}.sock", process::id()));
        // A socket left behind by a process that died is not a listener.
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;

        let stop = Arc::new(AtomicBool::new(false));
        let (ask_tx, asks) = mpsc::channel();
        let (denial_tx, denials) = mpsc::channel();

        let thread_stop = stop.clone();
        let thread_path = path.clone();
        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let asks = ask_tx.clone();
                        let denials = denial_tx.clone();
                        thread::spawn(move || {
                            for why in answer(stream, asks) {
                                let _ = denials.send(why);
                            }
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            let _ = fs::remove_file(&thread_path);
        });

        Ok(Self {
            path,
            stop,
            asks,
            denials,
            listener: Some(handle),
        })
    }

    /// What the sandbox spec carries: where the relay connects, and where the guest's proxy is.
    pub fn spec(&self) -> NetBridge {
        NetBridge {
            socket: self.path.clone(),
            proxy_port: PROXY_PORT,
        }
    }

    /// Answers every question the proxy has asked, and returns what was refused since last time.
    /// Called from the loop that waits for the command, so it never blocks a decision for longer
    /// than one request.
    pub fn service(&mut self, session: &mut Session) -> Vec<String> {
        loop {
            match self.asks.try_recv() {
                Ok(Ask::Decide { net, reply }) => {
                    let _ = reply.send(decide(session, &net));
                }
                Ok(Ask::Resolve { host, port, reply }) => {
                    let _ = reply.send(resolve(session, &host, port));
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        let denials: Vec<String> = self.denials.try_iter().collect();
        // A refusal that never reached the engine is still a refusal, and every one is recorded
        // (D10). The engine's own refusals were recorded by `decide`.
        for why in &denials {
            let _ = session.audit.event(
                Kind::Decision,
                Some(&Principal::Agent),
                Some(Scope::Net),
                Some(false),
                serde_json::json!({ "via": "exec" }),
                why,
            );
        }
        denials
    }

    /// Stops the listener and removes the socket.
    pub fn finish(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.listener.take() {
            let _ = handle.join();
        }
    }
}

/// Reads the relay's dialect line, carries the connection, and reports what was refused.
fn answer(stream: UnixStream, asks: Sender<Ask>) -> Vec<String> {
    let Ok(mut reader) = stream.try_clone().map(BufReader::new) else {
        return Vec::new();
    };
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return Vec::new();
    }
    let mode = match line.trim() {
        "tls" => Mode::Tls,
        "http" => Mode::Http,
        "proxy" => Mode::Proxy,
        _ => return Vec::new(),
    };
    let mut remote = Remote { asks };
    // The buffer is kept rather than dropped: the guest's first bytes often arrive with the
    // dialect line, and a client hello read into this buffer must not be lost.
    let mut guest_write = stream;
    match proxy::serve(&mut reader, &mut guest_write, mode, &mut remote) {
        Ok(outcome) => outcome.denials,
        Err(_) => Vec::new(),
    }
}

/// The whole decision for one request: the engine, the rule that a wildcard does not open
/// loopback, and the addresses the connection may use.
fn decide(session: &mut Session, net: &Net) -> Result<Vec<SocketAddr>, String> {
    let who = Principal::Agent;
    let target = Target {
        scheme: net.scheme.clone(),
        host: net.host.clone(),
        port: net.port,
        path: net.path.clone(),
    };
    if !session.net_names_host(&who, &target.host)
        && let Some(why) = fetch::local(&target)
    {
        return Err(why);
    }
    let d = session
        .decide(
            &who,
            &Request::Net {
                method: &net.method,
                scheme: &net.scheme,
                host: &net.host,
                port: net.port,
                path: &net.path,
                headers: &net.headers,
            },
        )
        .map_err(|e| format!("{e:#}"))?;
    if !d.allow {
        return Err(d.explanation);
    }
    resolve(session, &net.host, net.port)
}

/// Resolves and applies D28's address rule to what came back: an address that is private or
/// loopback is reachable only when a grant names it, whether it was written as a name or as an
/// address.
fn resolve(session: &mut Session, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("{host}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("{host} has no address"));
    }
    let who = Principal::Agent;
    let named = session.net_names_host(&who, host);
    for addr in &addrs {
        if fetch::private(addr.ip()).is_none() {
            continue;
        }
        if !named && !session.net_names_host(&who, &addr.ip().to_string()) {
            return Err(format!(
                "{host} resolves to {}, and no grant names that address",
                addr.ip()
            ));
        }
    }
    Ok(addrs)
}
