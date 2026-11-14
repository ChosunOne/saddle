//! only authority-free WASI adapters are linked (D11, D12, D127).

use anyhow::Result;
use wasmtime::component::{HasData, HasSelf, Linker, ResourceTable};
use wasmtime_wasi::p2::bindings::cli::{
    environment, exit, stderr, stdin, stdout, terminal_input, terminal_output, terminal_stderr,
    terminal_stdin, terminal_stdout,
};
use wasmtime_wasi::p2::bindings::clocks::{monotonic_clock, wall_clock};
use wasmtime_wasi::p2::bindings::filesystem::preopens;
use wasmtime_wasi::p2::bindings::random::{insecure, insecure_seed, random};
use wasmtime_wasi::p2::bindings::sync::filesystem::types as filesystem_types;
use wasmtime_wasi::p2::bindings::sync::io::{error as io_error, streams};
use wasmtime_wasi::random::{WasiRandomCtx, WasiRandomView};
use wasmtime_wasi::{WasiCtxView, WasiView};

use super::{HostServices, State};

struct Context;
impl HasData for Context {
    type Data<'a> = WasiCtxView<'a>;
}

pub(super) fn supports(name: &str) -> bool {
    let Some((interface, _)) = name.split_once('@') else {
        return false;
    };
    matches!(
        interface,
        "wasi:cli/environment"
            | "wasi:cli/exit"
            | "wasi:cli/stdin"
            | "wasi:cli/stdout"
            | "wasi:cli/stderr"
            | "wasi:cli/terminal-input"
            | "wasi:cli/terminal-output"
            | "wasi:cli/terminal-stdin"
            | "wasi:cli/terminal-stdout"
            | "wasi:cli/terminal-stderr"
            | "wasi:clocks/wall-clock"
            | "wasi:clocks/monotonic-clock"
            | "wasi:filesystem/types"
            | "wasi:filesystem/preopens"
            | "wasi:random/random"
            | "wasi:random/insecure"
            | "wasi:random/insecure-seed"
            | "wasi:io/error"
            | "wasi:io/streams"
    )
}

pub(super) fn add<H: HostServices>(linker: &mut Linker<State<H>>) -> Result<()> {
    environment::add_to_linker::<_, Context>(linker, WasiView::ctx)?;
    exit::add_to_linker::<_, Context>(linker, &Default::default(), WasiView::ctx)?;
    stdin::add_to_linker::<_, Context>(linker, WasiView::ctx)?;
    stdout::add_to_linker::<_, Context>(linker, WasiView::ctx)?;
    stderr::add_to_linker::<_, Context>(linker, WasiView::ctx)?;
    terminal_input::add_to_linker::<_, Context>(linker, WasiView::ctx)?;
    terminal_output::add_to_linker::<_, Context>(linker, WasiView::ctx)?;
    terminal_stdin::add_to_linker::<_, Context>(linker, WasiView::ctx)?;
    terminal_stdout::add_to_linker::<_, Context>(linker, WasiView::ctx)?;
    terminal_stderr::add_to_linker::<_, Context>(linker, WasiView::ctx)?;
    wall_clock::add_to_linker::<_, Context>(linker, WasiView::ctx)?;
    monotonic_clock::add_to_linker::<_, Context>(linker, WasiView::ctx)?;
    preopens::add_to_linker::<_, Context>(linker, WasiView::ctx)?;
    filesystem_types::add_to_linker::<_, Context>(linker, WasiView::ctx)?;
    random::add_to_linker::<_, HasSelf<WasiRandomCtx>>(linker, WasiRandomView::random)?;
    insecure::add_to_linker::<_, HasSelf<WasiRandomCtx>>(linker, WasiRandomView::random)?;
    insecure_seed::add_to_linker::<_, HasSelf<WasiRandomCtx>>(linker, WasiRandomView::random)?;
    io_error::add_to_linker::<_, HasSelf<ResourceTable>>(linker, |s| &mut s.resources)?;
    streams::add_to_linker::<_, HasSelf<ResourceTable>>(linker, |s| &mut s.resources)?;
    // Do not link synchronous io/poll: a guest-chosen clock subscription could block a native
    // poll forever, where fuel and epoch interruption cannot help. A cancellable adapter is next.
    // No sockets, outgoing-handler or broad WASI helper is defined here, even without imports.
    Ok(())
}
