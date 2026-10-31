//! `prometheus version [--json] [--verbose]`.

use std::fmt::Write as _;

use serde::Serialize;

use crate::build_info;
use crate::surface::SURFACES;

/// The `version --json` document: exactly these keys, in this order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionInfo {
    /// Semantic version.
    pub version: &'static str,
    /// Hash unique to this binary.
    pub build_id: &'static str,
    /// Short git commit.
    pub commit: &'static str,
    /// UTC build time, RFC 3339.
    pub build_date: &'static str,
    /// Target triple.
    pub target: &'static str,
    /// Support tier, `null` for an unsupported target.
    pub tier: Option<u8>,
    /// `stable` or `canary`.
    pub channel: &'static str,
    /// Client protocol `<major>.<minor>`.
    pub rpc_protocol: String,
    /// `config.toml` schema version.
    pub config_schema_version: u32,
    /// Newest `state.db` schema this build supports.
    pub store_schema_version: u32,
    /// Trajectory grammar version.
    pub trajectory_grammar: u32,
}

impl VersionInfo {
    /// This build.
    #[must_use]
    pub fn current() -> Self {
        Self {
            version: build_info::VERSION,
            build_id: build_info::BUILD_ID,
            commit: build_info::COMMIT,
            build_date: build_info::BUILD_DATE,
            target: build_info::TARGET,
            tier: build_info::tier(build_info::TARGET),
            channel: build_info::channel(build_info::VERSION),
            rpc_protocol: pmt_core::protocol::RPC_PROTOCOL.to_string(),
            config_schema_version: pmt_config::SCHEMA_VERSION,
            store_schema_version: pmt_store::SCHEMA_VERSION,
            trajectory_grammar: pmt_obs::GRAMMAR_VERSION,
        }
    }

    /// The text output: the identity line, then build details and the
    /// compiled-in surfaces when verbose.
    #[must_use]
    pub fn render_text(&self, verbose: bool) -> String {
        let mut out = format!("prometheus {}\n", build_info::identity());
        if !verbose {
            return out;
        }
        let tier = self
            .tier
            .map_or_else(|| "unsupported".to_owned(), |t| t.to_string());
        let fields = [
            ("build_id", self.build_id.to_owned()),
            ("build_date", self.build_date.to_owned()),
            ("profile", build_info::PROFILE.to_owned()),
            ("tier", tier),
            ("channel", self.channel.to_owned()),
            ("rpc_protocol", self.rpc_protocol.clone()),
            (
                "config_schema_version",
                self.config_schema_version.to_string(),
            ),
            (
                "store_schema_version",
                self.store_schema_version.to_string(),
            ),
            ("trajectory_grammar", self.trajectory_grammar.to_string()),
        ];
        for (key, value) in fields {
            let _ = writeln!(out, "{key}: {value}");
        }
        out.push_str("surfaces:\n");
        for (name, on) in SURFACES {
            let _ = writeln!(out, "  {name}: {}", if on { "yes" } else { "no" });
        }
        out
    }
}

/// Runs `prometheus version`: the JSON document with `--json`, otherwise
/// the text form. Returns the exit status (74 when stdout is unwritable).
#[must_use]
pub fn run(json: bool, verbose: bool) -> u8 {
    let info = VersionInfo::current();
    let mut stdout = std::io::stdout().lock();
    let written = if json {
        crate::output::write_json(&mut stdout, &info)
    } else {
        std::io::Write::write_all(&mut stdout, info.render_text(verbose).as_bytes())
    };
    match written {
        Ok(()) => pmt_core::exit::SUCCESS,
        Err(_) => pmt_core::ErrorKind::Io.exit_code(),
    }
}
