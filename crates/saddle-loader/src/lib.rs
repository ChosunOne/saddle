//! bounded, grant-shaped component loading (D11, D13, D126, D127, D150).

mod ambient;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use saddle_cap::component::{Direction, HostInterface, Interface, PluginExport, check_component};
use saddle_cap::delegate::{self, DelegationError};
use saddle_cap::manifest::{Manifest, above_ceiling};
use saddle_cap::policy::valid_launch_profile_name;
use saddle_cap::{Grants, Principal, Scope};
use saddle_limits::{self as budgets, Budgets};
use saddle_wit::ReadToolPre;
use saddle_wit::events::exports::saddle::plugin::plugin_events::{
    Event as PluginEvent, Subscription,
};
use saddle_wit::exports::saddle::plugin::tool::{Definition, Output};
use saddle_wit::harness::exports::saddle::plugin::observer::{
    Event as HarnessEvent, Kind as HarnessKind,
};
use saddle_wit::saddle::host::budgets::{self as budgets_interface, Host as BudgetsHost};
use saddle_wit::saddle::host::context_contribute::{self, Contribution, Host as ContextHost};
use saddle_wit::saddle::host::exec::{self, Command, Host as ExecHost, Reply as ExecReply};
use saddle_wit::saddle::host::exec_tasks::{self, Host as TasksHost, Task};
use saddle_wit::saddle::host::fs_list::EntryKind;
use saddle_wit::saddle::host::fs_list::{self, Host as ListHost, Page as DirectoryPage};
use saddle_wit::saddle::host::fs_paths::{self, Host as PathsHost};
use saddle_wit::saddle::host::fs_preimage::{self, Host as PreimageHost};
use saddle_wit::saddle::host::fs_read::{self, Failure, Host, Path};
use saddle_wit::saddle::host::fs_read_range::{self, Chunk, Host as RangeHost};
use saddle_wit::saddle::host::fs_write::{self, Change, Expectation, Host as WriteHost};
use saddle_wit::saddle::host::http::{
    self, Host as HttpHost, Request as HttpRequest, Response as HttpResponse,
};
use saddle_wit::saddle::host::model_invoke::{
    self, Host as ModelHost, Reply as ModelReply, Request as ModelRequest,
};
use saddle_wit::saddle::host::persist::{self, Change as PersistChange, Host as PersistHost};
use saddle_wit::saddle::host::plugin_events::{self, Host as EventsHost};
use saddle_wit::saddle::host::run_binary::{
    self, Host as LaunchHost, Launch, Reply as LaunchReply,
};
use saddle_wit::saddle::host::tool_paths::{
    self, Chunk as ToolChunk, Entry as ToolEntry, EntryKind as ToolEntryKind,
    Failure as ToolFailure, Host as ToolPathsHost, Page as ToolPage, Path as ToolPath,
};
use saddle_wit::saddle::host::tool_result::{self, Host as ResultHost, ImageInfo};
use saddle_wit::saddle::host::transcript_read::{self, Host as TranscriptHost};
use saddle_wit::saddle::host::ui::{self, Host as UiHost};
use saddle_wit::saddle::host::web_search::{self, Host as SearchHost, Snapshot as SearchSnapshot};
use saddle_wit::saddle::widgets::types::{self as widget_types, Panel};
use saddle_wit::veto::exports::saddle::plugin::veto::{Decision as VetoDecision, PendingToolCall};
use sha2::{Digest, Sha256};
use wasmtime::component::types::{Component as ComponentType, ComponentItem};
use wasmtime::component::{
    Component, ComponentNamedList, HasSelf, InstancePre, Lift, Linker, Lower, ResourceTable,
};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

/// native I/O must enforce this cap before allocating or reading the entire source.
pub const MAX_FILE_BYTES: usize = budgets::DEFAULTS.range_bytes;
pub const MAX_MUTATION_BYTES: usize = budgets::DEFAULTS.mutation_bytes;
pub const MAX_MUTATIONS: usize = budgets::DEFAULTS.mutation_changes;
pub const MAX_DIRECTORY_ENTRIES: usize = budgets::DEFAULTS.directory_page_entries;
pub const MAX_IMAGE_BYTES: usize = budgets::DEFAULTS.image_bytes;
pub const MAX_QUERY_BYTES: usize = budgets::DEFAULTS.query_bytes;
pub const MAX_QUERY_RESULT_BYTES: usize = budgets::DEFAULTS.query_result_bytes;
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
    pub budgets: Budgets,
    pub memory_bytes: usize,
    pub fuel: u64,
    pub timeout: Duration,
    pub input_bytes: usize,
    pub output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self::from_budgets(Budgets::default())
    }
}

impl Limits {
    /// construct an envelope and its SDK-visible profile from one configuration.
    pub fn from_budgets(budgets: Budgets) -> Self {
        Self {
            memory_bytes: budgets.plugin_memory_bytes,
            fuel: budgets.plugin_fuel,
            timeout: Duration::from_millis(budgets.plugin_timeout_ms as u64),
            input_bytes: budgets.plugin_input_bytes,
            output_bytes: budgets.plugin_output_bytes,
            budgets,
        }
    }
}

/// identity and effective reach assigned by the host, never by guest arguments.
pub struct Invocation {
    principal: Principal,
    budgets: Budgets,
    id: u64,
    grants: Arc<Grants>,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
    hard_deadline: Option<Instant>,
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

/// a typed task effect, independent of shell workflow or UI presentation (D153).
pub enum TaskOperation {
    Start(Command),
    Inspect(String),
    List,
    Cancel(String),
    /// Whether finishing this task should resume the agent (D174).
    Wake(String, bool),
}

/// typed effects, never an arbitrary host command or JSON protocol (D156).
pub enum Service {
    Contribute(Contribution),
    PersistGet(String),
    PersistApply(Vec<PersistChange>),
    Model(ModelRequest),
    PublishPanel(Panel),
    ClearPanel,
}
pub enum ServiceReply {
    Contribution(String),
    Value(Option<Vec<u8>>),
    Applied(bool),
    Model(ModelReply),
    Unit,
}

/// the binary supplies policy/audit/file operations; the loader never depends on its session.
///
/// Implementations must decide and audit using `call.principal()` and `call.grants()`, validate
/// root-relative paths and symlinks, and bound/cancel I/O independently of Wasm's epoch deadline.
/// `Err` is a host failure (including audit failure), not a guest-visible policy denial.
/// filesystem callbacks may hold any filesystem capability; determine authority per operation/path,
/// never from the existence of a linked interface (D139, D140).
pub trait HostServices: Send + 'static {
    fn service(
        &mut self,
        _call: &Invocation,
        _service: Service,
    ) -> anyhow::Result<Result<ServiceReply, String>> {
        Ok(Err("host does not implement extension services".into()))
    }

    /// enqueue a custom event; the host attaches the publishing principal (D155).
    fn emit_event(
        &mut self,
        _call: &Invocation,
        _name: String,
        _data: String,
    ) -> anyhow::Result<Result<(), String>> {
        Ok(Err("host does not implement plugin events".into()))
    }

    fn read_preimage(
        &mut self,
        _call: &Invocation,
        _path: Path,
    ) -> anyhow::Result<Result<Option<Vec<u8>>, Failure>> {
        Ok(Err(Failure::Denied(
            "host does not implement optional preimages".into(),
        )))
    }

