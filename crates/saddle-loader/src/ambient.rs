use anyhow::{Result, bail};
use saddle_cap::component::WasiInterface;
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

pub(super) fn bind<H: HostServices>(
    linker: &mut Linker<State<H>>,
    interface: WasiInterface,
) -> Result<()> {
    match interface {
        WasiInterface::Environment => {
            environment::add_to_linker::<_, Context>(linker, WasiView::ctx)
        }
        WasiInterface::Exit => {
            exit::add_to_linker::<_, Context>(linker, &Default::default(), WasiView::ctx)
        }
        WasiInterface::Stdin => stdin::add_to_linker::<_, Context>(linker, WasiView::ctx),
        WasiInterface::Stdout => stdout::add_to_linker::<_, Context>(linker, WasiView::ctx),
        WasiInterface::Stderr => stderr::add_to_linker::<_, Context>(linker, WasiView::ctx),
        WasiInterface::TerminalInput => {
            terminal_input::add_to_linker::<_, Context>(linker, WasiView::ctx)
        }
        WasiInterface::TerminalOutput => {
            terminal_output::add_to_linker::<_, Context>(linker, WasiView::ctx)
        }
        WasiInterface::TerminalStdin => {
            terminal_stdin::add_to_linker::<_, Context>(linker, WasiView::ctx)
        }
        WasiInterface::TerminalStdout => {
            terminal_stdout::add_to_linker::<_, Context>(linker, WasiView::ctx)
        }
        WasiInterface::TerminalStderr => {
            terminal_stderr::add_to_linker::<_, Context>(linker, WasiView::ctx)
        }
        WasiInterface::WallClock => wall_clock::add_to_linker::<_, Context>(linker, WasiView::ctx),
        WasiInterface::MonotonicClock => {
            monotonic_clock::add_to_linker::<_, Context>(linker, WasiView::ctx)
        }
        WasiInterface::Preopens => preopens::add_to_linker::<_, Context>(linker, WasiView::ctx),
        WasiInterface::FilesystemTypes => {
            filesystem_types::add_to_linker::<_, Context>(linker, WasiView::ctx)
        }
        WasiInterface::Random => {
            random::add_to_linker::<_, HasSelf<WasiRandomCtx>>(linker, WasiRandomView::random)
        }
        WasiInterface::InsecureRandom => {
            insecure::add_to_linker::<_, HasSelf<WasiRandomCtx>>(linker, WasiRandomView::random)
        }
        WasiInterface::InsecureSeed => insecure_seed::add_to_linker::<_, HasSelf<WasiRandomCtx>>(
            linker,
            WasiRandomView::random,
        ),
        WasiInterface::IoError => {
            io_error::add_to_linker::<_, HasSelf<ResourceTable>>(linker, |s| &mut s.resources)
        }
        WasiInterface::Streams => {
            streams::add_to_linker::<_, HasSelf<ResourceTable>>(linker, |s| &mut s.resources)
        }
        WasiInterface::Poll | WasiInterface::HttpOutgoingHandler | WasiInterface::HttpTypes => {
            bail!("wasi adapter is not implemented: {}", interface.name())
        }
    }
}
