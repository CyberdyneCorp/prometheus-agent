//! The language server registry (spec: lsp-diagnostics, Built-in server
//! registry; Server overrides and custom servers).
//!
//! Each built-in entry maps file extensions (or file names) to a server,
//! says how it is installed and whether it executes project code (build
//! scripts, project interpreters, compiler plugins), which decides whether
//! it may start in an untrusted workspace. The resolved [`Registry`] of a
//! profile applies `lsp.servers`: custom servers first, then the built-ins
//! that are not disabled, with their overrides.

use std::collections::BTreeMap;
use std::path::Path;

use pmt_config::schema::{LSP_SERVER_IDS, LspConfig, LspServerConfig};
use serde_json::{Map, Value};

/// How a server binary is obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// An npm package (with companion packages) providing `bin`.
    Npm {
        /// `name@version` of the server's package.
        package: &'static str,
        /// Packages installed alongside (the bundled TypeScript).
        extra: &'static [&'static str],
    },
    /// `go install <module>@<version>`.
    Go {
        /// `module@version`.
        module: &'static str,
    },
    /// A pip package in a private virtual environment.
    Pip {
        /// `name==version`.
        package: &'static str,
    },
    /// Installed by the user; `hint` says how.
    Manual {
        /// Where to get it.
        hint: &'static str,
    },
}

impl Install {
    /// `npm`, `go`, `pip` or `manual`.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::Npm { .. } => "npm",
            Self::Go { .. } => "go",
            Self::Pip { .. } => "pip",
            Self::Manual { .. } => "manual",
        }
    }
}

/// How a server that executes no project code is kept that way in an
/// untrusted workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    /// Nothing to pin.
    None,
    /// pyright: the managed Python, never the project's virtualenv.
    ManagedPython,
    /// typescript-language-server: the TypeScript installed with it,
    /// never the workspace's.
    BundledTypeScript,
    /// svelte-language-server: `isTrusted = false`.
    SvelteUntrusted,
}

/// One built-in server.
#[derive(Debug, Clone, Copy)]
pub struct Builtin {
    /// Its id.
    pub id: &'static str,
    /// What it checks.
    pub description: &'static str,
    /// `(".ext" or file name, language id)`.
    pub files: &'static [(&'static str, &'static str)],
    /// Files whose directory is preferred as the server's root (the git
    /// root otherwise).
    pub root_markers: &'static [&'static str],
    /// The executable and its arguments.
    pub command: &'static [&'static str],
    /// How it is installed.
    pub install: Install,
    /// Whether it runs project code.
    pub executes_project_code: bool,
    /// Its untrusted-workspace pin (servers that run no project code).
    pub pin: Pin,
    /// Directories below the user's home its toolchain reads.
    pub toolchain: &'static [&'static str],
    /// Environment pointing its caches into its cache directory
    /// (`(variable, subdirectory)`).
    pub cache_env: &'static [(&'static str, &'static str)],
}

const fn npm(package: &'static str) -> Install {
    Install::Npm {
        package,
        extra: &[],
    }
}

const fn manual(hint: &'static str) -> Install {
    Install::Manual { hint }
}

