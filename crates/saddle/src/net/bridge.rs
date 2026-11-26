use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use anyhow::Result as HostResult;
use serde_json::json;

use saddle_cap::sandbox::{NetBridge, PROXY_PORT};
use saddle_cap::{Principal, Scope};
use saddle_policy::{Kind, Request};

use super::proxy::{self, Mode, Peer, Upstream};
use crate::net::client::{self as fetch, Target};
use crate::session::Session;

enum Ask {
    Decide {
        net: Net,
        reply: Sender<Result<Vec<SocketAddr>, String>>,
    },
}

struct Net {
    method: String,
    scheme: String,
    host: String,
    port: u16,
    path: String,
    headers: Vec<String>,
}

struct Remote {
    asks: Sender<Ask>,
    allowed: Option<(String, u16, Vec<SocketAddr>)>,
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
        let addresses = answer
            .recv()
            .map_err(|_| "the session ended".to_owned())??;
        self.allowed = Some(((*host).into(), *port, addresses));
        Ok(())
    }

    fn dial(&mut self, host: &str, port: u16) -> io::Result<Upstream> {
        let (_, _, addrs) = self
            .allowed
            .take()
            .filter(|(name, p, _)| name == host && *p == port)
            .ok_or_else(|| io::Error::other("no matching proxy decision"))?;
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

pub struct Bridge {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    asks: Receiver<Ask>,
    denials: Receiver<String>,
    listener: Option<thread::JoinHandle<()>>,
}

impl Bridge {
    pub fn start(dir: &Path) -> io::Result<Self> {
        Self::bind(dir, false)
    }

    pub fn start_named(dir: &Path) -> io::Result<Self> {
        Self::bind(dir, true)
    }

    fn bind(dir: &Path, named: bool) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("net-{}-{n}.sock", process::id()));
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
                            for why in answer(stream, asks, named) {
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

    pub fn spec(&self) -> NetBridge {
        NetBridge {
            socket: self.path.clone(),
            proxy_port: PROXY_PORT,
        }
    }

    pub fn service(&mut self, session: &mut Session) -> Vec<String> {
        while let Ok(Ask::Decide { net, reply }) = self.asks.try_recv() {
            let _ = reply.send(decide(session, &net));
        }
        let denials: Vec<String> = self.denials.try_iter().collect();
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

    pub fn service_as(
        &mut self,
        session: &mut Session,
        who: &Principal,
        call: Option<&str>,
    ) -> HostResult<Vec<String>> {
        self.service_bounded(session, who, call, None)
    }

    pub fn service_bounded(
        &mut self,
        session: &mut Session,
        who: &Principal,
        call: Option<&str>,
        ceiling: Option<(&[String], &[String])>,
    ) -> HostResult<Vec<String>> {
        while let Ok(Ask::Decide { net, reply }) = self.asks.try_recv() {
            let result = if ceiling
                .is_none_or(|(https, ssh)| within_launch_ceiling(&net, https, ssh))
            {
                decide_as(session, who, call, &net)
            } else {
                let why = "network target is outside the named launch host lists";
                session.audit.event(Kind::Decision, Some(who), Some(Scope::Net), Some(false), json!({"call_id":call, "via":"run-binary", "scheme":net.scheme,"host":net.host,"port":net.port}), why)?;
                Ok(Err(why.into()))
            };
            match result {
                Ok(result) => {
                    let _ = reply.send(result);
                }
                Err(error) => {
                    let _ = reply.send(Err("network host failure".into()));
                    return Err(error);
                }
            }
        }
        let denials: Vec<String> = self
            .denials
            .try_iter()
            .take(32)
            .map(|why| why.chars().take(1024).collect())
            .collect();
        for why in &denials {
            session.audit.event(
                Kind::Decision,
                Some(who),
                Some(Scope::Net),
                Some(false),
                json!({"via":"exec", "call_id":call}),
                why,
            )?;
        }
        Ok(denials)
    }

    pub fn finish(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.listener.take() {
            let _ = handle.join();
        }
    }
}

fn within_launch_ceiling(net: &Net, https: &[String], ssh: &[String]) -> bool {
    let hosts = match (net.scheme.as_str(), net.port, net.method.as_str()) {
        ("https", 443, _) => https,
        ("ssh", 22, "connect") => ssh,
        _ => return false,
    };
    hosts
        .iter()
        .any(|host| host.eq_ignore_ascii_case(&net.host))
}

fn answer(stream: UnixStream, asks: Sender<Ask>, named: bool) -> Vec<String> {
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
        "proxy" if named => Mode::NamedProxy,
        "proxy" => Mode::Proxy,
        _ => return Vec::new(),
    };
    let mut remote = Remote {
        asks,
        allowed: None,
    };
    let mut guest_write = stream;
    match proxy::serve(&mut reader, &mut guest_write, mode, &mut remote) {
        Ok(outcome) => outcome.denials,
        Err(_) => Vec::new(),
    }
}

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

fn decide_as(
    session: &mut Session,
    who: &Principal,
    call: Option<&str>,
    net: &Net,
) -> HostResult<Result<Vec<SocketAddr>, String>> {
    let target = Target {
        scheme: net.scheme.clone(),
        host: net.host.clone(),
        port: net.port,
        path: net.path.clone(),
    };
    let request = Request::Net {
        method: &net.method,
        scheme: &net.scheme,
        host: &net.host,
        port: net.port,
        path: &net.path,
        headers: &net.headers,
    };
    if !session.net_names_host(who, &net.host)
        && let Some(why) = fetch::local(&target)
    {
        session.audit.event(
            Kind::Decision,
            Some(who),
            Some(Scope::Net),
            Some(false),
            json!({"request":request.args(), "call_id":call}),
            &why,
        )?;
        return Ok(Err(why));
    }
    let decision = session.engine.decide(who, &request);
    session.audit.decision_for_call_details(
        who,
        &request,
        &decision,
        call.unwrap_or("native-exec"),
        json!({"via":"exec"}),
    )?;
    if !decision.allow {
        return Ok(Err(decision.explanation));
    }
    Ok(resolve_as(session, who, &net.host, net.port))
}

fn resolve(session: &mut Session, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    resolve_as(session, &Principal::Agent, host, port)
}

fn resolve_as(
    session: &Session,
    who: &Principal,
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, String> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("{host}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("{host} has no address"));
    }
    let named = session.net_names_host(who, host);
    for addr in &addrs {
        if fetch::private(addr.ip()).is_none() {
            continue;
        }
        if !named && !session.net_names_host(who, &addr.ip().to_string()) {
            return Err(format!(
                "{host} resolves to {}, and no grant names that address",
                addr.ip()
            ));
        }
    }
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::{Net, within_launch_ceiling};
    use saddle_cap::{Policy, Principal, Roots};

    #[test]
    fn ssh_tunnels_need_the_ssh_ceiling_and_a_separate_net_grant() {
        let mut request = Net {
            method: "connect".into(),
            scheme: "ssh".into(),
            host: "github.com".into(),
            port: 22,
            path: String::new(),
            headers: vec![],
        };
        let hosts = ["github.com".into()];
        assert!(!within_launch_ceiling(&request, &hosts, &[]));
        assert!(within_launch_ceiling(&request, &[], &hosts));
        let policy = Policy::parse(
            "version = 1\n[agent]\nnet = [{host='github.com', scheme='https', port=22}, {host='github.com', scheme='ssh'}]",
            &Roots::default(),
        ).unwrap();
        let grants = &policy.grants(&Principal::Agent).unwrap().net;
        assert!(!grants[0].allows(&request.scheme, &request.host, request.port));
        assert!(grants[1].allows(&request.scheme, &request.host, request.port));
        request.method = "get".into();
        assert!(!within_launch_ceiling(&request, &[], &hosts));
        request.method = "connect".into();
        request.host = "other.example".into();
        assert!(!within_launch_ceiling(&request, &[], &hosts));
    }
}
