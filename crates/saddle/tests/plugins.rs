//! usable plugins through the real binary, with isolated home/config/state and compiled fixtures.

#[path = "support/plugin.rs"]
mod fixture;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use saddle_policy::audit::sha256_hex;
use serde_json::Value;
use tempfile::TempDir;

struct Env(TempDir);
impl Env {
    fn new(bytes: &[u8], grants: &str) -> Self {
        let env = Self(tempfile::tempdir().unwrap());
        fs::create_dir_all(env.ws()).unwrap();
        fs::create_dir_all(env.0.path().join("home")).unwrap();
        let config = env.config();
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "[agent]\nfs-read=[{root='workspace',exclude=['.env*']}]\nfs-write=[{root='workspace'}]\n").unwrap();
        fixture::configure(&config, bytes, grants);
        env
    }
    fn ws(&self) -> PathBuf {
        self.0.path().join("workspace")
    }
    fn config(&self) -> PathBuf {
        self.0.path().join("config/saddle/config.toml")
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_saddle"))
            .arg("-C")
            .arg(self.ws())
            .args(args)
            .env("HOME", self.0.path().join("home"))
            .env("XDG_CONFIG_HOME", self.0.path().join("config"))
            .env("XDG_STATE_HOME", self.0.path().join("state"))
            .output()
            .unwrap()
    }
    fn edit(&self, old: &str, new: &str) {
        let text = fs::read_to_string(self.config()).unwrap();
        assert!(text.contains(old));
        fs::write(self.config(), text.replace(old, new)).unwrap();
    }
    fn log(&self) -> PathBuf {
        let dir = self.0.path().join("state/saddle/audit");
        let mut files: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort_by_key(|p| p.metadata().unwrap().modified().unwrap());
        files.pop().unwrap()
    }
    fn records(&self) -> Vec<Value> {
        fs::read_to_string(self.log())
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
}
fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}
fn call(env: &Env) -> Output {
    env.run(&["plugin", "call", "reader", "{}"])
}
fn no_file_reads(env: &Env) {
    assert!(
        !env.records().iter().any(|r| r["scope"] == "fs-read"),
        "{:?}",
        env.records()
    );
}

#[test]
fn enabled_plugin_is_callable_through_both_cli_paths_with_audited_identity() {
    let env = Env::new(&fixture::reader("file"), fixture::GRANTS);
    fs::write(env.ws().join("file"), "hello from a component").unwrap();
    let listing = env.run(&["plugin", "list"]);
    assert!(listing.status.success(), "{}", text(&listing));
    assert!(
        text(&listing).contains("ready: plugin__reader__reader"),
        "{}",
        text(&listing)
    );
    for args in [
        vec!["plugin", "call", "reader", "{}"],
        vec!["tool", "plugin__reader__reader", "{}"],
    ] {
        let out = env.run(&args);
        assert!(out.status.success(), "{}", text(&out));
        assert!(text(&out).contains("hello from a component"));
        assert!(text(&out).contains("<plugin-output-"));
        let rows = env.records();
        let read = rows.iter().find(|r| r["scope"] == "fs-read").unwrap();
        assert_eq!(read["principal"], "plugin.reader");
        assert_eq!(read["args"]["path"], "workspace:file");
        assert_eq!(read["rule"], "plugin.reader.fs-read[0]");
        assert!(read["args"]["call_id"].is_string());
        let log = env.log();
        let verify = env.run(&["audit", "verify", log.to_str().unwrap()]);
        assert!(verify.status.success(), "{}", text(&verify));
    }
}

#[test]
fn parent_and_plugin_exclusions_and_link_targets_are_enforced() {
    for path in [".env", "private", "alias"] {
        let env = Env::new(&fixture::reader(path), fixture::GRANTS);
        fs::write(env.ws().join(".env"), "secret parent").unwrap();
        fs::write(env.ws().join("private"), "secret plugin").unwrap();
        symlink("private", env.ws().join("alias")).unwrap();
        let out = call(&env);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out));
        assert!(!text(&out).contains("secret"));
        let rows = env.records();
        assert!(rows.iter().any(|r| r["scope"] == "fs-read"
            && r["allow"] == false
            && r["principal"] == "plugin.reader"));
        if path == "alias" {
            assert!(
                rows.iter()
                    .any(|r| r["args"]["path"] == "workspace:alias" && r["allow"] == true)
            );
            assert!(
                rows.iter()
                    .any(|r| r["args"]["path"] == "workspace:private" && r["allow"] == false)
            );
        }
    }
}

