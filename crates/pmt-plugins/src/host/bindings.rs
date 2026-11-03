//! Host-side bindings of the WIT worlds the runtime instantiates. One
//! combined world gives the host import traits once and a loader per
//! exported plugin interface; a component exports whichever of them it
//! implements, and each loader is looked up on its own.

wasmtime::component::bindgen!({
    path: ["../../wit/host", "../../wit/tool", "../../wit/hook", "../../wit/provider"],
    inline: "
        package prometheus:runtime;
        world plugin-host {
            include prometheus:tool/tool@1.0.0;
            include prometheus:hook/hook@1.1.0;
            include prometheus:provider/provider@1.0.0;
        }
    ",
    world: "prometheus:runtime/plugin-host",
});