    fn label_root(
        &mut self,
        _call: &Invocation,
        _root: String,
    ) -> anyhow::Result<Result<String, Failure>> {
        Ok(Err(Failure::Denied(
            "host does not implement root labels".into(),
        )))
    }
    fn http_get(
        &mut self,
        _call: &Invocation,
        _request: HttpRequest,
    ) -> anyhow::Result<Result<HttpResponse, String>> {
        Ok(Err("host does not implement HTTP".into()))
    }
    fn search_web(
        &mut self,
        _call: &Invocation,
        _query: String,
        _count: u32,
    ) -> anyhow::Result<Result<SearchSnapshot, String>> {
        Ok(Err("host does not implement web search".into()))
    }
    fn last_search(
        &mut self,
        _call: &Invocation,
    ) -> anyhow::Result<Result<Option<SearchSnapshot>, String>> {
        Ok(Err("host does not implement search snapshots".into()))
    }
    fn execute(
        &mut self,
        _call: &Invocation,
        _request: Command,
    ) -> anyhow::Result<Result<ExecReply, String>> {
        Ok(Err("host does not implement execution".into()))
    }
    /// run the exact named profile through the session policy gate (D184).
    fn run_binary(
        &mut self,
        _call: &Invocation,
        _request: Launch,
        _allowance: Duration,
    ) -> anyhow::Result<Result<LaunchReply, String>> {
        Ok(Err("host does not implement named launches".into()))
    }
    fn task(
        &mut self,
        _call: &Invocation,
        _operation: TaskOperation,
    ) -> anyhow::Result<Result<Vec<Task>, String>> {
        Ok(Err("host does not implement background execution".into()))
    }
    fn emit_diff(
        &mut self,
        _call: &Invocation,
        _text: String,
    ) -> anyhow::Result<Result<(), String>> {
        Ok(Err("host does not implement diff presentation".into()))
    }
    fn apply_changes(
        &mut self,
        _call: &Invocation,
        _changes: Vec<Change>,
    ) -> anyhow::Result<Result<(), String>> {
        Ok(Err("host does not implement filesystem mutations".into()))
    }
    fn resolve_tool_path(
        &mut self,
        _call: &Invocation,
        _value: String,
    ) -> anyhow::Result<Result<Path, Failure>> {
        Ok(Err(Failure::Denied(
            "host does not implement tool paths".into(),
        )))
    }
    fn read_tool_range(
        &mut self,
        _call: &Invocation,
        _path: Path,
        _offset: u64,
        _limit: u32,
    ) -> anyhow::Result<Result<Chunk, Failure>> {
        Ok(Err(Failure::Denied(
            "host does not implement tool paths".into(),
        )))
    }
    fn read_tool_directory(
        &mut self,
        _call: &Invocation,
        _path: Path,
        _after: String,
        _limit: u32,
    ) -> anyhow::Result<Result<DirectoryPage, Failure>> {
        Ok(Err(Failure::Denied(
            "host does not implement tool paths".into(),
        )))
    }
    fn query_transcript(
        &mut self,
        _call: &Invocation,
        _sql: String,
        _session: String,
    ) -> anyhow::Result<Result<String, String>> {
        Ok(Err("host does not implement transcript queries".into()))
    }
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
    emitted_image_bytes: usize,
    host_time_left: Duration,
    host_calls: usize,
}

impl<H: HostServices> State<H> {
    fn host_wait<T>(
        &mut self,
        allowance: Duration,
        operation: impl FnOnce(&mut H, &Invocation, Duration) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        if self.call.cancelled()
            || Instant::now() >= self.call.deadline
            || self.host_calls >= budgets::host_calls()
        {
            return Err(anyhow!(
                "plugin call cancelled, timed out or exceeds configured host call limit"
            ));
        }
        let allowance = allowance.min(self.host_time_left).min(
            self.call.hard_deadline.map_or(allowance, |end| {
                end.saturating_duration_since(Instant::now())
            }),
        );
        if allowance.is_zero() {
            return Err(anyhow!("plugin host wait budget exhausted"));
        }
        self.host_calls += 1;
        let old = self.call.deadline;
        self.call.deadline = old
            .checked_add(allowance)
            .ok_or_else(|| anyhow!("plugin deadline overflow"))?;
        if let Some(end) = self.call.hard_deadline {
            self.call.deadline = self.call.deadline.min(end);
        }
        let start = Instant::now();
        let result = operation(&mut self.host, &self.call, allowance);
        let elapsed = start.elapsed().min(allowance);
        self.host_time_left = self.host_time_left.saturating_sub(elapsed);
        self.call.deadline = old
            .checked_add(elapsed)
            .ok_or_else(|| anyhow!("plugin deadline overflow"))?;
        if let Some(end) = self.call.hard_deadline {
            self.call.deadline = self.call.deadline.min(end);
        }
        result
    }
}

impl<H: HostServices> State<H> {
    fn service(&mut self, service: Service) -> anyhow::Result<Result<ServiceReply, String>> {
        self.host_wait(
            Duration::from_secs(budgets::host_wait_seconds() as u64),
            |host, call, _| host.service(call, service),
        )
    }
}
impl<H: HostServices> ContextHost for State<H> {
    fn append(&mut self, value: Contribution) -> anyhow::Result<Result<String, String>> {
        match self.service(Service::Contribute(value))? {
            Ok(ServiceReply::Contribution(id)) => Ok(Ok(id)),
            Err(why) => Ok(Err(why)),
            _ => Err(anyhow!("wrong typed extension reply")),
        }
    }
}
impl<H: HostServices> PersistHost for State<H> {
    fn get(&mut self, key: String) -> anyhow::Result<Result<Option<Vec<u8>>, String>> {
        match self.service(Service::PersistGet(key))? {
            Ok(ServiceReply::Value(value)) => Ok(Ok(value)),
            Err(why) => Ok(Err(why)),
            _ => Err(anyhow!("wrong typed extension reply")),
        }
    }
    fn apply(&mut self, changes: Vec<PersistChange>) -> anyhow::Result<Result<bool, String>> {
        match self.service(Service::PersistApply(changes))? {
            Ok(ServiceReply::Applied(value)) => Ok(Ok(value)),
            Err(why) => Ok(Err(why)),
            _ => Err(anyhow!("wrong typed extension reply")),
        }
    }
}
impl<H: HostServices> ModelHost for State<H> {
    fn invoke(&mut self, request: ModelRequest) -> anyhow::Result<Result<ModelReply, String>> {
        match self.service(Service::Model(request))? {
            Ok(ServiceReply::Model(value)) => Ok(Ok(value)),
            Err(why) => Ok(Err(why)),
            _ => Err(anyhow!("wrong typed extension reply")),
        }
    }
}
impl<H: HostServices> UiHost for State<H> {
    fn publish(&mut self, panel: Panel) -> anyhow::Result<Result<(), String>> {
        match self.service(Service::PublishPanel(panel))? {
            Ok(ServiceReply::Unit) => Ok(Ok(())),
            Err(why) => Ok(Err(why)),
            _ => Err(anyhow!("wrong typed extension reply")),
        }
    }
    fn clear(&mut self) -> anyhow::Result<Result<(), String>> {
        match self.service(Service::ClearPanel)? {
            Ok(ServiceReply::Unit) => Ok(Ok(())),
            Err(why) => Ok(Err(why)),
            _ => Err(anyhow!("wrong typed extension reply")),
        }
    }
}
impl<H: HostServices> widget_types::Host for State<H> {}

impl<H: HostServices> EventsHost for State<H> {
    fn emit(&mut self, name: String, data: String) -> anyhow::Result<Result<(), String>> {
        self.host_wait(
            Duration::from_secs(budgets::host_wait_seconds() as u64),
            |host, call, _| host.emit_event(call, name, data),
        )
    }
}

impl<H: HostServices> BudgetsHost for State<H> {
    fn read(&mut self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(&self.call.budgets)?)
    }
}

impl<H: HostServices> ExecHost for State<H> {
    fn run(&mut self, mut request: Command) -> anyhow::Result<Result<ExecReply, String>> {
        if request.argv.is_empty()
            || request.argv.len() > budgets::exec_arguments()
            || request.argv.iter().map(String::len).sum::<usize>() > budgets::exec_argument_bytes()
            || request.argv.iter().any(|s| s.contains('\0'))
        {
            return Ok(Err("invalid command or exceeds argument bounds".into()));
        }
        let granted = self
            .call
            .grants
            .exec
            .as_ref()
            .map_or(Duration::ZERO, |grant| grant.timeout);
        let timeout = Duration::from_secs(
            request
                .timeout_secs
                .clamp(1, budgets::exec_seconds() as u64),
        )
        .min(granted);
        let result = self.host_wait(timeout, |host, call, allowance| {
            request.timeout_secs = allowance.as_secs().max(1);
            host.execute(call, request)
        })?;
        Ok(bounded_exec_reply(result))
    }
}

impl<H: HostServices> LaunchHost for State<H> {
    fn run(&mut self, request: Launch) -> anyhow::Result<Result<LaunchReply, String>> {
        if !valid_launch_profile_name(&request.profile)
            || request.timeout_secs == 0
            || request.argv.len() >= budgets::exec_arguments()
            || request.argv.iter().map(String::len).sum::<usize>() > budgets::exec_argument_bytes()
            || request.argv.iter().any(|s| s.contains('\0'))
        {
            return Ok(Err("invalid named launch or exceeds argument bounds".into()));
        }
        let allowance =
            Duration::from_secs(request.timeout_secs.min(budgets::exec_seconds() as u64));
        let result = self.host_wait(allowance, |host, call, left| {
            host.run_binary(call, request, left)
        })?;
        if matches!(&result, Ok(reply) if !valid_exec_output(&reply.stdout, &reply.stderr, &reply.denials))
        {
            return Ok(Err("execution reply exceeds bounds".into()));
        }
        if matches!(&result, Err(why) if why.len() > budgets::metadata_bytes()) {
            return Ok(Err("host error exceeds configured byte limit".into()));
        }
        Ok(result)
    }
}