#[test]
fn a_relative_link_to_an_allowed_file_is_read_and_both_names_are_audited() {
    let env = Env::new(&fixture::reader("alias"), fixture::GRANTS);
    fs::write(env.ws().join("file"), "allowed link target").unwrap();
    symlink("file", env.ws().join("alias")).unwrap();
    let out = call(&env);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("allowed link target"));
    let rows = env.records();
    for path in ["workspace:alias", "workspace:file"] {
        assert!(rows.iter().any(|r| r["args"]["path"] == path
            && r["allow"] == true
            && r["principal"] == "plugin.reader"));
    }
}

#[test]
fn parent_extra_roots_and_an_active_siblings_grants_cannot_supply_missing_reach() {
    let bytes = fixture::reader_on_root("scratch", "file");
    let env = Env::new(&bytes, fixture::GRANTS);
    let scratch = env.0.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    fs::write(scratch.join("file"), "SIBLING-ONLY-CONTENT").unwrap();
    env.edit(
        "fs-read=[{root='workspace',exclude=['.env*']}]",
        "fs-read=[{root='workspace',exclude=['.env*']},{root='scratch'}]",
    );
    let sibling = env.config().parent().unwrap().join("plugins/sibling");
    fs::create_dir(&sibling).unwrap();
    fs::write(sibling.join("component.wasm"), &bytes).unwrap();
    fs::write(
        sibling.join("plugin.toml"),
        "id='sibling'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['tool','fs-read']\n",
    )
    .unwrap();
    let config = fs::read_to_string(env.config()).unwrap();
    fs::write(env.config(), format!("{config}\n[roots]\nscratch={:?}\n[plugin.sibling]\ntool={{}}\nfs-read=[{{root='scratch'}}]\n[plugins.sibling]\nenabled=true\nmanifest='sibling/plugin.toml'\nsha256='{}'\n", scratch.to_str().unwrap(), sha256_hex(&bytes))).unwrap();
    let out = call(&env);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(!text(&out).contains("SIBLING-ONLY-CONTENT"));
    assert!(
        env.records()
            .iter()
            .any(|r| r["principal"] == "plugin.reader"
                && r["args"]["path"] == "scratch:file"
                && r["allow"] == false)
    );
    let out = env.run(&["plugin", "call", "sibling", "{}"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("SIBLING-ONLY-CONTENT"));
}

#[test]
fn absolute_escaping_and_dangling_links_cannot_leave_the_root() {
    for target in ["../outside", "/etc/passwd", "../missing"] {
        let env = Env::new(&fixture::reader("link"), fixture::GRANTS);
        fs::write(env.0.path().join("outside"), "secret outside").unwrap();
        symlink(target, env.ws().join("link")).unwrap();
        let out = call(&env);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out));
        assert!(!text(&out).contains("secret outside"));
        assert!(
            env.records()
                .iter()
                .any(|r| r["scope"] == "fs-read" && r["allow"] == false)
        );
    }
}

#[test]
fn disabled_ungranted_and_above_agent_plugins_never_reach_host_reads() {
    for mode in ["disabled", "ungranted", "revoked"] {
        let env = Env::new(&fixture::reader("file"), fixture::GRANTS);
        fs::write(env.ws().join("file"), "secret").unwrap();
        match mode {
            "disabled" => env.edit("enabled=true", "enabled=false"),
            "ungranted" => env.edit("tool={}", ""),
            _ => env.edit(
                "[plugin.reader]",
                "[plugin.sibling]\nfs-read=[{root='workspace'}]\n[plugin.reader]",
            ),
        }
        let out = if mode == "revoked" {
            env.run(&["--revoke", "fs-read", "plugin", "call", "reader", "{}"])
        } else {
            call(&env)
        };
        assert_eq!(out.status.code(), Some(1), "{}", text(&out));
        assert!(!text(&out).contains("secret"));
        no_file_reads(&env);
    }
}

