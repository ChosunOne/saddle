//! bounded, grant-shaped component loading (D11, D13, D126, D127).

mod ambient;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use saddle_cap::component::{Direction, check_component};
use saddle_cap::delegate::{self, DelegationError};
use saddle_cap::manifest::{Manifest, above_ceiling};
use saddle_cap::{Grants, Principal, Scope};
use saddle_wit::ReadToolPre;
use saddle_wit::exports::saddle::plugin::tool::{Definition, Output};
use saddle_wit::saddle::host::fs_list::{self, Host as ListHost, Page as DirectoryPage};
use saddle_wit::saddle::host::fs_paths::{self, Host as PathsHost};
use saddle_wit::saddle::host::fs_read::{self, Failure, Host, Path};
use saddle_wit::saddle::host::fs_read_range::{self, Chunk, Host as RangeHost};
use saddle_wit::saddle::host::tool_result::{self, Host as ResultHost, ImageInfo};
use sha2::{Digest, Sha256};
use wasmtime::component::types::{Component as ComponentType, ComponentItem};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

/// native I/O must enforce this cap before allocating or reading the entire source.
pub const MAX_FILE_BYTES: usize = 64 * 1024;
pub const MAX_DIRECTORY_ENTRIES: usize = 128;
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_COMPONENT_BYTES: usize = 4 * 1024 * 1024;
const TICK: Duration = Duration::from_millis(10);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Delegation(#[from] DelegationError),
    #[error("plugin policy exceeds manifest ceiling: {0:?}")]
    Ceiling(Vec<Scope>),
    #[error("component refused: {0}")]
    Refused(String),
    #[error("{stage}: {detail}")]
    Runtime { stage: &'static str, detail: String },
    #[error("plugin input or output exceeds {0} bytes")]
    Size(usize),
    #[error("plugin call cancelled")]
    Cancelled,
}

fn runtime(stage: &'static str) -> impl FnOnce(anyhow::Error) -> Error {
    move |err| Error::Runtime {
        stage,
        detail: format!("{err:#}"),
    }
}

/// host-chosen bounds; callers may lower them, never let the guest choose them.
#[derive(Clone, Debug)]
pub struct Limits {
    pub memory_bytes: usize,
    pub fuel: u64,
    pub timeout: Duration,
    pub input_bytes: usize,
    pub output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 16 * 1024 * 1024,
            fuel: 100_000_000,
            timeout: Duration::from_secs(2),
            input_bytes: 64 * 1024,
            output_bytes: 64 * 1024,
        }
    }
}

/// identity and effective reach assigned by the host, never by guest arguments.
pub struct Invocation {
    principal: Principal,
    id: u64,
    grants: Arc<Grants>,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl Invocation {
    pub fn principal(&self) -> &Principal {
        &self.principal
    }
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn grants(&self) -> &Grants {
        &self.grants
    }
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}

/// the binary supplies policy/audit/file operations; the loader never depends on its session.
///
/// Implementations must decide and audit using `call.principal()` and `call.grants()`, validate
/// root-relative paths and symlinks, and bound/cancel I/O independently of Wasm's epoch deadline.
/// `Err` is a host failure (including audit failure), not a guest-visible policy denial.
pub trait HostServices: Send + 'static {
    fn emit_image(
        &mut self,
        _call: &Invocation,
        _name: String,
        _bytes: Vec<u8>,
    ) -> anyhow::Result<Result<ImageInfo, String>> {
        Ok(Err("host does not implement image results".into()))
    }
    fn read_file(
        &mut self,
        call: &Invocation,
        path: Path,
    ) -> anyhow::Result<Result<Vec<u8>, Failure>>;

    fn resolve_path(
        &mut self,
        _call: &Invocation,
        _value: String,
    ) -> anyhow::Result<Result<Path, Failure>> {
        Ok(Err(Failure::Io(
            "host does not implement path resolution".into(),
        )))
    }

    fn read_directory(
        &mut self,
        _call: &Invocation,
        _path: Path,
        _after: String,
        _limit: u32,
    ) -> anyhow::Result<Result<DirectoryPage, Failure>> {
        Ok(Err(Failure::Io(
            "host does not implement directory reads".into(),
        )))
    }

    /// serves a bounded range under the same grants; hosts without this service fail closed.
    fn read_range(
        &mut self,
        _call: &Invocation,
        _path: Path,
        _offset: u64,
        _limit: u32,
    ) -> anyhow::Result<Result<Chunk, Failure>> {
        Ok(Err(Failure::Io(
            "host does not implement ranged reads".into(),
        )))
    }
}

struct State<H> {
    host: H,
    call: Invocation,
    limits: StoreLimits,
    wasi: WasiCtx,
    resources: ResourceTable,
    emitted_images: usize,
}

impl<H: HostServices> WasiView for State<H> {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.resources,
        }
    }
}