fn valid_exec_output(stdout: &[u8], stderr: &[u8], denials: &[String]) -> bool {
    stdout.len() <= budgets::exec_capture_bytes()
        && stderr.len() <= budgets::exec_capture_bytes()
        && denials.iter().map(String::len).sum::<usize>() <= budgets::metadata_bytes()
}

fn bounded_exec_reply(result: Result<ExecReply, String>) -> Result<ExecReply, String> {
    if matches!(&result, Ok(reply) if !valid_exec_output(&reply.stdout, &reply.stderr, &reply.denials))
    {
        return Err("execution reply exceeds bounds".into());
    }
    if matches!(&result, Err(why) if why.len() > budgets::metadata_bytes()) {
        return Err("host error exceeds configured byte limit".into());
    }
    result
}

impl<H: HostServices> State<H> {
    fn task_operation(
        &mut self,
        operation: TaskOperation,
    ) -> anyhow::Result<Result<Vec<Task>, String>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("plugin task request cancelled or timed out"));
        }
        match &operation {
            TaskOperation::Start(command)
                if command.argv.is_empty()
                    || command.argv.len() > budgets::exec_arguments()
                    || command.argv.iter().map(String::len).sum::<usize>()
                        > budgets::exec_argument_bytes()
                    || command.argv.iter().any(|value| value.contains('\0')) =>
            {
                return Ok(Err("invalid command or exceeds argument bounds".into()));
            }
            TaskOperation::Inspect(id) | TaskOperation::Cancel(id) | TaskOperation::Wake(id, _)
                if id.len() > budgets::metadata_bytes() =>
            {
                return Ok(Err("task id exceeds byte budget".into()));
            }
            _ => {}
        }
        let result = self.host.task(&self.call, operation)?;
        if let Ok(tasks) = &result {
            if tasks.len() > budgets::exec_tasks()
                || tasks.iter().any(|task| {
                    task.id.len() > budgets::metadata_bytes()
                        || task.output.stdout.len() > budgets::exec_capture_bytes()
                        || task.output.stderr.len() > budgets::exec_capture_bytes()
                        || task.output.denials.iter().map(String::len).sum::<usize>()
                            > budgets::metadata_bytes()
                })
            {
                return Ok(Err("task reply exceeds configured bounds".into()));
            }
        } else if matches!(&result, Err(why) if why.len() > budgets::metadata_bytes()) {
            return Ok(Err("task error exceeds configured bounds".into()));
        }
        Ok(result)
    }
    fn one_task(&mut self, operation: TaskOperation) -> anyhow::Result<Result<Task, String>> {
        Ok(self.task_operation(operation)?.and_then(|mut tasks| {
            if tasks.len() == 1 {
                Ok(tasks.remove(0))
            } else {
                Err("task service returned the wrong number of tasks".into())
            }
        }))
    }
}

impl<H: HostServices> TasksHost for State<H> {
    fn start(&mut self, request: Command) -> anyhow::Result<Result<Task, String>> {
        self.one_task(TaskOperation::Start(request))
    }
    fn inspect(&mut self, id: String) -> anyhow::Result<Result<Task, String>> {
        self.one_task(TaskOperation::Inspect(id))
    }
    fn list(&mut self) -> anyhow::Result<Result<Vec<Task>, String>> {
        self.task_operation(TaskOperation::List)
    }
    fn cancel(&mut self, id: String) -> anyhow::Result<Result<Task, String>> {
        self.one_task(TaskOperation::Cancel(id))
    }
    fn set_wake(&mut self, id: String, wake: bool) -> anyhow::Result<Result<Task, String>> {
        self.one_task(TaskOperation::Wake(id, wake))
    }
}

impl<H: HostServices> HttpHost for State<H> {
    fn get(&mut self, request: HttpRequest) -> anyhow::Result<Result<HttpResponse, String>> {
        if request.url.len() > budgets::url_bytes()
            || request.headers.len() > budgets::http_headers()
            || request
                .headers
                .iter()
                .map(|h| h.name.len() + h.value.len())
                .sum::<usize>()
                > budgets::http_header_bytes()
        {
            return Ok(Err("HTTP request exceeds bounds".into()));
        }
        let result = self.host_wait(
            Duration::from_secs(budgets::network_seconds() as u64),
            |host, call, _| host.http_get(call, request),
        )?;
        if matches!(&result, Ok(reply) if reply.body.len() > budgets::http_capture_bytes() || reply.headers.len() > budgets::http_headers() || reply.headers.iter().map(|h|h.name.len()+h.value.len()).sum::<usize>() > budgets::http_header_bytes())
        {
            return Ok(Err("HTTP response exceeds bounds".into()));
        }
        if matches!(&result, Err(why) if why.len() > budgets::metadata_bytes()) {
            return Ok(Err("host error exceeds configured byte limit".into()));
        }
        Ok(result)
    }
}

impl<H: HostServices> SearchHost for State<H> {
    fn search(
        &mut self,
        query: String,
        count: u32,
    ) -> anyhow::Result<Result<SearchSnapshot, String>> {
        if query.len() > budgets::web_query_bytes()
            || !(1..=budgets::web_results() as u32).contains(&count)
        {
            return Ok(Err("invalid search query/count or exceeds 16 KiB".into()));
        }
        let result = self.host_wait(
            Duration::from_secs(budgets::network_seconds() as u64),
            |host, call, _| host.search_web(call, query, count),
        )?;
        if matches!(&result, Ok(snapshot) if snapshot.body.len() > budgets::search_response_bytes() || snapshot.query.len() > budgets::web_query_bytes() || snapshot.provider.len() > budgets::metadata_bytes() || snapshot.via.len() > budgets::metadata_bytes())
        {
            return Ok(Err("search response exceeds bounds".into()));
        }
        if matches!(&result, Err(why) if why.len() > budgets::metadata_bytes()) {
            return Ok(Err("host error exceeds configured byte limit".into()));
        }
        Ok(result)
    }
    fn last(&mut self) -> anyhow::Result<Result<Option<SearchSnapshot>, String>> {
        let result = self.host_wait(Duration::from_secs(1), |host, call, _| {
            host.last_search(call)
        })?;
        if matches!(&result, Ok(Some(snapshot)) if snapshot.body.len() > budgets::search_response_bytes() || snapshot.query.len() > budgets::web_query_bytes() || snapshot.provider.len() > budgets::metadata_bytes() || snapshot.via.len() > budgets::metadata_bytes())
        {
            return Ok(Err("search snapshot exceeds bounds".into()));
        }
        if matches!(&result, Err(why) if why.len() > budgets::metadata_bytes()) {
            return Ok(Err("host error exceeds configured byte limit".into()));
        }
        Ok(result)
    }
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
        if matches!(&result, Ok(bytes) if bytes.len() > budgets::range_bytes()) {
            return Ok(Err(Failure::TooLarge));
        }
        Ok(result)
    }
}

impl<H: HostServices> TranscriptHost for State<H> {
    fn query(&mut self, sql: String, session: String) -> anyhow::Result<Result<String, String>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("plugin call cancelled or timed out"));
        }
        if sql.len() > budgets::query_bytes() {
            return Ok(Err("transcript SQL exceeds 16 KiB".into()));
        }
        if session.len() > budgets::directory_name_bytes() {
            return Ok(Err("transcript session selector is too long".into()));
        }
        let result = self.host.query_transcript(&self.call, sql, session)?;
        if matches!(&result, Ok(value) | Err(value) if value.len() > budgets::query_result_bytes())
        {
            return Ok(Err("transcript result exceeds 50 KiB".into()));
        }
        Ok(result)
    }
}

impl<H: HostServices> WriteHost for State<H> {
    fn apply(&mut self, changes: Vec<Change>) -> anyhow::Result<Result<(), String>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("plugin call cancelled or timed out"));
        }
        if changes.is_empty() || changes.len() > budgets::mutation_changes()
            || changes.iter().any(|change| change.path.len() > budgets::path_bytes() || matches!(&change.expected, Expectation::Sha256(hash) if hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit())))
            || changes.iter().map(|change| change.content.as_ref().map_or(0, Vec::len)).sum::<usize>() > budgets::mutation_bytes() {
            return Ok(Err("invalid mutation batch or exceeds configured path/byte limit".into()));
        }
        let result = self.host.apply_changes(&self.call, changes)?;
        if matches!(&result, Err(why) if why.len() > budgets::metadata_bytes()) {
            return Ok(Err("mutation error exceeds host result limit".into()));
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
        if matches!(&result, Ok(chunk) if chunk.bytes.len() > budgets::range_bytes() || chunk.bytes.len() as u64 > u64::from(limit))
        {
            return Ok(Err(Failure::TooLarge));
        }
        Ok(result)
    }
}

