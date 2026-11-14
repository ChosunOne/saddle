# WASI plugin development: the initial library slice

The first runtime boundary is implemented (D126, D127). It is **not yet an installed extension
system**: `saddle` does not discover manifests, read a `[plugins]` configuration table, or register
these tools with a model. There is no plugin CLI subcommand yet. Do not put a manifest in a repository
and expect it to execute. Session host adapters, explicit installation/configuration and registration
with D25 provenance are the next steps.

## What exists

- `saddle-wit/wit`: canonical `saddle:host/fs-read@0.1.0` and `saddle:plugin/tool@0.1.0` interfaces,
  and the `read-tool` world. The host and SDK generate their bindings from these same files.
- `saddle-loader`: compilation and inspection of explicit component bytes, capability checks,
  pre-linking without initialization, and a fresh bounded store/instance for every `describe` or
  `invoke`. Both import types and export signatures are checked before initialization.
- `saddle-sdk`: generated guest bindings and a small read-only Rust example. It calls the host's
  `read-file` function, never `std::fs`.

Operational authority is bounded by the agent, then the manifest, then the plugin's explicit grants.
There is no inheritance when a grant is omitted, and no borrowing authority from another plugin.
An above-ceiling request is an error. Both filesystem exclusion matchers apply. `tool` authorizes
registration of an export, not the operations it performs; native tools already have the agent's
host-managed dispatch surface without an explicit `agent.tool` grant.

Only `fs-read` and `tool` grants are implemented by this first runtime. Other grants fail closed.
The pure component classifier still describes the wider planned ABI; that does not make an adapter
available in the runtime.

## Host integration contract

`Loader::prepare` accepts a validated manifest, the agent's effective grants, the plugin's requested
grants, explicit **binary component bytes**, and host-chosen `Limits`. It hashes and compiles those
same bytes, derives the bounded grants, inspects actual imports/exports, checks signatures, and
returns `Prepared<H>`. It does not execute initialization, load native serialized code or open the
manifest's component path. Artifact location, integrity policy and installation belong to the future
host integration, not to a guest-selectable path in this API.

The host supplies `H: HostServices` for each call. The trait receives an `Invocation` whose principal,
invocation id, bounded grants, cancellation and deadline are assigned by the loader. These are not
fields in guest arguments. Its filesystem implementation must:

1. use those effective grants in the existing policy engine and audit every allow/deny with that
   plugin principal;
2. validate `(root, relative)` paths and enforce the existing requested/resolved symlink checks,
   including absolute-link and escape refusals;
3. enforce `MAX_FILE_BYTES` **while reading**, before allocating a whole large file;
4. honor cancellation and the deadline in native I/O independently of Wasm interruption;
5. return typed guest failures for denials and ordinary file errors, but propagate host failures
   such as a failed audit write as traps.

The library's test host exercises the policy engine and verifies the audit chain. It is a test
fixture, not the production session filesystem adapter. Do not copy it in place of the binary's
existing symlink-safe operations. The loader deliberately does not import or own `Session`.

## Resource and ambient boundaries

Defaults per invocation are 10 million fuel units, a two-second deadline, 16 MiB per linear memory
(at most four), 10,000 elements per table (at most eight tables), at most 32 core instances, and
64 KiB input/output limits. Initialization and `describe` share these bounds too. The epoch clock
runs every 10 ms and remains alive while any prepared plugin needs it.

WASI starts with empty environment/arguments, no initial working directory or preopened descriptors,
closed stdin, discarded stdout/stderr, a 128-entry host resource table and 64 KiB random-byte request
limits. It never inherits the host terminal, sockets or credentials. A bounded debug-log sink is not
implemented yet. Raw sockets, outgoing HTTP and `wasi:io/poll` are not linked. The last is deliberate:
stock synchronous poll could wait forever in native code, beyond fuel/epoch interruption. General
Rust `wasm32-wasip2` components that import it are therefore not supported in this slice. The future
HTTP adapter must use existing policy-controlled networking, not an unrestricted WASI client.

## Build the example

The example is a guest library, not a native executable. With `wasm32-unknown-unknown` installed:

```sh
cargo build -p saddle-sdk --example reader --target wasm32-unknown-unknown
cargo run -p saddle-loader --example componentize -- \
  target/wasm32-unknown-unknown/debug/examples/reader.wasm target/reader.component.wasm
cargo nextest run -p saddle-loader --run-ignored only \
  -E 'test(sdk_reader_component_round_trips_through_the_runtime)'
```

This target avoids depending on unimplemented ambient WASI adapters. `wit-bindgen` embeds the WIT
metadata in the core module; the development-only `componentize` example encodes and validates the
component. It does not install, enable, grant or execute it. The SDK's WIT path currently assumes
this monorepo layout; separately published SDK packages are not part of this first slice. The last
command runs an opt-in Rust SDK round-trip test against that built component, including an allowed
file, an exclusion denial and audit-chain verification. It is ignored in ordinary host test runs so
those do not require a guest target or a previously built artifact.

## Test the boundary

```sh
cargo nextest run -p saddle-cap -p saddle-loader --no-fail-fast
cargo fmt --all --check
cargo clippy --all-targets
```

The runtime suite compiles real binary components from checked-in WAT, so ordinary host tests need
no guest target or downloaded guest binary. It checks refusal before a trapping start function,
manifest and agent ceilings, unknown interfaces/versions/shapes, import/export type mismatches,
allowed and excluded file reads with plugin attribution and a verifiable audit chain, fresh-instance
state, fuel/memory/table limits, deadlines/cancellation, argument/result sizes, native host failures,
empty ambient authority and host resource bounds.

Component input is capped at 4 MiB before compilation. Fuel and epochs bound guest execution, not
the compiler itself; isolating compilation with independent time/memory bounds is still deferred,
and should precede automatic loading of untrusted artifacts.
