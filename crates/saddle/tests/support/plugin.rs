//! tiny compiled components shared by plugin CLI/model tests; no guest target is needed.

use std::fs;
use std::path::Path;

use saddle_policy::audit::sha256_hex;

const READER: &str = include_str!("../../../saddle-loader/src/fixtures/read-tool.wat");

pub fn reader(path: &str) -> Vec<u8> {
    reader_on_root("workspace", path)
}

pub fn reader_on_root(root: &str, path: &str) -> Vec<u8> {
    wat::parse_str(reader_source(root, path)).unwrap()
}

fn reader_source(root: &str, path: &str) -> String {
    assert!(root.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    assert!(path.is_ascii() && !path.contains(['"', '\\']));
    READER
        .replace(
            "(data (i32.const 256) \"workspace\")",
            &format!("(data (i32.const 256) {root:?})"),
        )
        .replace(
            "i32.const 256 i32.const 9",
            &format!("i32.const 256 i32.const {}", root.len()),
        )
        .replace(
            "(data (i32.const 272) \"denied\")",
            &format!("(data (i32.const 272) \"denied\")\n(data (i32.const 300) {path:?})"),
        )
        .replace(
            "local.get $path local.get $len i32.const 32 call $read",
            &format!(
                "i32.const 300 i32.const {} i32.const 32 call $read",
                path.len()
            ),
        )
}

#[allow(dead_code)] // shared with the model suite, which does not need this fixture.
pub fn range_reader(path: &str, offset: u64, limit: u32) -> Vec<u8> {
    let source = reader_source("workspace", path)
        .replace("fs-read@0.1.0", "fs-read-range@0.1.0")
        .replace("\"read-file\"", "\"read-range\"")
        .replace("(type $result (result (list u8) (error $failure)))", "(type $chunk-record (record (field \"bytes\" (list u8)) (field \"eof\" bool))) (export \"chunk\" (type $chunk (eq $chunk-record))) (type $result (result $chunk (error $failure)))")
        .replace("(func (param \"path\" $path) (result $result))", "(func (param \"path\" $path) (param \"offset\" u64) (param \"limit\" u32) (result $result))")
        .replace("(param i32 i32 i32 i32 i32)", "(param i32 i32 i32 i32 i64 i32 i32)")
        .replace("i32.const 32 call $read", &format!("i64.const {offset} i32.const {limit} i32.const 32 call $read"));
    wat::parse_str(source).unwrap()
}

pub fn forbidden() -> Vec<u8> {
    wat::parse_str(
        READER
            .replace(
                "(component",
                "(component (import \"wasi:sockets/tcp@0.2.3\" (instance))",
            )
            .replace(
                ";; initialization-probe",
                "(func $start unreachable) (start $start)",
            ),
    )
    .unwrap()
}

pub fn configure(config: &Path, bytes: &[u8], grants: &str) {
    let root = config.parent().unwrap().join("plugins/reader");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("plugin.toml"),
        "id='reader'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['tool','fs-read']\n",
    )
    .unwrap();
    fs::write(root.join("component.wasm"), bytes).unwrap();
    let base = fs::read_to_string(config).unwrap();
    fs::write(config, format!("{base}\n[plugins.reader]\nenabled=true\nmanifest='reader/plugin.toml'\nsha256='{}'\n[plugin.reader]\n{grants}\n", sha256_hex(bytes))).unwrap();
}

pub const GRANTS: &str = "tool={}\nfs-read=[{root='workspace',exclude=['private']}]";
