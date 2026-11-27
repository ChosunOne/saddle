use saddle_cap::component::WasiInterface;
use wasmtime::component::{HasData, Linker, ResourceTable};
use wasmtime_wasi::WasiView;
use wasmtime_wasi::cli::{WasiCli, WasiCliView as _};
use wasmtime_wasi::clocks::{WasiClocks, WasiClocksView as _};
use wasmtime_wasi::filesystem::{WasiFilesystem, WasiFilesystemView as _};
use wasmtime_wasi::p2::bindings::cli::{
    environment, exit, stderr, stdin, stdout, terminal_input, terminal_output, terminal_stderr,
    terminal_stdin, terminal_stdout,
};
use wasmtime_wasi::p2::bindings::clocks::{monotonic_clock, wall_clock};
use wasmtime_wasi::p2::bindings::filesystem::preopens;
use wasmtime_wasi::p2::bindings::random::{insecure, insecure_seed, random};
use wasmtime_wasi::p2::bindings::sync::filesystem::types as filesystem_types;
use wasmtime_wasi::p2::bindings::sync::io::streams;
use wasmtime_wasi::random::{WasiRandom, WasiRandomCtx};
use wasmtime_wasi_io::bindings::wasi::io::error as io_error;

use super::{HostServices, State};

struct TableData;
impl HasData for TableData {
    type Data<'a> = &'a mut ResourceTable;
}

fn table<H: HostServices>(state: &mut State<H>) -> &mut ResourceTable {
    state.ctx().table
}

fn random_ctx<H: HostServices>(state: &mut State<H>) -> &mut WasiRandomCtx {
    state.ctx().ctx.random()
}

pub(super) fn bind<H: HostServices>(
    linker: &mut Linker<State<H>>,
    interface: WasiInterface,
) -> wasmtime::Result<()> {
    match interface {
        WasiInterface::Environment => {
            environment::add_to_linker::<State<H>, WasiCli>(linker, State::<H>::cli)
        }
        WasiInterface::Exit => exit::add_to_linker::<State<H>, WasiCli>(linker, State::<H>::cli),
        WasiInterface::Stdin => stdin::add_to_linker::<State<H>, WasiCli>(linker, State::<H>::cli),
        WasiInterface::Stdout => {
            stdout::add_to_linker::<State<H>, WasiCli>(linker, State::<H>::cli)
        }
        WasiInterface::Stderr => {
            stderr::add_to_linker::<State<H>, WasiCli>(linker, State::<H>::cli)
        }
        WasiInterface::TerminalInput => {
            terminal_input::add_to_linker::<State<H>, WasiCli>(linker, State::<H>::cli)
        }
        WasiInterface::TerminalOutput => {
            terminal_output::add_to_linker::<State<H>, WasiCli>(linker, State::<H>::cli)
        }
        WasiInterface::TerminalStdin => {
            terminal_stdin::add_to_linker::<State<H>, WasiCli>(linker, State::<H>::cli)
        }
        WasiInterface::TerminalStdout => {
            terminal_stdout::add_to_linker::<State<H>, WasiCli>(linker, State::<H>::cli)
        }
        WasiInterface::TerminalStderr => {
            terminal_stderr::add_to_linker::<State<H>, WasiCli>(linker, State::<H>::cli)
        }
        WasiInterface::WallClock => {
            wall_clock::add_to_linker::<State<H>, WasiClocks>(linker, State::<H>::clocks)
        }
        WasiInterface::MonotonicClock => {
            monotonic_clock::add_to_linker::<State<H>, WasiClocks>(linker, State::<H>::clocks)
        }
        WasiInterface::Preopens => {
            preopens::add_to_linker::<State<H>, WasiFilesystem>(linker, State::<H>::filesystem)
        }
        WasiInterface::FilesystemTypes => {
            filesystem_types::add_to_linker::<State<H>, WasiFilesystem>(
                linker,
                State::<H>::filesystem,
            )
        }
        WasiInterface::Random => random::add_to_linker::<State<H>, WasiRandom>(linker, random_ctx),
        WasiInterface::InsecureRandom => {
            insecure::add_to_linker::<State<H>, WasiRandom>(linker, random_ctx)
        }
        WasiInterface::InsecureSeed => {
            insecure_seed::add_to_linker::<State<H>, WasiRandom>(linker, random_ctx)
        }
        WasiInterface::IoError => io_error::add_to_linker::<State<H>, TableData>(linker, table),
        WasiInterface::Streams => streams::add_to_linker::<State<H>, TableData>(linker, table),
        WasiInterface::Poll | WasiInterface::HttpOutgoingHandler | WasiInterface::HttpTypes => {
            Err(wasmtime::Error::msg(format!(
                "wasi adapter is not implemented: {}",
                interface.name()
            )))
        }
    }
}
