//! The child-network bridge: a command gets a network that reaches one door (D79).
//!
//! `exec` used to have no network at all (D19), which is why a build could not fetch a dependency.
//! Now every command runs in a network namespace with no route out, and the only thing it can
//! reach is a relay on its own loopback. The relay carries bytes to this process over a unix
//! socket, and this process decides — with the same engine and the same audit log as `fetch`
//! (D39), from the same `net` grants, with no second vocabulary.
//!
//! The rule that makes it one authority: the guest never learns an address that is not the relay.
//! The name a connection is for therefore has to arrive in the bytes, and [`hello`] reads it from
//! the only places it is not encrypted — a `CONNECT` line, a request line, a `Host` header, or the
//! server name extension of a TLS client hello. A connection that names no host is not decided by
//! an address allowlist; it is refused, because an address is not a name and the grant names a
//! name (D28, D40).

pub mod bridge;
pub mod head;
pub mod hello;
pub mod proxy;

/// The form a host is compared in: lowercase, without a trailing root dot.
///
/// A grant is written the way a person writes a host, so the comparison has to be made in one
/// normal form at every point that has a name — the `CONNECT` line, the `Host` header, the
/// server name extension, and the URL `fetch` was given.
pub fn normalise_host(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

pub(crate) mod client;