impl<H: HostServices> PreimageHost for State<H> {
    fn read(&mut self, path: Path) -> anyhow::Result<Result<Option<Vec<u8>>, Failure>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("preimage read cancelled or timed out"));
        }
        if path.root.len() > 64 || path.relative.len() > budgets::path_bytes() {
            return Ok(Err(Failure::InvalidPath(
                "preimage path exceeds limits".into(),
            )));
        }
        let result = self.host.read_preimage(&self.call, path)?;
        if matches!(&result, Ok(Some(bytes)) if bytes.len() > budgets::mutation_bytes()) {
            return Ok(Err(Failure::TooLarge));
        }
        Ok(result)
    }
}

impl<H: HostServices> PathsHost for State<H> {
    fn label_root(&mut self, root: String) -> anyhow::Result<Result<String, Failure>> {
        if root.len() > 128 || self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Ok(Err(Failure::Denied(
                "invalid root label request or timed out".into(),
            )));
        }
        let result = self.host.label_root(&self.call, root)?;
        if matches!(&result,Ok(label) if label.len() > budgets::metadata_bytes()) {
            return Ok(Err(Failure::TooLarge));
        }
        Ok(result)
    }
    fn resolve_path(&mut self, value: String) -> anyhow::Result<Result<Path, Failure>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("plugin call cancelled or timed out"));
        }
        let result = self.host.resolve_path(&self.call, value)?;
        if matches!(&result, Ok(path) if path.root.len() > 64 || path.relative.len() > budgets::path_bytes())
        {
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
        if matches!(&result, Ok(page) if page.entries.len() > budgets::directory_page_entries() || page.entries.len() as u64 > u64::from(limit) || page.entries.iter().map(|entry| entry.name.len()).sum::<usize>() > budgets::metadata_bytes())
        {
            return Ok(Err(Failure::TooLarge));
        }
        Ok(result)
    }
}

fn tool_failure(failure: Failure) -> ToolFailure {
    match failure {
        Failure::Denied(why) => ToolFailure::Denied(why),
        Failure::InvalidPath(why) => ToolFailure::InvalidPath(why),
        Failure::Io(why) => ToolFailure::Io(why),
        Failure::TooLarge => ToolFailure::TooLarge,
    }
}

impl<H: HostServices> ToolPathsHost for State<H> {
    fn resolve_path(&mut self, value: String) -> anyhow::Result<Result<ToolPath, ToolFailure>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("plugin call cancelled or timed out"));
        }
        if value.len() > budgets::path_bytes() {
            return Ok(Err(ToolFailure::TooLarge));
        }
        let result = self.host.resolve_tool_path(&self.call, value)?;
        Ok(result
            .and_then(|path| {
                if path.root.len() > 64 || path.relative.len() > budgets::path_bytes() {
                    return Err(Failure::TooLarge);
                }
                Ok(ToolPath {
                    root: path.root,
                    relative: path.relative,
                })
            })
            .map_err(tool_failure))
    }
    fn read_range(
        &mut self,
        path: ToolPath,
        offset: u64,
        limit: u32,
    ) -> anyhow::Result<Result<ToolChunk, ToolFailure>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("plugin call cancelled or timed out"));
        }
        if limit == 0
            || limit as usize > budgets::range_bytes()
            || path.root.len() > 64
            || path.relative.len() > budgets::path_bytes()
        {
            return Ok(Err(ToolFailure::TooLarge));
        }
        let result = self.host.read_tool_range(
            &self.call,
            Path {
                root: path.root,
                relative: path.relative,
            },
            offset,
            limit,
        )?;
        Ok(result
            .and_then(|chunk| {
                if chunk.bytes.len() > limit as usize {
                    return Err(Failure::TooLarge);
                }
                Ok(ToolChunk {
                    bytes: chunk.bytes,
                    eof: chunk.eof,
                })
            })
            .map_err(tool_failure))
    }
    fn read_directory(
        &mut self,
        path: ToolPath,
        after: String,
        limit: u32,
    ) -> anyhow::Result<Result<ToolPage, ToolFailure>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("plugin call cancelled or timed out"));
        }
        if limit == 0
            || limit as usize > budgets::directory_page_entries()
            || after.len() > budgets::directory_name_bytes()
            || path.root.len() > 64
            || path.relative.len() > budgets::path_bytes()
        {
            return Ok(Err(ToolFailure::TooLarge));
        }
        let result = self.host.read_tool_directory(
            &self.call,
            Path {
                root: path.root,
                relative: path.relative,
            },
            after,
            limit,
        )?;
        Ok(result
            .and_then(|page| {
                if page.entries.len() > limit as usize
                    || page
                        .entries
                        .iter()
                        .map(|entry| entry.name.len())
                        .sum::<usize>()
                        > budgets::metadata_bytes()
                {
                    return Err(Failure::TooLarge);
                }
                Ok(ToolPage {
                    entries: page
                        .entries
                        .into_iter()
                        .map(|entry| ToolEntry {
                            name: entry.name,
                            kind: match entry.kind {
                                EntryKind::File => ToolEntryKind::File,
                                EntryKind::Directory => ToolEntryKind::Directory,
                                EntryKind::Symlink => ToolEntryKind::Symlink,
                                EntryKind::Other => ToolEntryKind::Other,
                            },
                        })
                        .collect(),
                    total: page.total,
                    skipped: page.skipped,
                    more: page.more,
                })
            })
            .map_err(tool_failure))
    }
}

impl<H: HostServices> ResultHost for State<H> {
    fn emit_diff(&mut self, text: String) -> anyhow::Result<Result<(), String>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("plugin call cancelled or timed out"));
        }
        if text.len() > budgets::diff_bytes() {
            return Ok(Err("diff exceeds configured byte limit".into()));
        }
        self.host.emit_diff(&self.call, text)
    }
    fn emit_image(
        &mut self,
        name: String,
        bytes: Vec<u8>,
    ) -> anyhow::Result<Result<ImageInfo, String>> {
        if self.call.cancelled() || Instant::now() >= self.call.deadline {
            return Err(anyhow!("plugin call cancelled or timed out"));
        }
        if name.len() > budgets::image_name_bytes()
            || bytes.len().saturating_add(self.emitted_image_bytes) > budgets::image_bytes()
            || self.emitted_images >= budgets::image_count()
        {
            return Ok(Err("image result exceeds name, byte or count limit".into()));
        }
        let byte_count = bytes.len();
        let result = self.host.emit_image(&self.call, name, bytes)?;
        if let Ok(info) = &result {
            if info.media_type.len() > 64 {
                return Ok(Err("invalid image metadata".into()));
            }
            self.emitted_images += 1;
            self.emitted_image_bytes += byte_count;
        }
        Ok(result)
    }
}

struct Runtime {
    stack_bytes: usize,
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

fn compiler_config(budgets: &Budgets, target: &str) -> Result<Config, Error> {
    let mut config = Config::new();
    config
        .wasm_component_model(true)
        .consume_fuel(true)
        .epoch_interruption(true);
    config.max_wasm_stack(budgets.plugin_stack_bytes);
    config
        .target(target)
        .map_err(runtime("selecting target ISA"))?;
    Ok(config)
}

impl Loader {
    /// package-build compilation, not a runtime native-code cache or tool entry point (D147).
    pub fn precompile_for_target(bytes: &[u8], target: &str) -> Result<Vec<u8>, Error> {
        let engine = Engine::new(&compiler_config(&Budgets::default(), target)?)
            .map_err(runtime("creating package compiler"))?;
        engine
            .precompile_component(bytes)
            .map_err(runtime("compiling bundled component"))
    }

