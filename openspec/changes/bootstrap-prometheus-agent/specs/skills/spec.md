# Skills

## Purpose
Skills are the agent's procedural memory: directories holding an agentskills.io-compatible `SKILL.md` (YAML frontmatter + Markdown body) plus optional `references/`, `templates/`, `scripts/`, `assets/` and `evals/`. They are resolved from tiered roots through one shared precedence order, advertised to the model as a compact index frozen in the session's cached system prompt, loaded on demand through `skill_view`, written by the agent through `skill_manage`, installed from hubs with scanning, lockfile pinning and signature verification, and kept healthy by a background curator. The key decisions are that skills (not new core tools) are the preferred extension surface, that skill changes never mutate a live session's prompt, and that every skill carries verifiable provenance and may carry executable evals.

## ADDED Requirements

### Requirement: SKILL.md format and agentskills.io compatibility
The system SHALL accept a `SKILL.md` that starts with a `---` line (a leading UTF-8 BOM tolerated), contains a YAML frontmatter mapping closed by a `---` line, and SHALL require the agentskills.io fields `name` (1-64 characters matching `^[a-z0-9][a-z0-9._-]*$`) and `description` (1-1024 characters), passing through the optional agentskills.io fields `license`, `compatibility`, `allowed-tools` and `metadata` unchanged. Prometheus-specific fields (`tags`, `category`, `related_skills`, `platforms`, `requires_toolsets`, `requires_tools`, `fallback_for_toolsets`, `fallback_for_tools`, `session_platforms`, `config`, `secrets`, `blueprint`, `python`) SHALL be read from `metadata.prometheus.*`, then from `metadata.hermes.*`, then from the top-level key, first match wins. A `SKILL.md` whose frontmatter is not valid YAML or lacks a required field SHALL NOT be loaded and SHALL be reported by `prometheus skills doctor` with the file path, line and reason.
**Milestone:** M1

#### Scenario: Hermes skill loads unchanged
- **GIVEN** a Hermes skill whose frontmatter has `name: axolotl`, `description: Fine-tune LLMs with Axolotl.` and `metadata.hermes.tags: [mlops]`
- **WHEN** the skill roots are scanned
- **THEN** the system SHALL load the skill with tags `[mlops]` without modifying the file

#### Scenario: Malformed frontmatter is reported, not guessed
- **GIVEN** a `SKILL.md` whose frontmatter contains an unclosed YAML flow sequence
- **WHEN** the skill roots are scanned
- **THEN** the system SHALL omit the skill from every listing and `prometheus skills doctor` SHALL print the path, the line number and `invalid frontmatter`

### Requirement: Skill directory layout and scanner exclusions
The system SHALL treat a directory as a skill only when it directly contains `SKILL.md`, SHALL treat `references/`, `templates/`, `scripts/`, `assets/` and `evals/` inside it as supporting directories (a `SKILL.md` inside them is never a standalone skill), and SHALL skip `.git`, `.github`, `.archive`, `.backups`, `.locks`, `node_modules`, `site-packages`, virtualenv and tool-cache directories during scans.
**Milestone:** M1

#### Scenario: Nested SKILL.md in references is ignored
- **GIVEN** skill `foo` containing `references/example/SKILL.md`
- **WHEN** the skill roots are scanned
- **THEN** the system SHALL register only `foo` and not `example`

### Requirement: Search roots and single precedence order
The system SHALL resolve skills across tiers in exactly one precedence order shared by the prompt index, `skills_list`, `skill_view`, slash commands, preload, cron and `prometheus skills list`: trusted project skills, then the profile skills directory `<profile_home>/skills/`, then each entry of `skills.external_dirs` (TOML list, default `[]`, `~` expanded, non-existent entries skipped with a WARN), then plugin-provided skills (namespaced `<plugin>:<skill>`), then bundled skills; the first tier that defines a name SHALL win, and two different skills sharing a name within one tier SHALL be refused with an error starting `Ambiguous skill name '<name>'` that lists both paths.
**Milestone:** M1

#### Scenario: Profile skill shadows bundled skill
- **GIVEN** a bundled skill `git-workflow` and `<profile_home>/skills/dev/git-workflow/SKILL.md`
- **WHEN** `skill_view(name="git-workflow")` is called
- **THEN** the system SHALL return the profile copy and `prometheus skills list` SHALL mark the bundled copy as shadowed

