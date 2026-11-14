//! canonical component bindings (D11, D12, D127); the guest SDK reads the same WIT files.

wasmtime::component::bindgen!({
    path: "wit",
    world: "read-tool",
    imports: { default: trappable },
});
