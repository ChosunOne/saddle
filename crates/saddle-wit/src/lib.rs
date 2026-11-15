//! canonical component bindings (D11, D12, D127); the guest SDK reads the same WIT files.

wasmtime::component::bindgen!({
    path: "wit",
    world: "read-tool",
    imports: { default: trappable },
});

/// typed harness hooks, separate from plugin-originated messages (D155).
pub mod harness {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "harness-hook",
    });
}
/// typed custom-event handlers (D155).
pub mod events {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "plugin-event-handler",
    });
}