#### Scenario: Same-tier collision is refused
- **GIVEN** two different skills both named `deploy` under `<profile_home>/skills/a/` and `<profile_home>/skills/b/`
- **WHEN** `skill_view(name="deploy")` is called
- **THEN** the system SHALL return an error starting `Ambiguous skill name 'deploy'` and SHALL NOT pick either

### Requirement: Bundled skills are read-only and ejectable (upgrade)
The system SHALL serve bundled skills read-only from the binary's embedded skill tree without copying them into the profile, SHALL provide `prometheus skills eject <name>` which copies a bundled skill into `<profile_home>/skills/<category>/<name>/` and records its origin digest so it shadows the bundled copy, and SHALL report in `prometheus skills list --outdated` every ejected skill whose bundled origin changed in a newer release, offering `prometheus skills diff <name>` and `prometheus skills reset <name>`.
**Milestone:** M1

#### Scenario: Upgrade never overwrites a user edit
- **GIVEN** an ejected and user-edited skill `git-workflow` and a new release whose bundled `git-workflow` changed
- **WHEN** the user upgrades Prometheus and starts a session
- **THEN** the system SHALL use the user's copy unchanged and `prometheus skills list --outdated` SHALL list `git-workflow`

#### Scenario: Deleted ejected copy falls back to bundled
- **GIVEN** an ejected skill whose profile directory the user deleted
- **WHEN** a new session starts
- **THEN** the system SHALL resolve the name to the bundled copy