/// Every built-in server, in match order.
pub const BUILTINS: &[Builtin] = &[
    Builtin {
        id: "rust-analyzer",
        description: "Rust",
        files: &[(".rs", "rust")],
        root_markers: &["Cargo.toml"],
        command: &["rust-analyzer"],
        install: manual("rustup component add rust-analyzer"),
        executes_project_code: true,
        pin: Pin::None,
        toolchain: &[".cargo", ".rustup"],
        cache_env: &[("CARGO_TARGET_DIR", "target")],
    },
    Builtin {
        id: "pyright",
        description: "Python",
        files: &[(".py", "python"), (".pyi", "python")],
        root_markers: &["pyproject.toml", "setup.py", "pyrightconfig.json"],
        command: &["pyright-langserver", "--stdio"],
        install: npm("pyright@1.1.390"),
        executes_project_code: false,
        pin: Pin::ManagedPython,
        toolchain: &[],
        cache_env: &[],
    },
    Builtin {
        id: "typescript-language-server",
        description: "TypeScript and JavaScript",
        files: &[
            (".ts", "typescript"),
            (".mts", "typescript"),
            (".cts", "typescript"),
            (".tsx", "typescriptreact"),
            (".js", "javascript"),
            (".mjs", "javascript"),
            (".cjs", "javascript"),
            (".jsx", "javascriptreact"),
        ],
        root_markers: &["tsconfig.json", "jsconfig.json", "package.json"],
        command: &["typescript-language-server", "--stdio"],
        install: Install::Npm {
            package: "typescript-language-server@4.3.3",
            extra: &["typescript@5.6.3"],
        },
        executes_project_code: false,
        pin: Pin::BundledTypeScript,
        toolchain: &[],
        cache_env: &[],
    },
    Builtin {
        id: "gopls",
        description: "Go",
        files: &[(".go", "go")],
        root_markers: &["go.work", "go.mod"],
        command: &["gopls"],
        install: Install::Go {
            module: "golang.org/x/tools/gopls@v0.16.2",
        },
        executes_project_code: true,
        pin: Pin::None,
        toolchain: &["go", "sdk"],
        cache_env: &[("GOCACHE", "go-build"), ("GOMODCACHE", "go-mod")],
    },
    Builtin {
        id: "clangd",
        description: "C, C++ and Objective-C",
        files: &[
            (".c", "c"),
            (".h", "c"),
            (".cc", "cpp"),
            (".cpp", "cpp"),
            (".cxx", "cpp"),
            (".hpp", "cpp"),
            (".hh", "cpp"),
            (".m", "objective-c"),
            (".mm", "objective-cpp"),
        ],
        root_markers: &["compile_commands.json", "compile_flags.txt", ".clangd"],
        command: &["clangd"],
        install: manual("install clangd from LLVM (https://clangd.llvm.org/installation)"),
        executes_project_code: false,
        pin: Pin::None,
        toolchain: &[],
        cache_env: &[("XDG_CACHE_HOME", "xdg")],
    },
    Builtin {
        id: "bash-language-server",
        description: "Bash",
        files: &[(".sh", "shellscript"), (".bash", "shellscript")],
        root_markers: &[],
        command: &["bash-language-server", "start"],
        install: npm("bash-language-server@5.4.0"),
        executes_project_code: false,
        pin: Pin::None,
        toolchain: &[],
        cache_env: &[],
    },
    Builtin {
        id: "yaml-language-server",
        description: "YAML",
        files: &[(".yaml", "yaml"), (".yml", "yaml")],
        root_markers: &[],
        command: &["yaml-language-server", "--stdio"],
        install: npm("yaml-language-server@1.15.0"),
        executes_project_code: false,
        pin: Pin::None,
        toolchain: &[],
        cache_env: &[],
    },
    Builtin {
        id: "dockerfile-language-server",
        description: "Dockerfiles",
        files: &[("Dockerfile", "dockerfile"), (".dockerfile", "dockerfile")],
        root_markers: &[],
        command: &["docker-langserver", "--stdio"],
        install: npm("dockerfile-language-server-nodejs@0.13.0"),
        executes_project_code: false,
        pin: Pin::None,
        toolchain: &[],
        cache_env: &[],
    },
    Builtin {
        id: "vue-language-server",
        description: "Vue",
        files: &[(".vue", "vue")],
        root_markers: &["package.json"],
        command: &["vue-language-server", "--stdio"],
        install: npm("@vue/language-server@2.1.10"),
        executes_project_code: true,
        pin: Pin::None,
        toolchain: &[],
        cache_env: &[],
    },
    Builtin {
        id: "svelte-language-server",
        description: "Svelte",
        files: &[(".svelte", "svelte")],
        root_markers: &["svelte.config.js", "package.json"],
        command: &["svelteserver", "--stdio"],
        install: npm("svelte-language-server@0.17.8"),
        executes_project_code: false,
        pin: Pin::SvelteUntrusted,
        toolchain: &[],
        cache_env: &[],
    },
    Builtin {
        id: "astro-language-server",
        description: "Astro",
        files: &[(".astro", "astro")],
        root_markers: &["astro.config.mjs", "package.json"],
        command: &["astro-ls", "--stdio"],
        install: npm("@astrojs/language-server@2.15.4"),
        executes_project_code: true,
        pin: Pin::None,
        toolchain: &[],
        cache_env: &[],
    },
    Builtin {
        id: "intelephense",
        description: "PHP",
        files: &[(".php", "php")],
        root_markers: &["composer.json"],
        command: &["intelephense", "--stdio"],
        install: npm("intelephense@1.12.6"),
        executes_project_code: false,
        pin: Pin::None,
        toolchain: &[],
        cache_env: &[],
    },
    Builtin {
        id: "lua-language-server",
        description: "Lua",
        files: &[(".lua", "lua")],
        root_markers: &[".luarc.json", ".luarc.jsonc"],
        command: &["lua-language-server"],
        install: manual("install lua-language-server (https://luals.github.io/#install)"),
        executes_project_code: true,
        pin: Pin::None,
        toolchain: &[],
        cache_env: &[],
    },
    Builtin {
        id: "terraform-ls",
        description: "Terraform",
        files: &[(".tf", "terraform"), (".tfvars", "terraform-vars")],
        root_markers: &[".terraform"],
        command: &["terraform-ls", "serve"],
        install: manual("install terraform-ls (https://github.com/hashicorp/terraform-ls)"),
        executes_project_code: true,
        pin: Pin::None,
        toolchain: &[".terraform.d"],
        cache_env: &[],
    },
    Builtin {
        id: "jdtls",
        description: "Java",
        files: &[(".java", "java")],
        root_markers: &["pom.xml", "build.gradle", "build.gradle.kts"],
        command: &["jdtls"],
        install: manual(
            "install Eclipse JDT Language Server (https://github.com/eclipse-jdtls/eclipse.jdt.ls)",
        ),
        executes_project_code: true,
        pin: Pin::None,
        toolchain: &[".m2", ".gradle"],
        cache_env: &[],
    },
    Builtin {
        id: "kotlin-language-server",
        description: "Kotlin",
        files: &[(".kt", "kotlin"), (".kts", "kotlin")],
        root_markers: &["build.gradle.kts", "build.gradle", "pom.xml"],
        command: &["kotlin-language-server"],
        install: manual(
            "install kotlin-language-server (https://github.com/fwcd/kotlin-language-server)",
        ),
        executes_project_code: true,
        pin: Pin::None,
        toolchain: &[".m2", ".gradle"],
        cache_env: &[],
    },
    Builtin {
        id: "elixir-ls",
        description: "Elixir",
        files: &[(".ex", "elixir"), (".exs", "elixir")],
        root_markers: &["mix.exs"],
        command: &["elixir-ls"],
        install: manual("install ElixirLS (https://github.com/elixir-lsp/elixir-ls)"),
        executes_project_code: true,
        pin: Pin::None,
        toolchain: &[".mix", ".hex"],
        cache_env: &[],
    },
    Builtin {
        id: "zls",
        description: "Zig",
        files: &[(".zig", "zig")],
        root_markers: &["build.zig"],
        command: &["zls"],
        install: manual("install zls (https://github.com/zigtools/zls)"),
        executes_project_code: true,
        pin: Pin::None,
        toolchain: &[],
        cache_env: &[("XDG_CACHE_HOME", "xdg")],
    },
    Builtin {
        id: "clojure-lsp",
        description: "Clojure",
        files: &[
            (".clj", "clojure"),
            (".cljs", "clojure"),
            (".cljc", "clojure"),
            (".edn", "clojure"),
        ],
        root_markers: &["deps.edn", "project.clj"],
        command: &["clojure-lsp"],
        install: manual("install clojure-lsp (https://clojure-lsp.io/installation/)"),
        executes_project_code: true,
        pin: Pin::None,
        toolchain: &[".m2"],
        cache_env: &[],
    },
    Builtin {
        id: "haskell-language-server",
        description: "Haskell",
        files: &[(".hs", "haskell"), (".lhs", "haskell")],
        root_markers: &["stack.yaml", "cabal.project", "package.yaml"],
        command: &["haskell-language-server-wrapper", "--lsp"],
        install: manual(
            "install haskell-language-server with ghcup (https://www.haskell.org/ghcup/)",
        ),
        executes_project_code: true,
        pin: Pin::None,
        toolchain: &[".ghcup", ".cabal", ".stack"],
        cache_env: &[("XDG_CACHE_HOME", "xdg")],
    },
    Builtin {
        id: "ocaml-lsp",
        description: "OCaml",
        files: &[(".ml", "ocaml"), (".mli", "ocaml")],
        root_markers: &["dune-project", "dune-workspace"],
        command: &["ocamllsp"],
        install: manual("opam install ocaml-lsp-server"),
        executes_project_code: true,
        pin: Pin::None,
        toolchain: &[".opam"],
        cache_env: &[],
    },
    Builtin {
        id: "prisma-language-server",
        description: "Prisma schemas",
        files: &[(".prisma", "prisma")],
        root_markers: &["package.json"],
        command: &["prisma-language-server", "--stdio"],
        install: npm("@prisma/language-server@5.22.0"),
        executes_project_code: false,
        pin: Pin::None,
        toolchain: &[],
        cache_env: &[],
    },
    Builtin {
        id: "cmake-language-server",
        description: "CMake",
        files: &[("CMakeLists.txt", "cmake"), (".cmake", "cmake")],
        root_markers: &[],
        command: &["cmake-language-server"],
        install: Install::Pip {
            package: "cmake-language-server==0.1.10",
        },
        executes_project_code: false,
        pin: Pin::None,
        toolchain: &[],
        cache_env: &[],
    },
];

