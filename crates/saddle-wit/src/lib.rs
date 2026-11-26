wasmtime::component::bindgen!({
    path: "wit",
    world: "read-tool",
    imports: { default: trappable },
});

pub mod harness {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "harness-hook",
    });
}
pub mod events {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "plugin-event-handler",
    });
}

pub mod veto {
    wasmtime::component::bindgen!({ path: "wit", world: "veto-hook" });
}
pub mod ui {
    wasmtime::component::bindgen!({ path: "wit", world: "ui-hook", with: { "saddle:widgets/types@0.1.0": crate::saddle::widgets::types } });
}
