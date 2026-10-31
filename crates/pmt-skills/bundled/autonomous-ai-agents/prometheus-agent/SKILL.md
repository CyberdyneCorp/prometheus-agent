---
name: prometheus-agent
description: Operate, configure and troubleshoot Prometheus itself.
category: autonomous-ai-agents
tags: [prometheus, configuration, troubleshooting]
---

# Prometheus Agent

You are running inside Prometheus. This skill tells you how to inspect and
change your own installation with the `prometheus` command line (alias
`pmt`). Every command accepts `--json` for one machine-readable document on
stdout, `-p <profile>` to act on another profile and `--home <path>` for
another home.

## When to Use

- The user asks how Prometheus works, where a setting lives or how to change it.
- A model, provider, tool, skill or session misbehaves and you must find out why.
- The user wants a new profile, a different model, a secret stored or a
  skill enabled, disabled or ejected.

## Procedure

1. Find the files first: `prometheus config path` prints the home and the
   profile's `config.toml`. Never guess paths.
2. Read settings with `prometheus config show` or `prometheus config get <key>`;
   `prometheus config explain <key>` says what a key does and when a change
   applies (live, next session or restart).
3. Change settings with `prometheus config set <key> <value>` and check them
   with `prometheus config validate`. Settings marked `next_session` apply
   from the next session.
4. Store credentials with `prometheus secrets set <NAME>` (the value is read
   from the terminal, never from the command line) or `prometheus auth`.
   Never print, log or repeat a secret value.
5. Models: `prometheus model list` and `prometheus model set <provider/model>`;
   `prometheus fallback` edits the fallback chain.
6. Profiles: `prometheus profile list`, `prometheus profile create <name>`
   (`--no-skills` starts without bundled skills), `prometheus profile use <name>`.
7. Tools: `prometheus tools list`, `prometheus tools explain <tool>` and
   `prometheus toolsets list`; `prometheus approvals test -- <command>` shows
   how a shell command would be approved.
8. Skills: `prometheus skills list`, `prometheus skills show <name>`,
   `prometheus skills enable|disable <name>`, `prometheus skills eject <name>`
   to edit a bundled skill, `prometheus skills lint` and
   `prometheus skills doctor` for skills that do not load.
9. Diagnose with `prometheus logs`, `prometheus sessions list`,
   `prometheus sessions show <id>`, `prometheus trajectory show <id>`,
   `prometheus usage` and `prometheus prompt size`.
10. Before a risky change, take a snapshot with `prometheus snapshots create`
    (restore with `prometheus snapshots restore`) or a whole-home archive with
    `prometheus backup create`.

## Pitfalls

- Skill, toolset and memory changes keep the running session's prompt (and
  its prompt cache) unchanged; they reach the model in the next session
  unless the user repeats the slash command with `--now`.
- `prometheus-agent` is the essential skill: it cannot be disabled.
- Bundled skills are read-only; eject a copy instead of editing them.
- Project skills of a cloned repository load only after
  `prometheus skills trust <repo root>`.
- `.env` files and environment variables are not settings: behaviour lives in
  `config.toml`, credentials in the profile's secret store.

## Verification

- `prometheus config validate` exits 0 after a settings change.
- `prometheus skills doctor` prints `no skill problems`.
- `prometheus version --json` shows the build when reporting a problem.
