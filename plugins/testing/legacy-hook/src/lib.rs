//! Test plugin built against hook world 1.0 (before `extensions`), with
//! that version of the world inlined: the host at a later 1.x minor must
//! still load it and run its callbacks. It adds context `legacy` on
//! `pre_llm_call`.

wit_bindgen::generate!({
    path: ["../../../wit/host"],
    inline: "
        package prometheus:hook@1.0.0;

        interface plugin {
            record hook-event {
                hook: string,
                schema-version: string,
                payload: string,
            }
            record approval {
                message: string,
                rule-key: option<string>,
            }
            variant reply {
                none,
                context(string),
                modify(string),
                block(string),
                approve(approval),
            }
            hooks: func() -> list<string>;
            on-hook: func(event: hook-event) -> result<reply, string>;
        }

        world hook {
            include prometheus:host/imports@1.0.0;
            export plugin;
        }
    ",
    world: "prometheus:hook/hook",
    generate_all,
});

use exports::prometheus::hook::plugin::{Guest, HookEvent, Reply};

struct Legacy;

impl Guest for Legacy {
    fn hooks() -> Vec<String> {
        vec!["pre_llm_call".to_owned()]
    }

    fn on_hook(_event: HookEvent) -> Result<Reply, String> {
        Ok(Reply::Context("legacy".to_owned()))
    }
}

export!(Legacy);