#[test]
fn digest_identity_and_artifact_symlink_mismatches_fail_before_guest_execution() {
    for mode in ["digest", "identity", "link", "escape"] {
        let env = Env::new(&fixture::reader("file"), fixture::GRANTS);
        let plugin = env.config().parent().unwrap().join("plugins/reader");
        match mode {
            "digest" => fs::write(plugin.join("component.wasm"), b"changed artifact").unwrap(),
            "identity" => {
                let manifest = fs::read_to_string(plugin.join("plugin.toml")).unwrap();
                fs::write(
                    plugin.join("plugin.toml"),
                    manifest.replace("id='reader'", "id='sibling'"),
                )
                .unwrap();
            }
            "link" => {
                fs::rename(plugin.join("component.wasm"), env.ws().join("artifact")).unwrap();
                symlink(env.ws().join("artifact"), plugin.join("component.wasm")).unwrap();
            }
            _ => env.edit("reader/plugin.toml", "../plugin.toml"),
        }
        let listing = env.run(&["plugin", "list"]);
        assert!(listing.status.success(), "{}", text(&listing));
        assert!(
            text(&listing).contains("plugin load failed"),
            "{}",
            text(&listing)
        );
        no_file_reads(&env);
        assert_eq!(call(&env).status.code(), Some(1));
    }
}

#[test]
fn forbidden_interfaces_do_not_initialize_and_native_tools_stay_available() {
    let env = Env::new(&fixture::forbidden(), fixture::GRANTS);
    let listing = env.run(&["plugin", "list"]);
    assert!(text(&listing).contains("raw sockets"), "{}", text(&listing));
    no_file_reads(&env);
    fs::write(env.ws().join("native"), "native tools still work").unwrap();
    let out = env.run(&["tool", "read", r#"{"path":"native"}"#]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("native tools still work"));
    assert!(!text(&out).contains("<plugin-output-"));
}

#[test]
fn special_and_oversized_files_fail_without_unbounded_reads() {
    for name in ["fifo", "large"] {
        let env = Env::new(&fixture::reader(name), fixture::GRANTS);
        if name == "fifo" {
            assert!(
                Command::new("mkfifo")
                    .arg(env.ws().join(name))
                    .status()
                    .unwrap()
                    .success()
            );
        } else {
            fs::write(env.ws().join(name), vec![b'x'; 65537]).unwrap();
        }
        let start = Instant::now();
        let out = call(&env);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out));
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}

#[test]
fn image_results_need_no_filesystem_authority_and_reject_invalid_bytes() {
    let env = Env::new(&fixture::image_emitter(&fixture::png(), false), "tool={}");
    let out = call(&env);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("image/png"));
    assert!(
        env.records()
            .iter()
            .any(|r| r["args"]["operation"] == "emit-image" && r["principal"] == "plugin.reader")
    );
    no_file_reads(&env);
    let env = Env::new(&fixture::image_emitter(b"not an image", false), "tool={}");
    assert_eq!(call(&env).status.code(), Some(1));
}