### Requirement: Project-local skills require explicit trust
The system SHALL, when the working directory is inside a git checkout (nearest ancestor containing `.git`, never the user's home directory, walk bounded at 64 levels), load `<root>/.prometheus/skills/` and `<root>/.agents/skills/` as the highest-precedence tier only when `<root>` is listed in `skills.trusted_project_dirs`; for an untrusted root containing skills it SHALL load none and SHALL emit one notice naming the count and the command `prometheus skills trust <root>`; `skills.project_discovery = false` SHALL disable both the scan and the notice; a trusted project skill whose security scan is `dangerous` or whose scan fails SHALL be quarantined.
**Milestone:** M1

#### Scenario: Untrusted repository skills do not load
- **GIVEN** a cloned repository with `.prometheus/skills/evil/SKILL.md` and the root not in `skills.trusted_project_dirs`
- **WHEN** a session starts in that repository
- **THEN** the system SHALL NOT index `evil` and SHALL show `1 project skill not loaded: run prometheus skills trust <root>`

#### Scenario: Scanner failure quarantines a trusted project skill
- **GIVEN** a trusted project root and a skill whose security scan raises an error
- **WHEN** the skill roots are scanned
- **THEN** the system SHALL quarantine the skill and SHALL NOT index it

### Requirement: Platform, environment and conditional gates
The system SHALL hide a skill from the prompt index, `skills_list` and slash commands when its `platforms` list (values `macos`, `linux`, `windows`, `android`; absent or empty means all) excludes the host OS, and `skill_view` SHALL fail with `Skill '<name>' is not supported on this platform.`; the prompt index SHALL additionally hide a skill when any `requires_toolsets`/`requires_tools` entry is unavailable in the session, when any `fallback_for_toolsets`/`fallback_for_tools` entry is available, or when `session_platforms` is set and the session's surface or adapter is not in it, and an unknown session platform SHALL show the skill.
**Milestone:** M1

#### Scenario: Fallback skill hidden when primary tool exists
- **GIVEN** skill `curl-search` with `fallback_for_tools: [web_search]` and a session where `web_search` is available
- **WHEN** the system prompt is built
- **THEN** the index SHALL NOT list `curl-search`

### Requirement: Disabled skills
The system SHALL treat as disabled the union of `skills.disabled` (list of names) and `skills.platform_disabled.<platform>` for the session's surface or adapter, SHALL refuse a config that disables the essential skill `prometheus-agent` with a validation error, SHALL omit disabled skills from listings, the index and slash commands, and `skill_view` SHALL fail with `Skill '<name>' is disabled. Enable it with prometheus skills enable <name>.`.
**Milestone:** M1

#### Scenario: Disabling the essential skill fails validation
- **GIVEN** `skills.disabled = ["prometheus-agent"]` in `config.toml`
- **WHEN** the config is loaded
- **THEN** the system SHALL fail config validation naming `skills.disabled` and `prometheus-agent`

### Requirement: Skills index in the system prompt
The system SHALL render visible skills as a `## Skills` section containing an `<available_skills>` block grouped by category sorted by name, one row `- <name>: <description>` per skill with the description clipped at a word boundary to `skills.index_description_chars` (default 60) characters, SHALL collapse categories demoted by the session posture into one `<category> [names only]: a, b, c` line rather than drop them, and SHALL render an empty index for a safe-worker session.
**Milestone:** M1

#### Scenario: Long description is clipped
- **GIVEN** a skill whose description is 140 characters long
- **WHEN** the system prompt is built
- **THEN** its index row description SHALL be at most 60 characters and end at a word boundary

### Requirement: Prompt-cache preservation for skill changes
The system SHALL keep the skills index bytes in a session's system prompt fixed for the life of that session; a skill created, installed, edited (name or description), enabled, disabled, archived or removed after the prompt was built SHALL reach the model only as a per-turn note on the next user message beginning `[System: The skills available in this conversation changed after its system prompt was built.` that states the full difference against the prompt, sent only when that difference changes, and SHALL be reflected in the index of the next session.
**Milestone:** M1

#### Scenario: Agent-created skill does not rebuild the prompt
- **GIVEN** a session whose system prompt hash is `H`
- **WHEN** the agent creates skill `new-thing` with `skill_manage` and the user sends another message
- **THEN** the system prompt hash SHALL still be `H` and the user message SHALL carry the change note naming `new-thing`

### Requirement: Deferred versus immediate activation of skill changes
`/skills install`, `/skills uninstall`, `/skills enable`, `/skills disable` and `/skills reset` SHALL treat the typed command as consent, SHALL by default defer the change to the next session and print `Skill change takes effect in your next session. Use /new to start one now, or repeat with --now to apply immediately (invalidates prompt cache).`, and with `--now` SHALL rebuild the current session's skills index and record a `prompt.rebuilt` event with `reason` `skills --now`.
**Milestone:** M1

#### Scenario: --now invalidates once and records it
- **GIVEN** an active session
- **WHEN** the user runs `/skills install official/devops/terraform --now`
- **THEN** the system SHALL rebuild the index in the current session and the trajectory SHALL contain one `prompt.rebuilt` event with `reason` `skills --now`

### Requirement: skills_list and skill_view tools
The `skills` toolset SHALL register `skills_list` (optional `category` filter; returns rows `{name, description, category, source}` plus `categories` and `count`) and `skill_view` (`name` required, optional `file_path`); `skill_view` SHALL return the processed SKILL.md body plus a `linked_files` map of supporting files, SHALL accept bare names, categorized paths (`mlops/axolotl`) and plugin-qualified names (`<plugin>:<skill>`), SHALL serve a supporting file only when its canonical path (symlinks resolved) lies inside the skill directory, and no skill-loading tool SHALL accept `offset` or `limit` parameters.
**Milestone:** M1

#### Scenario: Path traversal through a symlink is refused
- **GIVEN** skill `foo` with `references/leak` symlinked to `/etc/passwd`
- **WHEN** `skill_view(name="foo", file_path="references/leak")` is called
- **THEN** the system SHALL refuse with `file is outside the skill directory` and return no content

### Requirement: Load-time template substitution and inline shell
When a skill is loaded the system SHALL substitute `${PROMETHEUS_SKILL_DIR}`, `${PROMETHEUS_SESSION_ID}` and the aliases `${HERMES_SKILL_DIR}`, `${HERMES_SESSION_ID}` (an unknown token left in place) unless `skills.template_vars = false`, and SHALL expand inline `` !`cmd` `` snippets only when `skills.inline_shell = true` (default `false`), running each through the session's execution sandbox and approval policy with the skill directory as working directory, a `skills.inline_shell_timeout_ms` (default 10000) deadline and output capped at 4000 characters, rendering a failure as `[inline-shell error: <reason>]`; inline shell SHALL never expand for a skill whose trust level is `community` or whose provenance cannot be read.
**Milestone:** M1

#### Scenario: Community skill never runs inline shell
- **GIVEN** `skills.inline_shell = true` and a hub-installed community skill containing `` !`whoami` ``
- **WHEN** the skill is loaded
- **THEN** the system SHALL leave the snippet unexpanded and SHALL NOT execute it

### Requirement: Skill secrets and settings
The system SHALL read a skill's declared secrets from `metadata.prometheus.secrets` (alias `required_environment_variables`, legacy `prerequisites.env_vars`) without hiding the skill when they are unset; missing secrets SHALL be collected only through the local secure prompt (never through the model) and stored in the profile secrets store; while the skill is loaded in a session the system SHALL pass only those declared secrets into the session's terminal and code-execution sandboxes; non-secret `config` entries SHALL be stored under `skills.config.<skill>.<key>` in `config.toml` and injected into the skill body at load, and a value declared as non-secret SHALL never be written to the secrets store.
**Milestone:** M1

#### Scenario: Secret value never reaches the transcript
- **GIVEN** skill `notion` declaring secret `NOTION_API_KEY` that is unset
- **WHEN** the skill is loaded in an interactive CLI session
- **THEN** the system SHALL prompt the user locally with input hidden, store the value in the secrets store, and the transcript SHALL contain only the variable name

### Requirement: Skill scripts run sandboxed with managed runtimes
The system SHALL execute files under a skill's `scripts/` only through the session's terminal or code-execution tools under the session sandbox and approval policy, never implicitly at load; a skill declaring `metadata.prometheus.python` (`requires_python`, `requirements`) SHALL have `python`/`python3` in those invocations resolved to the python-bridge managed interpreter and a per-skill environment built from its pinned requirements, and a skill declaring `metadata.prometheus.node` SHALL resolve `node` to the managed Node runtime.
**Milestone:** M2

#### Scenario: Skill script gets its pinned environment
- **GIVEN** skill `pdf-tools` declaring `python.requirements = ["pypdf>=4,<5"]`
- **WHEN** the agent runs `python ${PROMETHEUS_SKILL_DIR}/scripts/extract.py` via the terminal tool
- **THEN** the system SHALL run it with the managed interpreter in an environment where `pypdf` 4.x is importable, inside the session sandbox

### Requirement: Skill slash commands
The system SHALL expose each enabled, gate-passing skill as `/<slug>` where the slug lowercases the name and maps spaces and underscores to hyphens, SHALL apply the search-root precedence on slug collisions, SHALL NOT register a slug equal to a core command name or alias, and SHALL show `slash command /<slug> unavailable: name taken by a built-in command; use /skill <name>` for such a skill in `/skills` listings.
**Milestone:** M1

#### Scenario: Built-in name wins
- **GIVEN** a skill named `help`
- **WHEN** slash commands are registered
- **THEN** `/help` SHALL remain the core command and `/skill help` SHALL load the skill

### Requirement: skill_manage tool validation
The `skill_manage` tool SHALL support exactly the actions `create`, `edit`, `patch`, `delete`, `write_file` and `remove_file`, and SHALL enforce: names and categories at most 64 characters matching `^[a-z0-9][a-z0-9._-]*$` (a category is one directory name); SKILL.md starting with valid frontmatter including `name` and `description` and a non-empty body; SKILL.md at most 100,000 characters; supporting files at most 1 MiB and only under `references/`, `templates/`, `scripts/`, `assets/` or `evals/` with no `..` component; an unknown action SHALL return `Unknown action '<action>'. Use: create, edit, patch, delete, write_file, remove_file`.
**Milestone:** M1

#### Scenario: Supporting file outside allowed dirs is refused
- **GIVEN** an existing agent skill `foo`
- **WHEN** `skill_manage(action="write_file", name="foo", file_path="../bar/x.md", ...)` is called
- **THEN** the system SHALL refuse and write nothing

### Requirement: Description budget and advisory lint on agent writes
A `skill_manage` write that creates or changes a SKILL.md description SHALL be rejected when the new description exceeds `skills.index_description_chars` (default 60), while an unchanged pre-existing longer description SHALL NOT block body edits; every successful write SHALL return `lint_warnings` containing only the lint findings that write introduced, and lint findings SHALL never block a write.
**Milestone:** M1

#### Scenario: Body patch on a legacy skill succeeds
- **GIVEN** an imported skill with a 90-character description
- **WHEN** the agent patches only its body
- **THEN** the system SHALL apply the patch and SHALL NOT report a description error

### Requirement: Skill mutation safety and fail-closed scanning
Every `skill_manage` mutation SHALL hold a per-skill exclusive lock, SHALL write atomically (temp file + rename), and SHALL security-scan the resulting skill; a `dangerous` verdict or a scanner error SHALL restore the previous content (or remove the new file) and return the verdict, so scanning fails closed. Deletion SHALL refuse symlinked skill directories, paths outside the profile skills directory and `skills.create_dir`, bundled and hub-installed skills, the essential `prometheus-agent` skill, and pinned skills (with `Skill '<name>' is pinned and cannot be deleted.`). With `skills.write_approval = true` (default `false`) mutations SHALL be staged as pending records reviewed with `/skills pending`, `/skills approve <id>` and `/skills reject <id>`.
**Milestone:** M1

#### Scenario: Scanner crash does not let a write through
- **GIVEN** the security scanner raises an internal error
- **WHEN** the agent creates a skill with `skill_manage`
- **THEN** the system SHALL NOT persist the skill and SHALL return an error naming the scan failure

### Requirement: Skill lookup and wrong-profile hints
`skill_manage` and `skill_view` SHALL locate a skill by directory name, then categorized relative path, then unique frontmatter `name` (a name claimed by two skills resolves to nothing), and a not-found error SHALL read `Skill '<name>' not found in profile '<profile>'.` followed by either `It exists in profile '<other>' (prometheus -p <other>).` when another profile on the same home holds it, or `Use skills_list() to see available skills.`.
**Milestone:** M1

#### Scenario: Skill in another profile is pointed to
- **GIVEN** skill `trading` exists only in profile `work`
- **WHEN** `skill_view(name="trading")` runs in profile `default`
- **THEN** the error SHALL name profile `work` and `prometheus -p work`

### Requirement: Autonomous skill creation and improvement
The system SHALL count tool-calling iterations within a session while `skill_manage` is available and, when a completed, non-interrupted turn with a final response reaches `skills.creation_nudge_interval` iterations (default 10, defined in the config schema; 0 disables), SHALL reset the counter and run a background review after the response is delivered that may create or patch skills; background review SHALL NOT run for cron, delegated child or harness-evolution sessions; inside a background review, overwriting an existing file SHALL be refused unless that file was read with `skill_view` in the review, deletes SHALL archive instead of removing, and deletes of pinned, external-dir, bundled, plugin and hub skills SHALL be refused.
**Milestone:** M1

#### Scenario: Review cannot clobber unseen content
- **GIVEN** a background review that has not called `skill_view` on skill `foo`
- **WHEN** it calls `skill_manage(action="edit", name="foo", ...)`
- **THEN** the system SHALL refuse with an instruction to view the skill first

### Requirement: Hub install pipeline and trust policy
`prometheus skills install <identifier>` SHALL fetch the bundle into a quarantine directory, security-scan it, verify its signature when present, and decide by trust level: `builtin` (official optional skills) allows every verdict, `trusted` (sources in `skills.trusted_sources`, default `["anthropics/skills", "openai/skills"]`) allows `safe` and `caution`, and `community` allows only `safe`; `--force` SHALL override a block except a `dangerous` verdict on a `trusted` or `community` source, which SHALL print `--force does not override a dangerous verdict.`; reinstalling an installed skill without `--force` SHALL exit 1 with `Already installed. Use --force to reinstall.`.
**Milestone:** M2

#### Scenario: Dangerous community skill is blocked even with --force
- **GIVEN** a community skill whose scan verdict is `dangerous`
- **WHEN** the user runs `prometheus skills install someone/skills/x --force`
- **THEN** the system SHALL exit non-zero, print `--force does not override a dangerous verdict.` and leave nothing installed

### Requirement: Skills lockfile and tamper detection (upgrade)
The system SHALL record every hub-installed or ejected skill in `<profile_home>/skills/skills.lock` (TOML) with `name`, `source`, `resolved_ref` (full commit SHA or content URL), `content_digest` (SHA-256 over the canonical file tree), `trust`, `signature` (`verified:<identity>`, `unsigned` or `invalid`) and `installed_at`; at index build the system SHALL recompute the digest of each hub-installed skill and SHALL quarantine one whose digest differs until the user runs `prometheus skills accept <name>` (which re-scans and re-records) or `prometheus skills reset <name>`; `prometheus skills install --locked` SHALL reproduce exactly the locked set and fail on any digest mismatch.
**Milestone:** M2

#### Scenario: Silent modification of a hub skill is caught
- **GIVEN** hub skill `terraform` locked with digest `D1` and a file in it modified on disk so its digest is `D2`
- **WHEN** a new session starts
- **THEN** the system SHALL exclude `terraform` from the index and `prometheus skills doctor` SHALL report `content digest mismatch`

### Requirement: Skill signatures and provenance (upgrade)
The system SHALL verify a detached signature (`SKILL.sig` over the canonical tree digest, minisign key or Sigstore keyless identity) when present, SHALL apply `skills.signature_policy` (`off`, `warn` default, `require`) to hub installs, where `require` refuses unsigned or invalidly signed skills with exit code 77 and `warn` installs unsigned skills with a WARN while always refusing an invalid signature, SHALL provide `prometheus skills sign <dir> [--key <path>|--keyless]` producing `SKILL.sig`, and SHALL show provenance (source, ref, digest, signer, trust) in `prometheus skills show <name>`.
**Milestone:** M2

#### Scenario: Invalid signature is never accepted
- **GIVEN** `skills.signature_policy = "warn"` and a skill whose `SKILL.sig` does not match its content
- **WHEN** it is installed
- **THEN** the system SHALL refuse the install and print `invalid signature`

### Requirement: Blueprint skills never auto-schedule
A skill carrying a `blueprint` block SHALL, on install, be registered only as a suggested scheduled job listed by `/suggestions` and `prometheus cron suggestions`, and SHALL NOT create a scheduled job until the user accepts it.
**Milestone:** M2

#### Scenario: Install does not create a job
- **GIVEN** a skill with a daily-digest blueprint
- **WHEN** it is installed
- **THEN** `prometheus cron list` SHALL show no new job and `/suggestions` SHALL list the blueprint

### Requirement: Skill usage records
The system SHALL record per-skill usage in the profile `state.db` table `skill_usage` (`name`, `use_count`, `view_count`, `patch_count`, `last_activity_at`, `state` in `active|stale|archived`, `pinned`, `created_by` in `user|agent|hub|bundled`), updating it transactionally on `skill_view`, slash invocation and successful `skill_manage` writes, and SHALL never transmit it off the machine.
**Milestone:** M1

#### Scenario: View bumps the counter
- **GIVEN** skill `foo` with `view_count = 2`
- **WHEN** `skill_view(name="foo")` succeeds
- **THEN** `view_count` SHALL be 3 and `last_activity_at` SHALL be updated

### Requirement: Curator configuration and scheduling
The curator SHALL be configured under `[curator]` with defaults `enabled = true`, `interval_secs = 604800`, `min_idle_secs = 7200`, `stale_after_secs = 1209600`, `archive_after_secs = 2592000`, `consolidate = false`, `prune_builtins = false`, `backup_keep = 2`, where values below 1 for the `_secs` fields SHALL fail config validation; an automatic pass SHALL run only when enabled, not paused, the last run is older than the interval, and the profile has been idle at least `min_idle_secs`; the first observation SHALL seed the last-run time; at most one pass per profile SHALL run at a time.
**Milestone:** M2

#### Scenario: Invalid interval is a validation error
- **GIVEN** `curator.interval_secs = 0`
- **WHEN** the config is loaded
- **THEN** the system SHALL fail validation naming `curator.interval_secs`

### Requirement: Curator transitions, scope and backups
The curator SHALL manage only skills with `created_by = "agent"` or explicitly adopted with `prometheus curator adopt <name>`, SHALL skip pinned skills and skills referenced by any scheduled job (including paused ones), SHALL mark an active skill stale after `stale_after_secs` without activity, reactivate a stale skill used again, archive a skill after `archive_after_secs` by moving it to `<profile_home>/skills/.archive/<name>` (timestamp suffix on collision) and SHALL never delete; hub, plugin, external-dir and bundled skills SHALL never be archived; every real (non-dry-run) pass SHALL first write a backup archive under `<profile_home>/skills/.backups/`, keeping the newest `backup_keep`.
**Milestone:** M2

#### Scenario: Every real pass is backed up
- **GIVEN** `curator.consolidate = false`
- **WHEN** `prometheus curator run --sync` archives one skill
- **THEN** a new backup archive SHALL exist under `.backups/` taken before the archive move

### Requirement: Curator CLI
The system SHALL expose `prometheus curator` with subcommands `status`, `run` (`--sync`, `--dry-run`, `--consolidate`), `pause`, `resume`, `pin`, `unpin`, `adopt`, `list-unmanaged`, `list-archived`, `archive`, `restore`, `prune --days <n>`, `backup` and `rollback`, where `--dry-run` SHALL change no state and print the planned transitions, and `restore` SHALL refuse a name now occupied by a hub, plugin or bundled skill.
**Milestone:** M2

#### Scenario: Dry run changes nothing
- **WHEN** the user runs `prometheus curator run --dry-run`
- **THEN** the skills tree and `skill_usage` SHALL be byte-for-byte unchanged and the planned transitions SHALL be printed

### Requirement: Skill authoring lint
The system SHALL provide `prometheus skills lint [<path>...] [--strict]` that checks frontmatter validity, directory name equal to `name`, description length against `skills.index_description_chars` and ending with a period, the banned marketing words `powerful`, `comprehensive`, `seamless`, `revolutionary`, `cutting-edge`, `state-of-the-art` and `advanced`, `related_skills` resolvability, absence of machine-local home paths, and body size at most 100,000 characters; it SHALL exit 0 when clean, 1 when `--strict` and any finding exists, and the same rules SHALL gate bundled skills in CI.
**Milestone:** M1

#### Scenario: Marketing word fails strict lint
- **GIVEN** a skill whose description is `Advanced log analysis.`
- **WHEN** `prometheus skills lint --strict` runs on it
- **THEN** the system SHALL report the banned word and exit 1

### Requirement: Skill evals (upgrade)
The system SHALL recognise `evals/evals.toml` inside a skill as a list of `[[case]]` entries with `prompt`, optional `setup_files`, and assertions `expect_skill_loaded` (bool), `expect_tools` (list), `expect_output_contains`, `expect_output_regex` and optional `judge` (a rubric string evaluated by the auxiliary judge model), and SHALL provide `prometheus skills test <name>|--all [--provider mock|live] [--repeat N]` that runs each case in an isolated temporary profile and sandbox, reports per-case pass/fail and the pass rate, writes a JSON report under `<profile_home>/evals/skills/`, and exits 0 only when every case passes; the mock provider SHALL make the run deterministic and offline.
**Milestone:** M3

#### Scenario: Skill eval detects a non-triggering description
- **GIVEN** skill `csv-tools` with a case `prompt = "sum column B of data.csv"` and `expect_skill_loaded = true`
- **WHEN** `prometheus skills test csv-tools --provider live` runs and the model never calls `skill_view(name="csv-tools")`
- **THEN** the case SHALL fail with `skill not loaded` and the command SHALL exit 1

### Requirement: Eval-gated skill edits (upgrade)
When `skills.eval_gate = true` (default `false`), the system SHALL run a skill's evals with `--repeat` equal to `skills.eval_gate_repeat` (default 3) before committing any background-review or curator consolidation change to a skill that has evals, and SHALL reject the change with `edit rejected: eval pass rate fell from <a> to <b>` when the pass rate drops below the pre-change rate; user-initiated edits SHALL only be warned.
**Milestone:** M3

#### Scenario: Regressing agent edit is rolled back
- **GIVEN** `skills.eval_gate = true` and skill `foo` passing 3 of 3 cases
- **WHEN** a background review patch makes it pass 1 of 3
- **THEN** the system SHALL keep the original content and record the rejection in the trajectory

### Requirement: Profile-scoped skill state
Every skill path and record (skills directory, lockfile, archive, backups, locks, usage rows, pending writes) SHALL be resolved from the explicit ProfileScope of the operation, never from process-wide state, so a gateway serving several profiles reads and writes only the bound profile's skills, and user-facing paths SHALL be printed relative to the displayed Prometheus home.
**Milestone:** M1

#### Scenario: Gateway serves two profiles without crossover
- **GIVEN** one gateway serving profiles `a` and `b`
- **WHEN** a session in `a` creates skill `x`
- **THEN** `skills_list` in a session of `b` SHALL NOT include `x`
