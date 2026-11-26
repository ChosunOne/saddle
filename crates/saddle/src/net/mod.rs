pub mod bridge;
pub mod head;
pub mod hello;
pub mod proxy;

pub fn normalise_host(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

pub(crate) mod client;