/// A server of the resolved registry.
#[derive(Debug, Clone)]
pub struct Server {
    /// Its id.
    pub id: String,
    /// What it checks.
    pub description: String,
    /// `(".ext" or file name, language id)`.
    pub files: Vec<(String, String)>,
    /// Preferred root markers.
    pub root_markers: Vec<String>,
    /// The argv: the override's, else the built-in default.
    pub command: Vec<String>,
    /// Whether `command` came from `lsp.servers.<id>.command` (no install).
    pub command_overridden: bool,
    /// How it is installed (custom servers: manual).
    pub install: Install,
    /// Whether it runs project code (custom servers always do).
    pub executes_project_code: bool,
    /// Its untrusted-workspace pin.
    pub pin: Pin,
    /// Toolchain directories below the user's home.
    pub toolchain: Vec<String>,
    /// Cache environment (`(variable, subdirectory)`).
    pub cache_env: Vec<(String, String)>,
    /// `lsp.servers.<id>.env`.
    pub env: BTreeMap<String, String>,
    /// `lsp.servers.<id>.initialization_options`.
    pub initialization_options: Map<String, Value>,
    /// Declared under `lsp.servers` rather than built in.
    pub custom: bool,
}

impl Server {
    fn builtin(builtin: &Builtin, overrides: Option<&LspServerConfig>) -> Self {
        let owned = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let command = overrides.and_then(|o| o.command.clone());
        Self {
            id: builtin.id.to_owned(),
            description: builtin.description.to_owned(),
            files: builtin
                .files
                .iter()
                .map(|(p, l)| ((*p).to_owned(), (*l).to_owned()))
                .collect(),
            root_markers: owned(builtin.root_markers),
            command_overridden: command.is_some(),
            command: command.unwrap_or_else(|| owned(builtin.command)),
            install: builtin.install,
            executes_project_code: builtin.executes_project_code,
            pin: builtin.pin,
            toolchain: owned(builtin.toolchain),
            cache_env: builtin
                .cache_env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            env: overrides.map(|o| o.env.clone()).unwrap_or_default(),
            initialization_options: overrides
                .map(|o| o.initialization_options.clone())
                .unwrap_or_default(),
            custom: false,
        }
    }