    /// prepare an immutable component shipped as part of the trusted executable (D147).
    ///
    /// # Safety
    /// `native` must be produced by this Wasmtime version from exactly `bytes`, embedded with
    /// the executable by its trusted build pipeline, never read from a runtime-writable source.
    /// both sources still undergo scope, signature and grant-shaped pre-link checks before init.
    pub unsafe fn prepare_embedded<H: HostServices>(
        &self,
        manifest: &Manifest,
        agent: &Grants,
        requested: &Grants,
        bytes: &[u8],
        native: &[u8],
        limits: Limits,
    ) -> Result<Prepared<H>, Error> {
        if bytes.len() > limits.budgets.plugin_component_bytes {
            return Err(Error::Size(limits.budgets.plugin_component_bytes));
        }
        // SAFETY: the caller guarantees immutable build-produced native code, not plugin input.
        let component = unsafe { Component::deserialize(&self.0.engine, native) }
            .map_err(runtime("loading bundled component"))?;
        self.prepare_component(manifest, agent, requested, bytes, limits, Some(component))
    }

    pub fn new() -> Result<Self, Error> {
        Self::with_budgets(&Budgets::default())
    }

    /// construct the engine with a protected stack budget; callers must provide sufficient native stack.
    pub fn with_budgets(budgets: &Budgets) -> Result<Self, Error> {
        budgets.validate().map_err(Error::Refused)?;
        let config = compiler_config(budgets, env!("SADDLE_BUILD_TARGET"))?;
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
            stack_bytes: budgets.plugin_stack_bytes,
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
        self.prepare_component(manifest, agent, requested, bytes, limits, None)
    }

    fn prepare_component<H: HostServices>(
        &self,
        manifest: &Manifest,
        agent: &Grants,
        requested: &Grants,
        bytes: &[u8],
        mut limits: Limits,
        compiled: Option<Component>,
    ) -> Result<Prepared<H>, Error> {
        // Compatibility envelope fields are normalized into the public per-call snapshot.
        limits.budgets.plugin_memory_bytes = limits.memory_bytes;
        limits.budgets.plugin_fuel = limits.fuel;
        limits.budgets.plugin_timeout_ms = usize::try_from(limits.timeout.as_millis())
            .map_err(|_| Error::Refused("plugin timeout does not fit the ABI".into()))?;
        limits.budgets.plugin_input_bytes = limits.input_bytes;
        limits.budgets.plugin_output_bytes = limits.output_bytes;
        if limits.budgets.plugin_stack_bytes != self.0.stack_bytes {
            return Err(Error::Refused(
                "stack budget must match the loader engine".into(),
            ));
        }
        limits.budgets.validate().map_err(Error::Refused)?;
        if bytes.len() > limits.budgets.plugin_component_bytes {
            return Err(Error::Size(limits.budgets.plugin_component_bytes));
        }
        let above = above_ceiling(manifest, requested);
        if !above.is_empty() {
            return Err(Error::Ceiling(above));
        }
        let grants = delegate::read_tool(agent, requested)?;
        let scopes = grants.scopes();
        let engine = &self.0.engine;
        let component = match compiled {
            Some(component) => component,
            None => {
                Component::from_binary(engine, bytes).map_err(runtime("compiling component"))?
            }
        };
        let ty = component.component_type();
        let imports = interfaces(Direction::Import, ty.imports(engine))?;
        let exports = interfaces(Direction::Export, ty.exports(engine))?;
        let checked = check_component(
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
        let exports: Vec<PluginExport> = checked
            .exports
            .into_iter()
            .map(|interface| match interface {
                Interface::Export(export) => Ok(export),
                _ => Err(Error::Refused("import in checked export plan".into())),
            })
            .collect::<Result<_, _>>()?;
        if exports.is_empty() {
            return Err(Error::Refused("plugin has no supported export".into()));
        }
        let mut linker = Linker::<State<H>>::new(engine);
        for interface in checked.imports {
            match interface {
                Interface::Host(host) => bind_host(&mut linker, host),
                Interface::Wasi(wasi) => ambient::bind(&mut linker, wasi),
                Interface::Export(_) => Err(anyhow!("plugin export cannot be bound as an import")),
            }
            .map_err(runtime("binding checked import"))?;
        }
        if exports.contains(&PluginExport::Tool) {
            check_tool_types(engine, &ty)?;
        }
        for export in &exports {
            match export {
                PluginExport::Observer => {
                    check_function::<(), (Vec<HarnessKind>,)>(
                        engine,
                        &ty,
                        export.name(),
                        "subscriptions",
                    )?;
                    check_function::<(HarnessEvent,), (Result<(), String>,)>(
                        engine,
                        &ty,
                        export.name(),
                        "observe",
                    )?;
                }
                PluginExport::PluginEvents => {
                    check_function::<(), (Vec<Subscription>,)>(
                        engine,
                        &ty,
                        export.name(),
                        "subscriptions",
                    )?;
                    check_function::<(PluginEvent,), (Result<(), String>,)>(
                        engine,
                        &ty,
                        export.name(),
                        "receive",
                    )?;
                }
                PluginExport::Tool => {}
                PluginExport::Veto => check_function::<(PendingToolCall,), (VetoDecision,)>(
                    engine,
                    &ty,
                    export.name(),
                    "before-tool",
                )?,
                PluginExport::Ui => check_function::<(), (Result<Panel, String>,)>(
                    engine,
                    &ty,
                    export.name(),
                    "render",
                )?,
            }
        }
        let pre = linker
            .instantiate_pre(&component)
            .map_err(runtime("pre-linking component"))?;
        let tool = if exports.contains(&PluginExport::Tool) {
            Some(ReadToolPre::new(pre.clone()).map_err(runtime("checking tool export types"))?)
        } else {
            None
        };
        Ok(Prepared {
            runtime: self.0.clone(),
            pre,
            tool,
            exports,
            version: manifest.version.clone(),
            principal: Principal::Plugin(manifest.id.clone()),
            grants: Arc::new(grants),
            hash: Sha256::digest(bytes).into(),
            limits,
        })
    }
}

/// adapter dispatch contains no names or permission expressions; those live in the catalogue (D150).
fn bind_host<H: HostServices>(
    linker: &mut Linker<State<H>>,
    interface: HostInterface,
) -> anyhow::Result<()> {
    match interface {
        HostInterface::Http => http::add_to_linker::<_, HasSelf<_>>(linker, |state| state),
        HostInterface::WebSearch => {
            web_search::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
        }
        HostInterface::PluginEvents => {
            plugin_events::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
        }
        HostInterface::RunBinary => {
            run_binary::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
        }
        HostInterface::Exec => exec::add_to_linker::<_, HasSelf<_>>(linker, |state| state),
        HostInterface::ExecTasks => {
            exec_tasks::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
        }
        HostInterface::FsPreimage => {
            fs_preimage::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
        }
        HostInterface::FsWrite => fs_write::add_to_linker::<_, HasSelf<_>>(linker, |state| state),
        HostInterface::FsRead => fs_read::add_to_linker::<_, HasSelf<_>>(linker, |state| state),
        HostInterface::FsReadRange => {
            fs_read_range::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
        }
        HostInterface::FsPaths => fs_paths::add_to_linker::<_, HasSelf<_>>(linker, |state| state),
        HostInterface::FsList => fs_list::add_to_linker::<_, HasSelf<_>>(linker, |state| state),
        HostInterface::ToolPaths => {
            tool_paths::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
        }
        HostInterface::ToolResult => {
            tool_result::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
        }
        HostInterface::TranscriptRead => {
            transcript_read::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
        }
        HostInterface::Budgets => {
            budgets_interface::add_to_linker::<State<H>, HasSelf<State<H>>>(linker, |state| state)
        }
        HostInterface::ContextContribute => {
            context_contribute::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
        }
        HostInterface::Persist => persist::add_to_linker::<_, HasSelf<_>>(linker, |state| state),
        HostInterface::ModelInvoke => {
            model_invoke::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
        }
        HostInterface::Ui => ui::add_to_linker::<_, HasSelf<_>>(linker, |state| state),
        HostInterface::Widgets => {
            widget_types::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
        }
    }
}

/// the generated `Pre::new` checks export names, not their signatures. Check the canonical
/// generated record types before instantiation too, rather than discovering a mismatch after start.
fn check_function<P, R>(
    engine: &Engine,
    ty: &ComponentType,
    interface: &str,
    name: &str,
) -> Result<(), Error>
where
    P: ComponentNamedList + Lower,
    R: ComponentNamedList + Lift,
{
    let interface = format!("{interface}@0.1.0");
    let Some(ComponentItem::ComponentInstance(exports)) = ty.get_export(engine, &interface) else {
        return Err(Error::Refused(format!("missing {interface} export")));
    };
    let Some(ComponentItem::ComponentFunc(function)) = exports.get_export(engine, name) else {
        return Err(Error::Refused(format!(
            "missing {interface} function {name}"
        )));
    };
    function
        .typecheck::<P, R>(&ty.instance_type())
        .map_err(runtime("checking hook export types"))
}

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
    pre: InstancePre<State<H>>,
    tool: Option<ReadToolPre<State<H>>>,
    exports: Vec<PluginExport>,
    version: String,
    principal: Principal,
    grants: Arc<Grants>,
    hash: [u8; 32],
    limits: Limits,
}

impl<H: HostServices> Prepared<H> {
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn exports(&self) -> &[PluginExport] {
        &self.exports
    }