impl<H: HostServices> Host for State<H> {
    fn read_file(&mut self, path: Path) -> anyhow::Result<Result<Vec<u8>, Failure>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("plugin call cancelled or timed out"));
        }
        let result = self.host.read_file(&self.call, path)?;
        if matches!(&result, Ok(bytes) if bytes.len() > MAX_FILE_BYTES) {
            return Ok(Err(Failure::TooLarge));
        }
        Ok(result)
    }
}

impl<H: HostServices> RangeHost for State<H> {
    fn read_range(
        &mut self,
        path: Path,
        offset: u64,
        limit: u32,
    ) -> anyhow::Result<Result<Chunk, Failure>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("plugin call cancelled or timed out"));
        }
        let result = self.host.read_range(&self.call, path, offset, limit)?;
        if matches!(&result, Ok(chunk) if chunk.bytes.len() > MAX_FILE_BYTES || chunk.bytes.len() as u64 > u64::from(limit))
        {
            return Ok(Err(Failure::TooLarge));
        }
        Ok(result)
    }
}

impl<H: HostServices> PathsHost for State<H> {
    fn resolve_path(&mut self, value: String) -> anyhow::Result<Result<Path, Failure>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("plugin call cancelled or timed out"));
        }
        let result = self.host.resolve_path(&self.call, value)?;
        if matches!(&result, Ok(path) if path.root.len() > 64 || path.relative.len() > 4096) {
            return Ok(Err(Failure::TooLarge));
        }
        Ok(result)
    }
}

impl<H: HostServices> ListHost for State<H> {
    fn read_directory(
        &mut self,
        path: Path,
        after: String,
        limit: u32,
    ) -> anyhow::Result<Result<DirectoryPage, Failure>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("plugin call cancelled or timed out"));
        }
        let result = self.host.read_directory(&self.call, path, after, limit)?;
        if matches!(&result, Ok(page) if page.entries.len() > MAX_DIRECTORY_ENTRIES || page.entries.len() as u64 > u64::from(limit) || page.entries.iter().map(|entry| entry.name.len()).sum::<usize>() > MAX_FILE_BYTES)
        {
            return Ok(Err(Failure::TooLarge));
        }
        Ok(result)
    }
}

impl<H: HostServices> ResultHost for State<H> {
    fn emit_image(
        &mut self,
        name: String,
        bytes: Vec<u8>,
    ) -> anyhow::Result<Result<ImageInfo, String>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("plugin call cancelled or timed out"));
        }
        if name.len() > 256 || bytes.len() > MAX_IMAGE_BYTES || self.emitted_images != 0 {
            return Ok(Err("image result exceeds name, byte or count limit".into()));
        }
        let result = self.host.emit_image(&self.call, name, bytes)?;
        if let Ok(info) = &result {
            if info.media_type.len() > 64 {
                return Ok(Err("invalid image metadata".into()));
            }
            self.emitted_images += 1;
        }
        Ok(result)
    }
}

struct Runtime {
    engine: Engine,
    stop: Arc<AtomicBool>,
    ticker: Option<JoinHandle<()>>,
    next_call: AtomicU64,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.join();
        }
    }
}

/// shared compiler and epoch clock; prepared plugins keep both alive.
#[derive(Clone)]
pub struct Loader(Arc<Runtime>);