    fn custom(id: &str, config: &LspServerConfig) -> Self {
        let files = config
            .extensions
            .iter()
            .map(|ext| {
                let language = config
                    .language_id
                    .clone()
                    .unwrap_or_else(|| ext.trim_start_matches('.').to_owned());
                (ext.clone(), language)
            })
            .collect();
        Self {
            id: id.to_owned(),
            description: config
                .description
                .clone()
                .unwrap_or_else(|| "custom server".to_owned()),
            files,
            root_markers: config.root_markers.clone(),
            command: config.command.clone().unwrap_or_default(),
            command_overridden: true,
            install: Install::Manual {
                hint: "a custom server: install it yourself",
            },
            executes_project_code: true,
            pin: Pin::None,
            toolchain: Vec::new(),
            cache_env: Vec::new(),
            env: config.env.clone(),
            initialization_options: config.initialization_options.clone(),
            custom: true,
        }
    }

    /// The language id of `path`, when this server checks it.
    #[must_use]
    pub fn language_of(&self, path: &Path) -> Option<&str> {
        let name = path.file_name()?.to_string_lossy();
        let lower = name.to_ascii_lowercase();
        self.files
            .iter()
            .find(|(pattern, _)| {
                if pattern.starts_with('.') {
                    lower.ends_with(&pattern.to_ascii_lowercase()) && lower.len() > pattern.len()
                } else {
                    name == pattern.as_str()
                }
            })
            .map(|(_, language)| language.as_str())
    }

