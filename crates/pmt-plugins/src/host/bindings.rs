//! Host-side bindings of the WIT worlds the runtime instantiates. One
//! combined world gives the host import traits once and a loader per
//! exported plugin interface; a component exports whichever of them it
//! implements, and each loader is looked up on its own.

wasmtime::component::bindgen!({
    path: [
        "../../wit/host",
        "../../wit/messaging",
        "../../wit/subagents",
        "../../wit/credentials",
        "../../wit/tool",
        "../../wit/hook",
        "../../wit/provider",
        "../../wit/terminal-backend",
        "../../wit/delegation-router",
        "../../wit/memory-provider",
        "../../wit/context-engine",
        "../../wit/platform-adapter",
        "../../wit/ui-panel",
    ],
    inline: "
        package prometheus:runtime;
        world plugin-host {
            include prometheus:tool/tool@1.2.0;
            include prometheus:hook/hook@1.3.0;
            include prometheus:provider/provider@1.1.0;
            include prometheus:terminal-backend/terminal-backend@1.2.0;
            include prometheus:delegation-router/delegation-router@1.0.0;
            include prometheus:memory-provider/memory-provider@1.0.0;
            include prometheus:context-engine/context-engine@1.0.0;
            include prometheus:platform-adapter/platform-adapter@1.0.0;
            include prometheus:ui-panel/ui-panel@1.0.0;
        }
    ",
    world: "prometheus:runtime/plugin-host",
});