#[test]
fn metadata_services_use_plugin_grants_and_do_not_borrow_native_tool_path_authority() {
    let env = Env::new(&fixture::directory_reader(".", "", 1), fixture::GRANTS);
    fs::write(env.ws().join(".env"), "parent secret").unwrap();
    fs::write(env.ws().join("private"), "plugin secret").unwrap();
    fs::write(env.ws().join("visible"), "ok").unwrap();
    let out = call(&env);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("visible"));
    assert!(!text(&out).contains(".env"));
    assert!(!text(&out).contains("private"));
    assert!(
        env.records()
            .iter()
            .any(|r| r["args"]["operation"] == "read-directory"
                && r["principal"] == "plugin.reader"
                && r["args"]["limit"] == 1)
    );
    for limit in [0, 129] {
        let env = Env::new(&fixture::directory_reader(".", "", limit), fixture::GRANTS);
        assert_eq!(call(&env).status.code(), Some(1));
    }
    for path in ["private", ".env", "../outside", "~/.agents/secret"] {
        let env = Env::new(&fixture::path_resolver(path), fixture::GRANTS);
        assert_eq!(call(&env).status.code(), Some(1), "{path}");
    }
    let env = Env::new(&fixture::path_resolver("sub/file"), fixture::GRANTS);
    let out = call(&env);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("sub/file"));
    let env = Env::new(&fixture::path_resolver("~/.agents/secret"), fixture::GRANTS);
    let shared = env.0.path().join("home/.agents");
    fs::create_dir_all(&shared).unwrap();
    fs::write(shared.join("secret"), "managed-native-content").unwrap();
    let arguments = format!(
        r#"{{"path":{:?}}}"#,
        shared.join("secret").to_str().unwrap()
    );
    let native = env.run(&["tool", "read", &arguments]);
    assert!(native.status.success(), "{}", text(&native));
    assert!(text(&native).contains("managed-native-content"));
    let out = call(&env);
    assert_eq!(out.status.code(), Some(1));
    assert!(!text(&out).contains("managed-native-content"));
    let env = Env::new(&fixture::directory_reader(".", "", 1), "tool={}");
    assert_eq!(call(&env).status.code(), Some(1));
    no_file_reads(&env);
}

#[test]
fn ranged_reads_are_bounded_audited_and_cannot_bypass_existing_read_policy() {
    let env = Env::new(&fixture::range_reader("file", 70000, 4), fixture::GRANTS);
    let mut data = vec![b'x'; 70000];
    data.extend_from_slice(b"tail-more");
    fs::write(env.ws().join("file"), data).unwrap();
    let out = call(&env);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("tail"));
    assert!(!text(&out).contains("tail-more"));
    let rows = env.records();
    let row = rows.iter().find(|r| r["scope"] == "fs-read").unwrap();
    assert_eq!(row["args"]["offset"], 70000);
    assert_eq!(row["args"]["limit"], 4);
    for limit in [0, 65537, u32::MAX] {
        let env = Env::new(&fixture::range_reader("file", 0, limit), fixture::GRANTS);
        fs::write(env.ws().join("file"), "secret").unwrap();
        let out = call(&env);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out));
        assert!(!text(&out).contains("secret"));
        assert!(
            env.records()
                .iter()
                .any(|r| r["scope"] == "fs-read" && r["allow"] == false)
        );
    }
    for path in ["private", "alias", ".env"] {
        let env = Env::new(&fixture::range_reader(path, 0, 4), fixture::GRANTS);
        fs::write(env.ws().join("private"), "secret").unwrap();
        fs::write(env.ws().join(".env"), "secret").unwrap();
        symlink("private", env.ws().join("alias")).unwrap();
        let out = call(&env);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out));
        assert!(!text(&out).contains("secret"));
    }
    let env = Env::new(&fixture::range_reader("file", 0, 4), "tool={}");
    assert_eq!(call(&env).status.code(), Some(1));
    no_file_reads(&env);
}