    /// The executable name the server is looked up by.
    #[must_use]
    pub fn program(&self) -> &str {
        self.command.first().map_or("", String::as_str)
    }
}

/// A profile's servers in match order.
#[derive(Debug, Clone)]
pub struct Registry {
    servers: Vec<Server>,
    disabled: Vec<String>,
}

impl Registry {
    /// The registry `config` resolves to (validated: unknown keys were
    /// refused at load).
    #[must_use]
    pub fn new(config: &LspConfig) -> Self {
        let mut servers: Vec<Server> = config
            .servers
            .iter()
            .filter(|(id, entry)| !LSP_SERVER_IDS.contains(&id.as_str()) && !entry.disabled)
            .map(|(id, entry)| Server::custom(id, entry))
            .collect();
        let mut disabled = Vec::new();
        for builtin in BUILTINS {
            let overrides = config.servers.get(builtin.id);
            if overrides.is_some_and(|o| o.disabled) {
                disabled.push(builtin.id.to_owned());
                continue;
            }
            servers.push(Server::builtin(builtin, overrides));
        }
        Self { servers, disabled }
    }

    /// The server that checks `path`: the first whose files match.
    #[must_use]
    pub fn for_file(&self, path: &Path) -> Option<(&Server, &str)> {
        self.servers
            .iter()
            .find_map(|server| server.language_of(path).map(|language| (server, language)))
    }

    /// The server `id`, unless unknown or disabled.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Server> {
        self.servers.iter().find(|s| s.id == id)
    }

    /// Every enabled server.
    #[must_use]
    pub fn servers(&self) -> &[Server] {
        &self.servers
    }

    /// Whether `id` is a built-in disabled by `lsp.servers.<id>.disabled`.
    #[must_use]
    pub fn is_disabled(&self, id: &str) -> bool {
        self.disabled.iter().any(|d| d == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtins_are_the_config_ids() {
        let ids: Vec<&str> = BUILTINS.iter().map(|b| b.id).collect();
        assert_eq!(ids, LSP_SERVER_IDS);
    }

    #[test]
    fn a_rust_file_goes_to_rust_analyzer() {
        let registry = Registry::new(&LspConfig::default());
        let (server, language) = registry.for_file(Path::new("/w/src/lib.rs")).unwrap();
        assert_eq!((server.id.as_str(), language), ("rust-analyzer", "rust"));
        assert!(server.executes_project_code);
        let (docker, _) = registry.for_file(Path::new("/w/Dockerfile")).unwrap();
        assert_eq!(docker.id, "dockerfile-language-server");
        assert!(registry.for_file(Path::new("/w/notes.txt")).is_none());
        assert!(registry.for_file(Path::new("/w/.rs")).is_none());
    }

    #[test]
    fn a_custom_server_matches_before_the_builtins_and_runs_project_code() {
        let config: LspConfig = toml::from_str(
            "[servers.mylang]\ncommand = [\"mylang-ls\", \"--stdio\"]\nextensions = [\".ml2\", \".py\"]\n\
             [servers.gopls]\ndisabled = true\n\
             [servers.pyright]\ncommand = [\"/opt/pyright\", \"--stdio\"]\n",
        )
        .unwrap();
        let registry = Registry::new(&config);
        let (server, language) = registry.for_file(Path::new("a.ml2")).unwrap();
        assert_eq!(server.command, ["mylang-ls", "--stdio"]);
        assert_eq!(language, "ml2");
        assert!(server.custom && server.executes_project_code);
        assert_eq!(registry.for_file(Path::new("a.py")).unwrap().0.id, "mylang");
        assert!(registry.get("gopls").is_none() && registry.is_disabled("gopls"));
        let pyright = registry.get("pyright").unwrap();
        assert!(pyright.command_overridden);
        assert_eq!(pyright.program(), "/opt/pyright");
    }

    #[test]
    fn every_builtin_declares_an_install_method() {
        for builtin in BUILTINS {
            assert!(
                ["npm", "go", "pip", "manual"].contains(&builtin.install.method()),
                "{}",
                builtin.id
            );
            assert!(!builtin.command.is_empty() && !builtin.files.is_empty());
            // A server that runs project code needs no pin: it never starts
            // untrusted.
            if builtin.executes_project_code {
                assert_eq!(builtin.pin, Pin::None, "{}", builtin.id);
            }
        }
    }
}