    fn hook<P, R>(
        &self,
        host: H,
        cancelled: Arc<AtomicBool>,
        interface: PluginExport,
        name: &str,
        args: P,
        deadline: Option<Instant>,
    ) -> Result<R, Error>
    where
        P: ComponentNamedList + Lower,
        R: ComponentNamedList + Lift,
    {
        budgets::using(&self.limits.budgets, || {
            if !self.exports.contains(&interface) {
                return Err(Error::Refused("plugin does not export this hook".into()));
            }
            let mut store = self.store_until(host, cancelled, deadline)?;
            let instance = self
                .pre
                .instantiate(&mut store)
                .map_err(runtime("initializing hook"))?;
            let (_, parent) = instance
                .get_export(&mut store, None, &format!("{}@0.1.0", interface.name()))
                .ok_or_else(|| Error::Refused("missing hook interface".into()))?;
            let (_, index) = instance
                .get_export(&mut store, Some(&parent), name)
                .ok_or_else(|| Error::Refused("missing hook function".into()))?;
            let func = instance
                .get_typed_func::<P, R>(&mut store, &index)
                .map_err(runtime("checking hook function"))?;
            let result = func
                .call(&mut store, args)
                .map_err(runtime("calling hook"))?;
            func.post_return(&mut store)
                .map_err(runtime("finishing hook"))?;
            Self::check_live(&store)?;
            Ok(result)
        })
    }