#[test]
#[ignore = "build the standard read component first; see docs/plugins.md"]
fn standard_read_component_paginates_large_files_through_the_production_host() {
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/read-plugin/read.component.wasm");
    let bytes =
        fs::read(component).expect("build the standard read component first; see docs/plugins.md");
    let env = Env::new(&bytes, fixture::GRANTS);
    fs::write(env.ws().join("file"), "hello\n".repeat(20_000)).unwrap();
    let out = env.run(&[
        "plugin",
        "call",
        "reader",
        r#"{"path":"file","offset":15000,"limit":2}"#,
    ]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("hello\nhello\n"));
    assert!(text(&out).contains("offset=15002"));
    let rows = env.records();
    let reads: Vec<_> = rows
        .iter()
        .filter(|row| row["scope"] == "fs-read" && row["args"]["offset"].is_u64())
        .collect();
    assert!(reads.len() > 1);
    assert!(reads.iter().all(|row| row["principal"] == "plugin.reader"
        && matches!(row["args"]["limit"].as_u64(), Some(8192 | 65536))));
    assert!(
        reads
            .iter()
            .any(|row| row["args"]["offset"].as_u64().unwrap() > 65536)
    );
    let log = env.log();
    assert!(
        env.run(&["audit", "verify", log.to_str().unwrap()])
            .status
            .success()
    );
    fs::write(env.ws().join("private"), "secret").unwrap();
    symlink("private", env.ws().join("alias")).unwrap();
    for path in [".env", "private", "alias"] {
        fs::write(env.ws().join(".env"), "secret").unwrap();
        let arguments = format!(r#"{{"path":{path:?}}}"#);
        let out = env.run(&["plugin", "call", "reader", &arguments]);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out));
        assert!(!text(&out).contains("secret"));
    }
    let absolute = format!(
        r#"{{"path":{:?},"offset":2,"limit":1}}"#,
        env.ws().join("file").to_str().unwrap()
    );
    let out = env.run(&["plugin", "call", "reader", &absolute]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("offset=3"));
    fs::write(env.ws().join("deep"), "deep line\n".repeat(180_000)).unwrap();
    let out = env.run(&[
        "plugin",
        "call",
        "reader",
        r#"{"path":"deep","offset":170000,"limit":2}"#,
    ]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("lines 170000-170001 of 180000"));
    let mut image = fixture::png();
    image.resize(100_000, 0);
    fs::write(env.ws().join("misnamed.txt"), image).unwrap();
    let out = env.run(&["plugin", "call", "reader", r#"{"path":"misnamed.txt"}"#]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("1×1, image/png"));
    assert!(
        env.records()
            .iter()
            .any(|row| row["args"]["operation"] == "emit-image")
    );
}

#[test]
#[ignore = "build the standard ls component first; see docs/plugins.md"]
fn standard_ls_component_lists_filtered_pages_and_never_follows_entry_links() {
    let component =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ls-plugin/ls.component.wasm");
    let bytes =
        fs::read(component).expect("build the standard ls component first; see docs/plugins.md");
    let env = Env::new(&bytes, fixture::GRANTS);
    fs::create_dir(env.ws().join("dir")).unwrap();
    for n in 0..600 {
        fs::write(env.ws().join("dir").join(format!("file{n:04}")), "ok").unwrap();
    }
    fs::write(env.ws().join("dir/private"), "secret").unwrap();
    fs::write(env.ws().join("dir/.env"), "secret").unwrap();
    let args = format!(r#"{{"path":{:?}}}"#, env.ws().join("dir").to_str().unwrap());
    let out = env.run(&["plugin", "call", "reader", &args]);
    assert!(out.status.success(), "{}", text(&out));
    let output = text(&out);
    assert!(output.contains("first 500 of 600 entries"));
    assert!(output.contains("2 paths skipped"));
    assert!(!output.contains("private"));
    assert!(!output.contains("file0500"));
    assert_eq!(
        env.records()
            .iter()
            .filter(|row| row["args"]["operation"] == "read-directory")
            .count(),
        4
    );
    fs::create_dir(env.ws().join("empty")).unwrap();
    let out = env.run(&["plugin", "call", "reader", r#"{"path":"empty"}"#]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("(empty)"));
    fs::create_dir(env.ws().join("links")).unwrap();
    fs::create_dir(env.ws().join("links/subdir")).unwrap();
    symlink("/etc", env.ws().join("links/outside")).unwrap();
    let out = env.run(&["plugin", "call", "reader", r#"{"path":"links"}"#]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("outside@"));
    assert!(text(&out).contains("subdir/"));
    let out = env.run(&["plugin", "call", "reader", r#"{"path":"links/outside"}"#]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
}

#[test]
#[ignore = "build standard find/grep components first; see docs/plugins.md"]
fn standard_search_components_walk_and_match_without_bypassing_exclusions() {
    for (tool, filename) in [
        ("find", "find-plugin/find.component.wasm"),
        ("grep", "grep-plugin/grep.component.wasm"),
    ] {
        let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(filename);
        let bytes = fs::read(component)
            .expect("build standard find/grep components first; see docs/plugins.md");
        let env = Env::new(&bytes, fixture::GRANTS);
        fs::create_dir_all(env.ws().join("src")).unwrap();
        fs::create_dir_all(env.ws().join(".git")).unwrap();
        fs::write(env.ws().join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(env.ws().join(".git/hidden.rs"), "HELLO secret").unwrap();
        fs::write(env.ws().join("ignored.rs"), "HELLO secret").unwrap();
        fs::write(env.ws().join("private"), "HELLO secret").unwrap();
        fs::write(env.ws().join(".env"), "HELLO secret").unwrap();
        fs::write(
            env.ws().join("src/file.rs"),
            format!("{}\nHELLO Rust\n", "x".repeat(70_000)),
        )
        .unwrap();
        symlink("src/file.rs", env.ws().join("link.rs")).unwrap();
        let arguments = if tool == "find" {
            r#"{"pattern":"*.rs"}"#
        } else {
            r#"{"pattern":"hello","ignore_case":true,"glob":"*.rs"}"#
        };
        let out = env.run(&["plugin", "call", "reader", arguments]);
        assert!(out.status.success(), "{tool}: {}", text(&out));
        let output = text(&out);
        assert!(output.contains("src/file.rs"), "{output}");
        if tool == "grep" {
            assert!(output.contains("src/file.rs:2: HELLO Rust"), "{output}");
        }
        assert!(!output.contains("ignored.rs"));
        assert!(!output.contains("hidden.rs"));
        assert!(!output.contains("link.rs"));
        assert!(!output.contains("secret"));
        let rows = env.records();
        assert!(
            rows.iter()
                .filter(|r| r["scope"] == "fs-read")
                .all(|r| r["principal"] == "plugin.reader")
        );
        let log = env.log();
        assert!(
            env.run(&["audit", "verify", log.to_str().unwrap()])
                .status
                .success()
        );
    }
}

#[test]
fn workspace_plugin_grants_are_layered_and_one_run_revocation_keeps_them_bounded() {
    let env = Env::new(&fixture::reader("file"), "");
    fs::write(env.ws().join("file"), "workspace-scoped plugin").unwrap();
    assert_eq!(call(&env).status.code(), Some(1));
    let workspace = env.ws().canonicalize().unwrap();
    let key = workspace
        .to_string_lossy()
        .trim_start_matches('/')
        .replace(['/', '\\', ':'], "-");
    let policies = env.config().parent().unwrap().join("policies");
    fs::create_dir_all(&policies).unwrap();
    fs::write(
        policies.join(format!("--{key}--.toml")),
        format!(
            "workspace={:?}\n[plugin.reader]\n{}\n",
            workspace.to_str().unwrap(),
            fixture::GRANTS
        ),
    )
    .unwrap();
    let out = call(&env);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("workspace-scoped plugin"));
    let out = env.run(&["--revoke", "fs-read", "plugin", "call", "reader", "{}"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    no_file_reads(&env);
}

#[test]
fn installation_copies_and_pins_artifacts_but_does_not_enable_or_grant() {
    let env = Env::new(&fixture::reader("file"), fixture::GRANTS);
    let source = env.ws().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("reader.wasm"), fixture::reader("file")).unwrap();
    fs::write(
        source.join("plugin.toml"),
        "id='installed'\nversion='0.1.0'\ncomponent='reader.wasm'\nscopes=['fs-read','tool']\n",
    )
    .unwrap();
    let before = fs::read(env.config()).unwrap();
    let out = env.run(&[
        "plugin",
        "install",
        source.join("plugin.toml").to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("not enabled and no grants changed"));
    assert!(text(&out).contains("sha256 = \""));
    assert_eq!(fs::read(env.config()).unwrap(), before);
    let installed = env.config().parent().unwrap().join("plugins/installed");
    assert!(installed.join("component.wasm").is_file());
    assert!(installed.join("plugin.toml").is_file());
    assert_eq!(
        env.run(&[
            "plugin",
            "install",
            source.join("plugin.toml").to_str().unwrap()
        ])
        .status
        .code(),
        Some(2)
    );
    let out = env.run(&["plugin", "call", "installed", "{}"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
}
