//! tiny compiled components shared by plugin CLI/model tests; no guest target is needed.

use std::fs;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use saddle_policy::audit::sha256_hex;

const READER: &str = include_str!("../../../saddle-loader/src/fixtures/read-tool.wat");
const TRANSCRIPT: &str = include_str!("../../../saddle-loader/src/fixtures/transcript-tool.wat");

pub fn transcript_query(sql: &str) -> Vec<u8> {
    let escaped: String = sql.bytes().map(|byte| format!("\\{byte:02x}")).collect();
    wat::parse_str(
        TRANSCRIPT
            .replace(
                ";; initialization-probe",
                &format!("(data (i32.const 256) \"{escaped}\")"),
            )
            .replace(
                "local.get $sql local.get $len i32.const 32 call $query",
                &format!(
                    "i32.const 256 i32.const {} i32.const 32 call $query",
                    sql.len()
                ),
            ),
    )
    .unwrap()
}

#[allow(dead_code)] // shared with CLI tests, which configure an existing fixture installation.
pub fn configure_transcript(config: &Path, bytes: &[u8], grants: &str) {
    configure(config, bytes, grants);
    fs::write(config.parent().unwrap().join("plugins/reader/plugin.toml"),
        "id='reader'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['tool','transcript-read']\n").unwrap();
}

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
    wat::parse_str(range_source("workspace", path, offset, limit)).unwrap()
}

#[allow(dead_code)] // managed-path fixtures are only used by the CLI suite.
pub fn tool_range_reader(root: &str, path: &str, offset: u64, limit: u32) -> Vec<u8> {
    wat::parse_str(
        range_source(root, path, offset, limit).replace("fs-read-range@0.1.0", "tool-paths@0.1.0"),
    )
    .unwrap()
}

fn range_source(root: &str, path: &str, offset: u64, limit: u32) -> String {
    reader_source(root, path)
        .replace("fs-read@0.1.0", "fs-read-range@0.1.0")
        .replace("\"read-file\"", "\"read-range\"")
        .replace("(type $result (result (list u8) (error $failure)))", "(type $chunk-record (record (field \"bytes\" (list u8)) (field \"eof\" bool))) (export \"chunk\" (type $chunk (eq $chunk-record))) (type $result (result $chunk (error $failure)))")
        .replace("(func (param \"path\" $path) (result $result))", "(func (param \"path\" $path) (param \"offset\" u64) (param \"limit\" u32) (result $result))")
        .replace("(param i32 i32 i32 i32 i32)", "(param i32 i32 i32 i32 i64 i32 i32)")
        .replace("i32.const 32 call $read", &format!("i64.const {offset} i32.const {limit} i32.const 32 call $read"))
}

#[allow(dead_code)] // shared with model tests that do not need metadata fixtures.
pub fn directory_reader(path: &str, after: &str, limit: u32) -> Vec<u8> {
    assert!(after.is_ascii() && !after.contains(['"', '\\']));
    let source = reader_source("workspace", path)
        .replace("fs-read@0.1.0", "fs-list@0.1.0")
        .replace("\"read-file\"", "\"read-directory\"")
        .replace("(type $result (result (list u8) (error $failure)))", "(type $kind-type (enum \"file\" \"directory\" \"symlink\" \"other\")) (export \"entry-kind\" (type $kind (eq $kind-type))) (type $entry-type (record (field \"name\" string) (field \"kind\" $kind))) (export \"entry\" (type $entry (eq $entry-type))) (type $page-type (record (field \"entries\" (list $entry)) (field \"total\" u32) (field \"skipped\" u32) (field \"more\" bool))) (export \"page\" (type $page (eq $page-type))) (type $result (result $page (error $failure)))")
        .replace("(func (param \"path\" $path) (result $result))", "(func (param \"path\" $path) (param \"after\" string) (param \"limit\" u32) (result $result))")
        .replace("(param i32 i32 i32 i32 i32)", "(param i32 i32 i32 i32 i32 i32 i32 i32)")
        .replace("(data (i32.const 272) \"denied\")", &format!("(data (i32.const 272) \"denied\") (data (i32.const 512) {after:?})"))
        .replace("i32.const 32 call $read", &format!("i32.const 512 i32.const {} i32.const {limit} i32.const 32 call $read", after.len()))
        .replace("i32.const 64 i32.const 36 i32.load i32.store", "i32.const 64 i32.const 36 i32.load i32.load i32.store")
        .replace("i32.const 68 i32.const 40 i32.load i32.store", "i32.const 68 i32.const 36 i32.load i32.const 4 i32.add i32.load i32.store");
    wat::parse_str(source).unwrap()
}

#[allow(dead_code)] // shared with model tests that do not need metadata fixtures.
pub fn path_resolver(path: &str) -> Vec<u8> {
    let source = reader_source("workspace", path)
        .replace("fs-read@0.1.0", "fs-paths@0.1.0")
        .replace("\"read-file\"", "\"resolve-path\"")
        .replace(
            "(result (list u8) (error $failure))",
            "(result $path (error $failure))",
        )
        .replace(
            "(func (param \"path\" $path) (result $result))",
            "(func (param \"value\" string) (result $result))",
        )
        .replace("(param i32 i32 i32 i32 i32)", "(param i32 i32 i32)")
        .replace("i32.const 256 i32.const 9 ", "")
        .replace("i32.const 36 i32.load", "i32.const 44 i32.load")
        .replace("i32.const 40 i32.load", "i32.const 48 i32.load");
    wat::parse_str(source).unwrap()
}

pub fn png() -> Vec<u8> {
    STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/lXkAAAAASUVORK5CYII=").unwrap()
}

pub fn image_emitter(data: &[u8], fail_after: bool) -> Vec<u8> {
    let escaped: String = data.iter().map(|byte| format!("\\{byte:02x}")).collect();
    let mut source = reader_source("workspace", "dummy")
        .replace("fs-read@0.1.0", "tool-result@0.1.0")
        .replace("\"read-file\"", "\"emit-image\"")
        .replace("(type $result (result (list u8) (error $failure)))", "(type $info-type (record (field \"media-type\" string) (field \"width\" u32) (field \"height\" u32))) (export \"image-info\" (type $info (eq $info-type))) (type $result (result $info (error string)))")
        .replace("(func (param \"path\" $path) (result $result))", "(func (param \"name\" string) (param \"bytes\" (list u8)) (result $result))")
        .replace("(data (i32.const 300) \"dummy\")", &format!("(data (i32.const 300) \"{escaped}\")"))
        .replace("i32.const 300 i32.const 5", &format!("i32.const 300 i32.const {}", data.len()));
    if fail_after {
        source = source.replace("i32.const 64)", "unreachable i32.const 64)");
    }
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