impl Loader {
    pub fn new() -> Result<Self, Error> {
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .consume_fuel(true)
            .epoch_interruption(true);
        config.max_wasm_stack(512 * 1024);
        let engine = Engine::new(&config).map_err(runtime("creating component engine"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let clock_stop = stop.clone();
        let clock_engine = engine.clone();
        let ticker = thread::Builder::new()
            .name("saddle-wasm-clock".into())
            .spawn(move || {
                while !clock_stop.load(Ordering::Relaxed) {
                    thread::sleep(TICK);
                    clock_engine.increment_epoch();
                }
            })
            .map_err(|e| Error::Runtime {
                stage: "starting component clock",
                detail: e.to_string(),
            })?;
        Ok(Self(Arc::new(Runtime {
            engine,
            stop,
            ticker: Some(ticker),
            next_call: AtomicU64::new(1),
        })))
    }

    /// checks the actual compiled type and pre-links it without running initialization.
    ///
    /// Bytes come from an explicit host choice, not a guest-selected path. This API neither
    /// installs plugins nor reads repository manifests or serialized native code.
    pub fn prepare<H: HostServices>(
        &self,
        manifest: &Manifest,
        agent: &Grants,
        requested: &Grants,
        bytes: &[u8],
        limits: Limits,
    ) -> Result<Prepared<H>, Error> {
        if bytes.len() > MAX_COMPONENT_BYTES {
            return Err(Error::Size(MAX_COMPONENT_BYTES));
        }
        let above = above_ceiling(manifest, requested);
        if !above.is_empty() {
            return Err(Error::Ceiling(above));
        }
        let grants = delegate::read_tool(agent, requested)?;
        let scopes = grants.scopes();
        let engine = &self.0.engine;
        let component =
            Component::from_binary(engine, bytes).map_err(runtime("compiling component"))?;
        let ty = component.component_type();
        let imports = interfaces(Direction::Import, ty.imports(engine))?;
        let exports = interfaces(Direction::Export, ty.exports(engine))?;
        check_component(
            &scopes,
            imports.iter().map(String::as_str),
            exports.iter().map(String::as_str),
        )
        .map_err(|e| {
            Error::Refused(
                e.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        })?;
        // There is deliberately no broad WASI linker helper: it would also define raw sockets.
        // Even an empty, unused import must be refused if its adapter is not implemented;
        // Wasmtime can otherwise elide it during pre-linking.
        for name in &imports {
            if !matches!(
                name.as_str(),
                "saddle:host/fs-read@0.1.0"
                    | "saddle:host/fs-read-range@0.1.0"
                    | "saddle:host/fs-paths@0.1.0"
                    | "saddle:host/fs-list@0.1.0"
                    | "saddle:host/tool-result@0.1.0"
            ) && !ambient::supports(name)
            {
                return Err(Error::Refused(format!(
                    "import `{name}` is not implemented by the initial runtime"
                )));
            }
        }
        let mut linker = Linker::<State<H>>::new(engine);
        ambient::add(&mut linker).map_err(runtime("linking ambient WASI"))?;
        if scopes.contains(&Scope::FsRead) {
            fs_read::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
                .map_err(runtime("linking fs-read"))?;
            fs_read_range::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
                .map_err(runtime("linking fs-read-range"))?;
            fs_paths::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
                .map_err(runtime("linking fs-paths"))?;
            fs_list::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
                .map_err(runtime("linking fs-list"))?;
        }
        if scopes.contains(&Scope::Tool) {
            tool_result::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
                .map_err(runtime("linking tool-result"))?;
        }
        check_tool_types(engine, &ty)?;
        let pre = linker
            .instantiate_pre(&component)
            .map_err(runtime("pre-linking component"))?;
        let pre = ReadToolPre::new(pre).map_err(runtime("checking tool export types"))?;
        Ok(Prepared {
            runtime: self.0.clone(),
            pre,
            principal: Principal::Plugin(manifest.id.clone()),
            grants: Arc::new(grants),
            hash: Sha256::digest(bytes).into(),
            limits,
        })
    }
}

/// the generated `Pre::new` checks export names, not their signatures. Check the canonical
/// generated record types before instantiation too, rather than discovering a mismatch after start.
fn check_tool_types(engine: &Engine, ty: &ComponentType) -> Result<(), Error> {
    let Some(ComponentItem::ComponentInstance(tool)) =
        ty.get_export(engine, "saddle:plugin/tool@0.1.0")
    else {
        return Err(Error::Refused(
            "missing saddle:plugin/tool@0.1.0 export".into(),
        ));
    };
    let Some(ComponentItem::ComponentFunc(describe)) = tool.get_export(engine, "describe") else {
        return Err(Error::Refused("missing tool describe function".into()));
    };
    let Some(ComponentItem::ComponentFunc(invoke)) = tool.get_export(engine, "invoke") else {
        return Err(Error::Refused("missing tool invoke function".into()));
    };
    // This pre-instantiation typecheck API is supplied by the deliberately pinned Wasmtime.
    describe
        .typecheck::<(), (Definition,)>(&ty.instance_type())
        .map_err(runtime("checking tool export types"))?;
    invoke
        .typecheck::<(&str,), (Output,)>(&ty.instance_type())
        .map_err(runtime("checking tool export types"))?;
    Ok(())
}

fn interfaces(
    direction: Direction,
    items: impl IntoIterator<Item = (impl AsRef<str>, ComponentItem)>,
) -> Result<Vec<String>, Error> {
    items
        .into_iter()
        .map(|(name, item)| {
            let name = name.as_ref();
            if name.starts_with("saddle:") && !name.ends_with("@0.1.0") {
                return Err(Error::Refused(format!(
                    "{direction} `{name}`: initial runtime supports saddle ABI 0.1.0"
                )));
            }
            if !matches!(item, ComponentItem::ComponentInstance(_)) {
                return Err(Error::Refused(format!(
                    "{direction} `{name}` is not an interface instance"
                )));
            }
            Ok(name.to_owned())
        })
        .collect()
}

/// one pre-linked plugin; every public call creates a fresh instance, even `describe`.
pub struct Prepared<H: HostServices> {
    runtime: Arc<Runtime>,
    pre: ReadToolPre<State<H>>,
    principal: Principal,
    grants: Arc<Grants>,
    hash: [u8; 32],
    limits: Limits,
}

impl<H: HostServices> Prepared<H> {
    fn check_live(store: &Store<State<H>>) -> Result<(), Error> {
        if store.data().call.cancelled() {
            return Err(Error::Cancelled);
        }
        if Instant::now() >= store.data().call.deadline {
            return Err(Error::Runtime {
                stage: "plugin call timed out",
                detail: "deadline elapsed".into(),
            });
        }
        Ok(())
    }

    pub fn hash(&self) -> &[u8; 32] {
        &self.hash
    }
    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    fn store(&self, host: H, cancelled: Arc<AtomicBool>) -> Result<Store<State<H>>, Error> {
        if cancelled.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let deadline = Instant::now()
            .checked_add(self.limits.timeout)
            .ok_or_else(|| Error::Refused("timeout is too large".into()))?;
        let state = State {
            host,
            call: Invocation {
                principal: self.principal.clone(),
                id: self.runtime.next_call.fetch_add(1, Ordering::Relaxed),
                grants: self.grants.clone(),
                cancelled,
                deadline,
            },
            limits: StoreLimitsBuilder::new()
                .memory_size(self.limits.memory_bytes)
                .table_elements(10_000)
                .instances(32)
                .memories(4)
                .tables(8)
                .trap_on_grow_failure(true)
                .build(),
            wasi: WasiCtx::builder()
                .max_random_size(MAX_FILE_BYTES as u64)
                .build(),
            resources: {
                let mut table = ResourceTable::new();
                table.set_max_capacity(128);
                table
            },
            emitted_images: 0,
        };
        let mut store = Store::new(&self.runtime.engine, state);
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(runtime("setting plugin fuel"))?;
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|cx| {
            let call = &cx.data().call;
            if call.cancelled() || Instant::now() >= call.deadline {
                return Err(anyhow!("plugin call cancelled or timed out"));
            }
            Ok(UpdateDeadline::Continue(1))
        });
        Self::check_live(&store)?;
        Ok(store)
    }

    pub fn describe(&self, host: H, cancelled: Arc<AtomicBool>) -> Result<Definition, Error> {
        let mut store = self.store(host, cancelled)?;
        let guest = self
            .pre
            .instantiate(&mut store)
            .map_err(runtime("initializing plugin"))?;
        let definition = guest
            .saddle_plugin_tool()
            .call_describe(&mut store)
            .map_err(runtime("describing plugin"))?;
        Self::check_live(&store)?;
        let total = definition
            .name
            .len()
            .saturating_add(definition.description.len())
            .saturating_add(definition.input_schema.len());
        if total > self.limits.output_bytes {
            return Err(Error::Size(self.limits.output_bytes));
        }
        Ok(definition)
    }

    pub fn invoke(
        &self,
        host: H,
        arguments: &str,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Output, Error> {
        if arguments.len() > self.limits.input_bytes {
            return Err(Error::Size(self.limits.input_bytes));
        }
        let mut store = self.store(host, cancelled)?;
        let guest = self
            .pre
            .instantiate(&mut store)
            .map_err(runtime("initializing plugin"))?;
        let output = guest
            .saddle_plugin_tool()
            .call_invoke(&mut store, arguments)
            .map_err(runtime("invoking plugin"))?;
        Self::check_live(&store)?;
        if output.text.len() > self.limits.output_bytes {
            return Err(Error::Size(self.limits.output_bytes));
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Read;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use cap_std::fs::Dir;
    use saddle_cap::{Location, PluginId, Policy, RelPath, RootName, Roots};
    use saddle_policy::audit::{self, AuditLog};
    use saddle_policy::{Engine as PolicyEngine, Request};
    use serde_json::Value;
    use tempfile::TempDir;
    use wasmtime_wasi::p2::bindings::cli::environment::Host as EnvironmentHost;
    use wasmtime_wasi::p2::bindings::filesystem::preopens::Host as PreopensHost;
    use wasmtime_wasi::p2::bindings::random::random::Host as RandomHost;
    use wasmtime_wasi::random::WasiRandomView;

    const READER: &str = include_str!("fixtures/read-tool.wat");
    const GRANTS: &str = "[agent]\nfs-read=[{root='workspace',exclude=['.env*']}]\n[plugin.reader]\ntool={}\nfs-read=[{root='workspace',exclude=['private']}]\n";

    struct ImageHost(Arc<AtomicUsize>);
    impl HostServices for ImageHost {
        fn read_file(
            &mut self,
            _: &Invocation,
            _: Path,
        ) -> anyhow::Result<Result<Vec<u8>, Failure>> {
            Ok(Err(Failure::Denied("unused".into())))
        }
        fn emit_image(
            &mut self,
            _: &Invocation,
            _: String,
            _: Vec<u8>,
        ) -> anyhow::Result<Result<ImageInfo, String>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(Ok(ImageInfo {
                media_type: "image/png".into(),
                width: 1,
                height: 1,
            }))
        }
    }

    #[test]
    fn image_adapter_limits_bytes_names_and_count_before_calling_the_host() {
        let (_dir, agent, plugin) = grants(GRANTS);
        let prepared = Loader::new()
            .unwrap()
            .prepare::<ImageHost>(
                &manifest(),
                &agent,
                &plugin,
                &wat::parse_str(READER).unwrap(),
                Limits::default(),
            )
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut store = prepared
            .store(ImageHost(calls.clone()), cancelled())
            .unwrap();
        assert!(
            ResultHost::emit_image(store.data_mut(), "x".into(), vec![0; MAX_IMAGE_BYTES + 1])
                .unwrap()
                .is_err()
        );
        assert!(
            ResultHost::emit_image(store.data_mut(), "x".repeat(257), Vec::new())
                .unwrap()
                .is_err()
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(
            ResultHost::emit_image(store.data_mut(), "x".into(), Vec::new())
                .unwrap()
                .is_ok()
        );
        assert!(
            ResultHost::emit_image(store.data_mut(), "x".into(), Vec::new())
                .unwrap()
                .is_err()
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    struct NoHost;
    impl HostServices for NoHost {
        fn read_file(
            &mut self,
            _: &Invocation,
            _: Path,
        ) -> anyhow::Result<Result<Vec<u8>, Failure>> {
            panic!("guest must not reach a host operation");
        }
    }

    fn manifest() -> Manifest {
        Manifest::parse(
            "id='reader'\nversion='0.1.0'\ncomponent='reader.wasm'\nscopes=['tool','fs-read']\n",
        )
        .unwrap()
    }

    fn grants(text: &str) -> (TempDir, Grants, Grants) {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), dir.path().into())], &[]).unwrap();
        let policy = Policy::parse(&format!("version=1\n{text}"), &roots).unwrap();
        let agent = policy
            .grants(&Principal::Agent)
            .cloned()
            .unwrap_or_default();
        let plugin = policy
            .grants(&Principal::Plugin(PluginId::new("reader").unwrap()))
            .cloned()
            .unwrap();
        (dir, agent, plugin)
    }

    fn cancelled() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn refusal<H: HostServices>(result: Result<Prepared<H>, Error>) -> String {
        match result {
            Ok(_) => panic!("component was accepted"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn compiled_hostile_component_is_refused_before_initialization() {
        let (_dir, agent, plugin) =
            grants("[agent]\nfs-read=[{root='workspace'}]\n[plugin.reader]\ntool={}\n");
        let bytes = wat::parse_str(READER.replace(
            ";; initialization-probe",
            "(func $start unreachable) (start $start)",
        ))
        .unwrap();
        let loader = Loader::new().unwrap();
        let why = refusal(loader.prepare::<NoHost>(
            &manifest(),
            &agent,
            &plugin,
            &bytes,
            Limits::default(),
        ));
        assert!(
            why.contains("fs-read") && why.contains("not granted"),
            "{why}"
        );
        assert!(!why.contains("unreachable"), "initialization ran: {why}");
    }

    #[test]
    fn manifest_and_agent_ceilings_are_checked_before_guest_code() {
        let (_dir, agent, plugin) = grants(GRANTS);
        let loader = Loader::new().unwrap();
        let mut ceiling = manifest();
        ceiling.scopes.remove(&Scope::FsRead);
        let why = refusal(loader.prepare::<NoHost>(
            &ceiling,
            &agent,
            &plugin,
            b"not even wasm",
            Limits::default(),
        ));
        assert!(why.contains("manifest ceiling"), "{why}");
        let why = refusal(loader.prepare::<NoHost>(
            &manifest(),
            &Grants::default(),
            &plugin,
            b"not even wasm",
            Limits::default(),
        ));
        assert!(why.contains("agent does not hold"), "{why}");
    }

    #[test]
    fn an_operational_grant_does_not_authorize_tool_registration() {
        let (_dir, agent, plugin) = grants(
            "[agent]\nfs-read=[{root='workspace'}]\n[plugin.reader]\nfs-read=[{root='workspace'}]\n",
        );
        let bytes = wat::parse_str(READER.replace(
            ";; initialization-probe",
            "(func $start unreachable) (start $start)",
        ))
        .unwrap();
        let loader = Loader::new().unwrap();
        let why = refusal(loader.prepare::<NoHost>(
            &manifest(),
            &agent,
            &plugin,
            &bytes,
            Limits::default(),
        ));
        assert!(why.contains("tool") && why.contains("not granted"), "{why}");
    }

    #[test]
    fn unknown_interfaces_versions_and_non_interface_shapes_fail_closed() {
        let (_dir, agent, plugin) = grants(GRANTS);
        let loader = Loader::new().unwrap();
        for name in [
            "wasi:sockets/tcp@0.2.3",
            "acme:evil/backdoor@1.0.0",
            "saddle:host/fs-read@0.2.0",
            "saddle:host/fs-read@0.1.1",
            "saddle:host/fs-read",
        ] {
            let wat = READER.replacen("saddle:host/fs-read@0.1.0", name, 1);
            let bytes = wat::parse_str(wat).unwrap();
            let why = refusal(loader.prepare::<NoHost>(
                &manifest(),
                &agent,
                &plugin,
                &bytes,
                Limits::default(),
            ));
            assert!(why.contains(name), "{why}");
        }
        for source in [
            "(component (import \"saddle:host/fs-read@0.1.0\" (func)))",
            "(component (core module $m) (export \"saddle:plugin/tool@0.1.0\" (core module $m)))",
        ] {
            let why = refusal(loader.prepare::<NoHost>(
                &manifest(),
                &agent,
                &plugin,
                &wat::parse_str(source).unwrap(),
                Limits::default(),
            ));
            assert!(why.contains("not an interface instance"), "{why}");
        }
    }

    #[test]
    fn type_mismatches_are_refused_before_initialization() {
        let (_dir, agent, plugin) = grants(GRANTS);
        let loader = Loader::new().unwrap();
        let source = READER
            .replace("(field \"root\" string)", "(field \"wrong\" string)")
            .replace(
                ";; initialization-probe",
                "(func $start unreachable) (start $start)",
            );
        let why = refusal(loader.prepare::<NoHost>(
            &manifest(),
            &agent,
            &plugin,
            &wat::parse_str(source).unwrap(),
            Limits::default(),
        ));
        assert!(why.contains("pre-linking"), "{why}");
        let source = READER
            .replace("(field \"description\" string)", "(field \"wrong\" string)")
            .replace(
                ";; initialization-probe",
                "(func $start unreachable) (start $start)",
            );
        let why = refusal(loader.prepare::<NoHost>(
            &manifest(),
            &agent,
            &plugin,
            &wat::parse_str(source).unwrap(),
            Limits::default(),
        ));
        assert!(why.contains("tool export types"), "{why}");
    }

    #[test]
    fn fuel_memory_and_initialization_are_bounded() {
        let (_dir, agent, plugin) = grants(GRANTS);
        let loader = Loader::new().unwrap();
        for source in [
            READER.replace(";; invoke-probe", "(loop $spin br $spin)"),
            READER.replace(
                ";; initialization-probe",
                "(func $start (loop $spin br $spin)) (start $start)",
            ),
            READER.replace(";; describe-probe", "(loop $spin br $spin)"),
            READER.replace(";; invoke-probe", "i32.const 1024 memory.grow drop"),
            READER.replace(";; initialization-probe", "(table 20000 funcref)"),
        ] {
            let bytes = wat::parse_str(&source).unwrap();
            let prepared = loader
                .prepare::<NoHost>(
                    &manifest(),
                    &agent,
                    &plugin,
                    &bytes,
                    Limits {
                        fuel: 1000,
                        ..Limits::default()
                    },
                )
                .unwrap();
            let result = if source.contains(";; describe-probe") {
                prepared.invoke(NoHost, "file", cancelled()).map(|_| ())
            } else {
                prepared.describe(NoHost, cancelled()).map(|_| ())
            };
            assert!(result.is_err(), "unbounded guest succeeded");
        }
    }

    #[test]
    fn epoch_deadline_and_in_flight_cancellation_interrupt_guest_code() {
        let (_dir, agent, plugin) = grants(GRANTS);
        let bytes =
            wat::parse_str(READER.replace(";; invoke-probe", "(loop $spin br $spin)")).unwrap();
        let loader = Loader::new().unwrap();
        let prepared = loader
            .prepare::<NoHost>(
                &manifest(),
                &agent,
                &plugin,
                &bytes,
                Limits {
                    fuel: u64::MAX,
                    timeout: Duration::from_millis(40),
                    ..Limits::default()
                },
            )
            .unwrap();
        // A prepared plugin must keep its epoch clock alive even after the loader is dropped.
        drop(loader);
        let start = Instant::now();
        let why = prepared
            .invoke(NoHost, "", cancelled())
            .unwrap_err()
            .to_string();
        assert!(why.contains("timed out"), "{why}");
        assert!(start.elapsed() < Duration::from_secs(1));

        let loader = Loader::new().unwrap();
        let prepared = loader
            .prepare::<NoHost>(
                &manifest(),
                &agent,
                &plugin,
                &bytes,
                Limits {
                    fuel: u64::MAX,
                    timeout: Duration::from_secs(30),
                    ..Limits::default()
                },
            )
            .unwrap();
        let flag = cancelled();
        let signal = flag.clone();
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            signal.store(true, Ordering::Relaxed);
        });
        let start = Instant::now();
        assert!(
            prepared
                .invoke(NoHost, "", flag)
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        worker.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn arguments_results_and_pre_cancelled_calls_are_bounded() {
        let (_dir, agent, plugin) = grants(GRANTS);
        let loader = Loader::new().unwrap();
        let bytes = wat::parse_str(READER).unwrap();
        let prepared = loader
            .prepare::<NoHost>(
                &manifest(),
                &agent,
                &plugin,
                &bytes,
                Limits {
                    input_bytes: 1,
                    output_bytes: 1,
                    ..Limits::default()
                },
            )
            .unwrap();
        assert!(matches!(
            prepared.invoke(NoHost, "large", cancelled()),
            Err(Error::Size(1))
        ));
        assert!(matches!(
            prepared.describe(NoHost, cancelled()),
            Err(Error::Size(1))
        ));
        assert!(matches!(
            prepared.invoke(NoHost, "", Arc::new(AtomicBool::new(true))),
            Err(Error::Cancelled)
        ));
        assert_eq!(prepared.hash(), &<[u8; 32]>::from(Sha256::digest(&bytes)));
    }

    #[test]
    fn ambient_wasi_has_no_preopens_arguments_environment_or_unbounded_resources() {
        let (_dir, agent, plugin) = grants(GRANTS);
        let loader = Loader::new().unwrap();
        let bytes = wat::parse_str(READER).unwrap();
        let prepared = loader
            .prepare::<NoHost>(&manifest(), &agent, &plugin, &bytes, Limits::default())
            .unwrap();
        let mut store = prepared.store(NoHost, cancelled()).unwrap();
        let state = store.data_mut();
        let mut context = state.ctx();
        assert!(
            EnvironmentHost::get_arguments(&mut context)
                .unwrap()
                .is_empty()
        );
        assert!(
            EnvironmentHost::get_environment(&mut context)
                .unwrap()
                .is_empty()
        );
        assert!(
            EnvironmentHost::initial_cwd(&mut context)
                .unwrap()
                .is_none()
        );
        assert!(
            PreopensHost::get_directories(&mut context)
                .unwrap()
                .is_empty()
        );
        assert_eq!(state.resources.max_capacity(), 128);
        assert!(RandomHost::get_random_bytes(state.random(), u64::MAX).is_err());
        for _ in 0..128 {
            state.resources.push(()).unwrap();
        }
        assert!(state.resources.push(()).is_err());
    }

    #[test]
    fn a_blocking_poll_or_http_adapter_is_not_linked_in_the_initial_runtime() {
        let (_dir, agent, plugin) = grants(GRANTS);
        let loader = Loader::new().unwrap();
        for name in ["wasi:io/poll@0.2.3", "wasi:http/outgoing-handler@0.2.3"] {
            let source = READER
                .replacen(
                    "(component",
                    &format!("(component (import \"{name}\" (instance))"),
                    1,
                )
                .replace(
                    ";; initialization-probe",
                    "(func $start unreachable) (start $start)",
                );
            let why = refusal(loader.prepare::<NoHost>(
                &manifest(),
                &agent,
                &plugin,
                &wat::parse_str(source).unwrap(),
                Limits::default(),
            ));
            assert!(why.contains(name), "{why}");
            assert!(!why.contains("unreachable"), "initialization ran: {why}");
        }
    }

    struct FailedHost;
    impl HostServices for FailedHost {
        fn read_file(
            &mut self,
            _: &Invocation,
            _: Path,
        ) -> anyhow::Result<Result<Vec<u8>, Failure>> {
            Err(anyhow!("audit failed"))
        }
    }

    struct OversizedHost;
    impl HostServices for OversizedHost {
        fn read_file(
            &mut self,
            _: &Invocation,
            _: Path,
        ) -> anyhow::Result<Result<Vec<u8>, Failure>> {
            Ok(Ok(vec![b'x'; MAX_FILE_BYTES + 1]))
        }
    }

    #[test]
    fn host_failures_trap_and_oversized_reads_are_typed_failures() {
        let (_dir, agent, plugin) = grants(GRANTS);
        let loader = Loader::new().unwrap();
        let bytes = wat::parse_str(READER).unwrap();
        let prepared = loader
            .prepare::<FailedHost>(&manifest(), &agent, &plugin, &bytes, Limits::default())
            .unwrap();
        assert!(
            prepared
                .invoke(FailedHost, "file", cancelled())
                .unwrap_err()
                .to_string()
                .contains("audit failed")
        );
        let prepared = loader
            .prepare::<OversizedHost>(&manifest(), &agent, &plugin, &bytes, Limits::default())
            .unwrap();
        assert!(
            prepared
                .invoke(OversizedHost, "file", cancelled())
                .unwrap()
                .is_error
        );
    }

    struct Access {
        engine: PolicyEngine,
        audit: AuditLog,
        root: Dir,
        calls: Vec<u64>,
    }
    #[derive(Clone)]
    struct Files(Arc<Mutex<Access>>);
    impl HostServices for Files {
        fn read_file(
            &mut self,
            call: &Invocation,
            path: Path,
        ) -> anyhow::Result<Result<Vec<u8>, Failure>> {
            let mut access = self.0.lock().unwrap();
            access.calls.push(call.id());
            let root = RootName::new(&path.root)?;
            let rel = RelPath::new(&path.relative)?;
            let loc = Location::new(root, rel);
            let req = Request::FsRead { loc: &loc };
            let decision = access.engine.decide(call.principal(), &req);
            access.audit.decision(call.principal(), &req, &decision)?;
            if !decision.allow {
                return Ok(Err(Failure::Denied(decision.explanation)));
            }
            let mut bytes = Vec::new();
            access
                .root
                .open(&path.relative)?
                .take((MAX_FILE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_FILE_BYTES {
                return Ok(Err(Failure::TooLarge));
            }
            Ok(Ok(bytes))
        }
    }

    fn files(dir: &TempDir, bounded: &Grants) -> Files {
        let who = Principal::Plugin(manifest().id);
        let policy = Policy {
            principals: [(who, bounded.clone())].into_iter().collect(),
        };
        Files(Arc::new(Mutex::new(Access {
            engine: PolicyEngine::new(policy),
            audit: AuditLog::create(&dir.path().join("audit"), "plugin-test").unwrap(),
            root: Dir::open_ambient_dir(dir.path(), cap_std::ambient_authority()).unwrap(),
            calls: Vec::new(),
        })))
    }

    #[test]
    #[ignore = "build and componentize the SDK example first; see docs/plugins.md"]
    fn sdk_reader_component_round_trips_through_the_runtime() {
        let (dir, agent, plugin) = grants(GRANTS);
        fs::write(dir.path().join("file"), "SDK round trip").unwrap();
        fs::write(dir.path().join(".env"), "secret").unwrap();
        let component =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/reader.component.wasm");
        let bytes = fs::read(component)
            .expect("build and componentize the SDK example first; see docs/plugins.md");
        let loader = Loader::new().unwrap();
        let prepared = loader
            .prepare::<Files>(&manifest(), &agent, &plugin, &bytes, Limits::default())
            .unwrap();
        let host = files(&dir, prepared.grants());
        assert_eq!(
            prepared.describe(host.clone(), cancelled()).unwrap().name,
            "read-file"
        );
        let out = prepared
            .invoke(
                host.clone(),
                r#"{"root":"workspace","relative":"file"}"#,
                cancelled(),
            )
            .unwrap();
        assert_eq!(out.text, "SDK round trip");
        assert!(!out.is_error);
        let out = prepared
            .invoke(
                host.clone(),
                r#"{"root":"workspace","relative":".env"}"#,
                cancelled(),
            )
            .unwrap();
        assert!(out.is_error);
        let access = host.0.lock().unwrap();
        audit::verify(access.audit.path()).unwrap();
    }

    #[test]
    fn allowed_reads_exclusions_attribution_and_fresh_instances_use_the_same_gate() {
        let (dir, agent, plugin) = grants(GRANTS);
        fs::write(dir.path().join("file"), "hello").unwrap();
        fs::write(dir.path().join(".env"), "secret").unwrap();
        fs::write(dir.path().join("private"), "secret").unwrap();
        let loader = Loader::new().unwrap();
        let bytes = wat::parse_str(READER).unwrap();
        let prepared = loader
            .prepare::<Files>(&manifest(), &agent, &plugin, &bytes, Limits::default())
            .unwrap();
        let host = files(&dir, prepared.grants());
        let definition = prepared.describe(host.clone(), cancelled()).unwrap();
        assert_eq!(definition.name, "reader");
        for path in ["file", "file", ".env", "private"] {
            let out = prepared.invoke(host.clone(), path, cancelled()).unwrap();
            if path == "file" {
                assert_eq!(out.text, "hello");
                assert!(!out.is_error);
            } else {
                assert!(out.is_error);
            }
        }
        let access = host.0.lock().unwrap();
        assert!(access.calls.windows(2).all(|ids| ids[0] < ids[1]));
        let path = access.audit.path();
        audit::verify(path).unwrap();
        let rows: Vec<Value> = fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 4);
        for row in &rows {
            assert_eq!(row["principal"], "plugin.reader");
        }
        assert_eq!(rows[0]["rule"], "plugin.reader.fs-read[0]");
        assert_eq!(
            rows.iter()
                .map(|r| r["allow"].as_bool().unwrap())
                .collect::<Vec<_>>(),
            [true, true, false, false]
        );
    }
}