    pub fn harness_subscriptions(
        &self,
        host: H,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Vec<HarnessKind>, Error> {
        let (items,): (Vec<HarnessKind>,) = self.hook(
            host,
            cancelled,
            PluginExport::Observer,
            "subscriptions",
            (),
            None,
        )?;
        if items.len() > self.limits.budgets.event_subscriptions {
            return Err(Error::Size(self.limits.budgets.event_subscriptions));
        }
        Ok(items)
    }
    pub fn event_subscriptions(
        &self,
        host: H,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Vec<Subscription>, Error> {
        let (items,): (Vec<Subscription>,) = self.hook(
            host,
            cancelled,
            PluginExport::PluginEvents,
            "subscriptions",
            (),
            None,
        )?;
        if items.len() > self.limits.budgets.event_subscriptions
            || items.iter().fold(0usize, |total, item| {
                total
                    .saturating_add(item.publisher.len())
                    .saturating_add(item.name.len())
            }) > self.limits.output_bytes
            || items.iter().any(|item| {
                item.publisher.len().saturating_add(item.name.len())
                    > self.limits.budgets.event_bytes
            })
        {
            return Err(Error::Size(self.limits.budgets.event_subscriptions));
        }
        Ok(items)
    }
    pub fn observe(
        &self,
        host: H,
        event: HarnessEvent,
        cancelled: Arc<AtomicBool>,
        deadline: Option<Instant>,
    ) -> Result<Result<(), String>, Error> {
        if event.data.len() > self.limits.input_bytes {
            return Err(Error::Size(self.limits.input_bytes));
        }
        let (result,): (Result<(), String>,) = self.hook(
            host,
            cancelled,
            PluginExport::Observer,
            "observe",
            (event,),
            deadline,
        )?;
        self.hook_result(result)
    }
    pub fn receive(
        &self,
        host: H,
        event: PluginEvent,
        cancelled: Arc<AtomicBool>,
        deadline: Option<Instant>,
    ) -> Result<Result<(), String>, Error> {
        if event
            .publisher
            .len()
            .saturating_add(event.name.len())
            .saturating_add(event.data.len())
            > self.limits.input_bytes
        {
            return Err(Error::Size(self.limits.input_bytes));
        }
        let (result,): (Result<(), String>,) = self.hook(
            host,
            cancelled,
            PluginExport::PluginEvents,
            "receive",
            (event,),
            deadline,
        )?;
        self.hook_result(result)
    }
    pub fn veto(
        &self,
        host: H,
        call: PendingToolCall,
        cancelled: Arc<AtomicBool>,
        deadline: Option<Instant>,
    ) -> Result<VetoDecision, Error> {
        if call.name.len().saturating_add(call.arguments.len()) > self.limits.input_bytes {
            return Err(Error::Size(self.limits.input_bytes));
        }
        let (decision,): (VetoDecision,) = self.hook(
            host,
            cancelled,
            PluginExport::Veto,
            "before-tool",
            (call,),
            deadline,
        )?;
        if matches!(&decision, VetoDecision::Deny(reason) if reason.len() > self.limits.output_bytes)
        {
            return Err(Error::Size(self.limits.output_bytes));
        }
        Ok(decision)
    }
    pub fn render(
        &self,
        host: H,
        cancelled: Arc<AtomicBool>,
        deadline: Option<Instant>,
    ) -> Result<Result<Panel, String>, Error> {
        let (result,): (Result<Panel, String>,) =
            self.hook(host, cancelled, PluginExport::Ui, "render", (), deadline)?;
        if result
            .as_ref()
            .err()
            .is_some_and(|why| why.len() > self.limits.output_bytes)
        {
            return Err(Error::Size(self.limits.output_bytes));
        }
        Ok(result)
    }

    fn hook_result(&self, result: Result<(), String>) -> Result<Result<(), String>, Error> {
        if result
            .as_ref()
            .err()
            .is_some_and(|why| why.len() > self.limits.output_bytes)
        {
            return Err(Error::Size(self.limits.output_bytes));
        }
        Ok(result)
    }

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

    /// native stack the caller must reserve in addition to its own host frame allowance.
    pub fn stack_bytes(&self) -> usize {
        self.limits.budgets.plugin_stack_bytes
    }

    pub fn hash(&self) -> &[u8; 32] {
        &self.hash
    }
    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    fn store(&self, host: H, cancelled: Arc<AtomicBool>) -> Result<Store<State<H>>, Error> {
        self.store_until(host, cancelled, None)
    }

    fn store_until(
        &self,
        host: H,
        cancelled: Arc<AtomicBool>,
        hard_deadline: Option<Instant>,
    ) -> Result<Store<State<H>>, Error> {
        if cancelled.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let deadline = Instant::now()
            .checked_add(self.limits.timeout)
            .ok_or_else(|| Error::Refused("timeout is too large".into()))?;
        let deadline = hard_deadline.map_or(deadline, |end| deadline.min(end));
        let state = State {
            host,
            call: Invocation {
                principal: self.principal.clone(),
                budgets: self.limits.budgets.clone(),
                id: self.runtime.next_call.fetch_add(1, Ordering::Relaxed),
                grants: self.grants.clone(),
                cancelled,
                deadline,
                hard_deadline,
            },
            limits: StoreLimitsBuilder::new()
                .memory_size(self.limits.memory_bytes)
                .table_elements(self.limits.budgets.plugin_table_elements)
                .instances(self.limits.budgets.plugin_instances)
                .memories(self.limits.budgets.plugin_memories)
                .tables(self.limits.budgets.plugin_tables)
                .trap_on_grow_failure(true)
                .build(),
            wasi: WasiCtx::builder()
                .max_random_size(self.limits.budgets.plugin_random_bytes as u64)
                .build(),
            resources: {
                let mut table = ResourceTable::new();
                table.set_max_capacity(self.limits.budgets.plugin_resources);
                table
            },
            emitted_images: 0,
            emitted_image_bytes: 0,
            host_time_left: Duration::from_secs(self.limits.budgets.host_wait_seconds as u64),
            host_calls: 0,
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
        budgets::using(&self.limits.budgets, || {
            self.describe_inner(host, cancelled)
        })
    }

    fn describe_inner(&self, host: H, cancelled: Arc<AtomicBool>) -> Result<Definition, Error> {
        let mut store = self.store(host, cancelled)?;
        let guest = self
            .tool
            .as_ref()
            .ok_or_else(|| Error::Refused("plugin does not export a tool".into()))?
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
        self.invoke_until(host, arguments, cancelled, None)
    }

    /// apply an absolute caller deadline in addition to CPU and host-wait budgets (D152).
    pub fn invoke_until(
        &self,
        host: H,
        arguments: &str,
        cancelled: Arc<AtomicBool>,
        deadline: Option<Instant>,
    ) -> Result<Output, Error> {
        budgets::using(&self.limits.budgets, || {
            self.invoke_inner(host, arguments, cancelled, deadline)
        })
    }

    fn invoke_inner(
        &self,
        host: H,
        arguments: &str,
        cancelled: Arc<AtomicBool>,
        deadline: Option<Instant>,
    ) -> Result<Output, Error> {
        if arguments.len() > self.limits.input_bytes {
            return Err(Error::Size(self.limits.input_bytes));
        }
        let mut store = self.store_until(host, cancelled, deadline)?;
        let guest = self
            .tool
            .as_ref()
            .ok_or_else(|| Error::Refused("plugin does not export a tool".into()))?
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
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use cap_std::fs::Dir;
    use saddle_cap::component::WasiInterface;
    use saddle_cap::{Location, PluginId, Policy, RelPath, RootName, Roots};
    use saddle_policy::audit::{self, AuditLog};
    use saddle_policy::{Engine as PolicyEngine, Request};
    use saddle_standard::{SDK_HOOKS, SDK_READER};
    use serde_json::Value;
    use tempfile::TempDir;
    use wasmtime_wasi::p2::bindings::cli::environment::Host as EnvironmentHost;
    use wasmtime_wasi::p2::bindings::filesystem::preopens::Host as PreopensHost;
    use wasmtime_wasi::p2::bindings::random::random::Host as RandomHost;
    use wasmtime_wasi::random::WasiRandomView;

    #[test]
    fn exhaustive_binders_agree_with_catalogue_availability() {
        let loader = Loader::new().unwrap();
        for interface in HostInterface::ALL {
            let mut linker = Linker::<State<NoHost>>::new(&loader.0.engine);
            assert_eq!(
                bind_host(&mut linker, *interface).is_ok(),
                interface.implemented(),
                "{}",
                interface.name()
            );
        }
        for interface in WasiInterface::ALL {
            let mut linker = Linker::<State<NoHost>>::new(&loader.0.engine);
            assert_eq!(
                ambient::bind(&mut linker, *interface).is_ok(),
                interface.implemented(),
                "{}",
                interface.name()
            );
        }
    }

    #[test]
    fn compatible_wasi_import_versions_bind_once() {
        let (_dir, agent, plugin) = grants(GRANTS);
        let source = READER.replacen("(component", "(component (import \"wasi:io/error@0.2.0\" (instance)) (import \"wasi:io/error@0.2.3\" (instance))", 1);
        let prepared = Loader::new()
            .unwrap()
            .prepare::<NoHost>(
                &manifest(),
                &agent,
                &plugin,
                &wat::parse_str(source).unwrap(),
                Limits::default(),
            )
            .unwrap();
        prepared.describe(NoHost, cancelled()).unwrap();
    }

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

    struct QueryHost {
        calls: Arc<AtomicUsize>,
        result: Result<String, String>,
    }
    impl HostServices for QueryHost {
        fn read_file(
            &mut self,
            _: &Invocation,
            _: Path,
        ) -> anyhow::Result<Result<Vec<u8>, Failure>> {
            panic!("transcript-only plugin must not read files");
        }
        fn query_transcript(
            &mut self,
            call: &Invocation,
            _: String,
            _: String,
        ) -> anyhow::Result<Result<String, String>> {
            assert!(call.grants().fs_read.is_empty());
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.result.clone())
        }
    }

    #[test]
    fn transcript_adapter_bounds_input_and_both_success_and_error_outputs() {
        let (_dir, agent, plugin) =
            grants("[agent]\ntranscript-read={}\n[plugin.reader]\ntool={}\ntranscript-read={}\n");
        let manifest = Manifest::parse("id='reader'\nversion='0.1.0'\ncomponent='query.wasm'\nscopes=['tool','transcript-read']\n").unwrap();
        let bytes = wat::parse_str(include_str!("fixtures/transcript-tool.wat")).unwrap();
        let prepared = Loader::new()
            .unwrap()
            .prepare::<QueryHost>(&manifest, &agent, &plugin, &bytes, Limits::default())
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut store = prepared
            .store(
                QueryHost {
                    calls: calls.clone(),
                    result: Ok("ok".into()),
                },
                cancelled(),
            )
            .unwrap();
        assert!(
            TranscriptHost::query(
                store.data_mut(),
                "x".repeat(MAX_QUERY_BYTES + 1),
                String::new()
            )
            .unwrap()
            .is_err()
        );
        assert!(
            TranscriptHost::query(
                store.data_mut(),
                "SELECT 1".into(),
                "s".repeat(budgets::directory_name_bytes() + 1)
            )
            .unwrap()
            .is_err(),
            "a selector longer than a name is refused"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            TranscriptHost::query(store.data_mut(), "SELECT 1".into(), String::new())
                .unwrap()
                .unwrap(),
            "ok"
        );
        for result in [
            Ok("x".repeat(MAX_QUERY_RESULT_BYTES + 1)),
            Err("x".repeat(MAX_QUERY_RESULT_BYTES + 1)),
        ] {
            store.data_mut().host.result = result;
            assert!(
                TranscriptHost::query(store.data_mut(), "SELECT 1".into(), String::new())
                    .unwrap()
                    .unwrap_err()
                    .contains("50 KiB")
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn transcript_import_without_its_grant_fails_before_initialization() {
        let (_dir, agent, plugin) =
            grants("[agent]\ntranscript-read={}\n[plugin.reader]\ntool={}\n");
        let bytes = wat::parse_str(include_str!("fixtures/transcript-tool.wat").replace(
            ";; initialization-probe",
            "(func $start unreachable) (start $start)",
        ))
        .unwrap();
        let why = refusal(Loader::new().unwrap().prepare::<NoHost>(
            &manifest(),
            &agent,
            &plugin,
            &bytes,
            Limits::default(),
        ));
        assert!(
            why.contains("transcript-read") && why.contains("not granted"),
            "{why}"
        );
        assert!(!why.contains("unreachable"), "{why}");
    }

    struct NetworkHost(Arc<AtomicUsize>);
    impl HostServices for NetworkHost {
        fn read_file(
            &mut self,
            _: &Invocation,
            _: Path,
        ) -> anyhow::Result<Result<Vec<u8>, Failure>> {
            panic!("network adapter must not read files");
        }
        fn http_get(
            &mut self,
            _: &Invocation,
            _: HttpRequest,
        ) -> anyhow::Result<Result<HttpResponse, String>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: vec![0; 100 * 1024 + 1],
                truncated: false,
            }))
        }
        fn search_web(
            &mut self,
            _: &Invocation,
            query: String,
            count: u32,
        ) -> anyhow::Result<Result<SearchSnapshot, String>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(Ok(SearchSnapshot {
                query,
                count,
                via: "provider".into(),
                provider: "test".into(),
                status: 200,
                body: "x".repeat(1024 * 1024 + 1),
            }))
        }
    }

    #[test]
    fn network_adapters_bound_arguments_before_dispatch_and_replies_before_guest_return() {
        let (_dir, agent, plugin) = grants(
            "[agent]\nfs-read=[{root='workspace'}]\n[plugin.reader]\ntool={}\nfs-read=[{root='workspace'}]\n",
        );
        let bytes = wat::parse_str(include_str!("fixtures/read-tool.wat")).unwrap();
        let prepared = Loader::new()
            .unwrap()
            .prepare::<NetworkHost>(&manifest(), &agent, &plugin, &bytes, Limits::default())
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut store = prepared
            .store(NetworkHost(calls.clone()), cancelled())
            .unwrap();
        assert!(
            HttpHost::get(
                store.data_mut(),
                HttpRequest {
                    url: "x".repeat(8193),
                    headers: vec![]
                }
            )
            .unwrap()
            .is_err()
        );
        assert!(
            SearchHost::search(store.data_mut(), "x".repeat(16385), 1)
                .unwrap()
                .is_err()
        );
        assert!(
            SearchHost::search(store.data_mut(), "query".into(), 101)
                .unwrap()
                .is_err()
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(
            HttpHost::get(
                store.data_mut(),
                HttpRequest {
                    url: "https://example.test/".into(),
                    headers: vec![]
                }
            )
            .unwrap()
            .is_err()
        );
        assert!(
            SearchHost::search(store.data_mut(), "query".into(), 1)
                .unwrap()
                .is_err()
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        let state = store.data_mut();
        state.host_time_left = Duration::ZERO;
        assert!(
            HttpHost::get(
                state,
                HttpRequest {
                    url: "https://example.test/".into(),
                    headers: vec![]
                }
            )
            .is_err()
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    struct MutationHost(Arc<AtomicUsize>);
    impl HostServices for MutationHost {
        fn read_file(
            &mut self,
            _: &Invocation,
            _: Path,
        ) -> anyhow::Result<Result<Vec<u8>, Failure>> {
            panic!("writer must not read");
        }
        fn apply_changes(
            &mut self,
            call: &Invocation,
            _: Vec<Change>,
        ) -> anyhow::Result<Result<(), String>> {
            assert!(call.grants().fs_read.is_empty());
            assert_eq!(call.grants().fs_write.len(), 1);
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(Ok(()))
        }
    }

    #[test]
    fn mutation_adapter_bounds_batches_before_host_dispatch() {
        let (_dir, agent, plugin) = grants(
            "[agent]\nfs-write=[{root='workspace'}]\n[plugin.reader]\ntool={}\nfs-write=[{root='workspace'}]\n",
        );
        let manifest = Manifest::parse(
            "id='reader'\nversion='0.1.0'\ncomponent='mutator.wasm'\nscopes=['tool','fs-write']",
        )
        .unwrap();
        let bytes = wat::parse_str(include_str!("fixtures/mutation-tool.wat")).unwrap();
        let prepared = Loader::new()
            .unwrap()
            .prepare::<MutationHost>(&manifest, &agent, &plugin, &bytes, Limits::default())
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut store = prepared
            .store(MutationHost(calls.clone()), cancelled())
            .unwrap();
        let change = || Change {
            path: "file".into(),
            content: Some(b"content".to_vec()),
            expected: Expectation::Any,
        };
        for changes in [
            Vec::new(),
            (0..33).map(|_| change()).collect(),
            vec![Change {
                content: Some(vec![0; MAX_MUTATION_BYTES + 1]),
                ..change()
            }],
            vec![Change {
                expected: Expectation::Sha256("invalid".into()),
                ..change()
            }],
        ] {
            assert!(
                WriteHost::apply(store.data_mut(), changes)
                    .unwrap()
                    .is_err()
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(
            WriteHost::apply(store.data_mut(), vec![change()])
                .unwrap()
                .is_ok()
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
    fn filesystem_imports_need_a_read_capability_before_initialization() {
        let (_dir, agent, plugin) =
            grants("[agent]\nfs-read=[{root='workspace'}]\n[plugin.reader]\ntool={}\n");
        let source = READER
            .replacen(
                "(component",
                "(component (import \"saddle:host/tool-paths@0.1.0\" (instance))",
                1,
            )
            .replace(
                ";; initialization-probe",
                "(func $start unreachable) (start $start)",
            );
        let why = refusal(Loader::new().unwrap().prepare::<NoHost>(
            &manifest(),
            &agent,
            &plugin,
            &wat::parse_str(source).unwrap(),
            Limits::default(),
        ));
        assert!(
            why.contains("tool-read") && why.contains("not granted"),
            "{why}"
        );
        assert!(!why.contains("unreachable"), "{why}");
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
    fn caller_deadline_interrupts_guest_without_repreparing_or_extending_cpu_budget() {
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
                    timeout: Duration::from_secs(30),
                    ..Limits::default()
                },
            )
            .unwrap();
        let start = Instant::now();
        let why = prepared
            .invoke_until(
                NoHost,
                "",
                cancelled(),
                Some(start + Duration::from_millis(40)),
            )
            .unwrap_err()
            .to_string();
        assert!(why.contains("timed out"), "{why}");
        assert!(start.elapsed() < Duration::from_secs(2));
        // A later caller deadline cannot replace the independent guest CPU deadline.
        let mut store = prepared
            .store_until(
                NoHost,
                cancelled(),
                Some(Instant::now() + Duration::from_secs(60)),
            )
            .unwrap();
        assert!(store.data().call.deadline <= Instant::now() + Duration::from_secs(30));
        store.data_mut().call.deadline = Instant::now();
        assert!(Prepared::<NoHost>::check_live(&store).is_err());
    }

    #[test]
    fn absolute_deadline_caps_all_host_waits_and_does_not_leak_to_the_next_invocation() {
        let (_dir, agent, plugin) = grants(GRANTS);
        let prepared = Loader::new()
            .unwrap()
            .prepare::<NoHost>(
                &manifest(),
                &agent,
                &plugin,
                &wat::parse_str(READER).unwrap(),
                Limits::default(),
            )
            .unwrap();
        let end = Instant::now() + Duration::from_secs(1);
        let mut store = prepared
            .store_until(NoHost, cancelled(), Some(end))
            .unwrap();
        for _ in 0..2 {
            store
                .data_mut()
                .host_wait(Duration::from_secs(30), |_, call, allowance| {
                    assert!(call.deadline() <= end);
                    assert!(allowance <= Duration::from_secs(1));
                    thread::sleep(Duration::from_millis(10));
                    Ok(())
                })
                .unwrap();
            assert!(store.data().call.deadline <= end);
        }
        let why = prepared
            .invoke_until(NoHost, "", cancelled(), Some(Instant::now()))
            .unwrap_err()
            .to_string();
        assert!(why.contains("timed out"), "{why}");
        // Same prepared artifact, ordinary call after expiry, fresh deadline.
        assert!(prepared.store(NoHost, cancelled()).is_ok());
        assert!(prepared.describe(NoHost, cancelled()).is_ok());
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
                    timeout: Duration::from_secs(budgets::network_seconds() as u64),
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
        for name in [
            "wasi:io/poll@0.2.3",
            "wasi:http/outgoing-handler@0.2.3",
            "wasi:http/types@0.2.3",
            "wasi:io/unknown@0.2.3",
            "wasi:random/unknown@0.2.3",
            "wasi:cli/terminal-unknown@0.2.3",
        ] {
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

    #[test]
    fn a_hook_only_sdk_artifact_registers_without_a_tool_export() {
        let (_dir, agent, plugin) = grants(
            "[agent]\ntranscript-read={}\nevent-publish={}\nevent-subscribe={}\nfs-read=[{root='workspace'}]\nfs-write=[{root='workspace'}]\n[plugin.reader]\ntranscript-read={}\nevent-publish={}\nevent-subscribe={}\nfs-read=[{root='workspace'}]\nfs-write=[{root='workspace'}]\n",
        );
        let mut manifest = manifest();
        manifest.scopes = plugin.scopes();
        let prepared = Loader::new()
            .unwrap()
            .prepare::<NoHost>(&manifest, &agent, &plugin, SDK_HOOKS, Limits::default())
            .unwrap();
        assert!(!prepared.exports().contains(&PluginExport::Tool));
        assert_eq!(
            prepared.harness_subscriptions(NoHost, cancelled()).unwrap(),
            [HarnessKind::TurnStart]
        );
        let subscriptions = prepared.event_subscriptions(NoHost, cancelled()).unwrap();
        assert_eq!(subscriptions.len(), 1);
        assert_eq!(subscriptions[0].publisher, "hooks");
        assert_eq!(subscriptions[0].name, "turn-start");
        assert!(prepared.describe(NoHost, cancelled()).is_err());
    }

    #[test]
    fn an_incomplete_observer_export_is_refused_before_initialization() {
        let (_dir, agent, plugin) = grants(
            "[agent]\nfs-read=[{root='workspace'}]\ntranscript-read={}\n[plugin.reader]\ntool={}\nfs-read=[{root='workspace'}]\ntranscript-read={}\n",
        );
        let mut manifest = manifest();
        manifest.scopes.insert(Scope::TranscriptRead);
        let source = READER.replacen("(component", r#"(component (instance $observer) (export "saddle:plugin/observer@0.1.0" (instance $observer))"#, 1).replace(";; initialization-probe", "(func $start unreachable) (start $start)");
        let why = refusal(Loader::new().unwrap().prepare::<NoHost>(
            &manifest,
            &agent,
            &plugin,
            &wat::parse_str(source).unwrap(),
            Limits::default(),
        ));
        assert!(
            why.contains("observer") && why.contains("subscriptions"),
            "{why}"
        );
        assert!(!why.contains("unreachable"), "{why}");
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
    fn sdk_reader_component_round_trips_through_the_runtime() {
        let (dir, agent, plugin) = grants(GRANTS);
        fs::write(dir.path().join("file"), "SDK round trip").unwrap();
        fs::write(dir.path().join(".env"), "secret").unwrap();
        let bytes = SDK_READER;
        let loader = Loader::new().unwrap();
        let limits = Limits::default();
        let prepared = loader
            .prepare::<Files>(&manifest(), &agent, &plugin, bytes, limits)
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
