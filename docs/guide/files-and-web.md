# File and web tools

The `file` toolset ([`read_file`](../reference/tools.md#read_file),
[`write_file`](../reference/tools.md#write_file),
[`patch`](../reference/tools.md#patch) and
[`search_files`](../reference/tools.md#search_files)) and the `web` toolset
([`web_search`](../reference/tools.md#web_search) and
[`web_extract`](../reference/tools.md#web_extract)) are part of `core`. This
page describes what they refuse and what they return.

## Where file tools act

File tools go through the session's execution backend. On `local` they
touch host paths, and every access is checked first against the session's
`fs.read`/`fs.write` capabilities (by default the workspace and the
session scratch directory; see
[Capabilities, grants and redaction](security.md)). A path outside them is
`denied` naming the missing capability. Relative paths resolve against the
session's working directory, which follows `cd` in the terminal; `~`
expands to the user home.

## Reading

[`read_file`](../reference/tools.md#read_file) returns numbered lines
(`     1|text`), 2,000 lines from `offset` (default 1) unless `limit` says
otherwise, and at most `files.read_max_chars` (default 100,000) formatted
characters. When lines remain the result carries `next_offset`.

- UTF-16 files (with a byte-order mark, or recognised by their NUL-byte
  pattern) are decoded and the result names the `encoding`.
- A range holding merge-conflict blocks (`<<<<<<< ` ... `>>>>>>> ` at line
  start) carries `conflict_blocks` and a hint to resolve them first.
- A missing path answers `File not found: <path>` with up to five similar
  paths of the same or the parent directory.
- Reading the same unchanged region again returns a short
  `{"status": "unchanged"}` stub; a third identical read is refused with
  `BLOCKED: You have already read this exact region ...`. A change on disk
  or a context compaction makes the region new again.

Refused outright: binary files, device files, named pipes and sockets,
`/proc/<pid>/environ`, `mem`, `maps` and similar files, and project dotenv
files (`.env`, `.env.*` except `.env.example`, `.env.sample` and
`.env.template`, `.envrc`), which typically hold secrets; a link to one is
refused the same way. Every tool that would show file content applies these
read refusals: [`search_files`](../reference/tools.md#search_files) skips
such files in content searches and [`patch`](../reference/tools.md#patch)
refuses to edit them (its diff and closest-candidate region would reveal the
old content). Writing a dotenv file stays allowed: it can be created by
[`write_file`](../reference/tools.md#write_file) or a V4A `Add File`, and an
existing one can be replaced whole without the usual full read (which can
never happen); one that changed on disk since the session's own write is
still refused as stale. Windows NT and device-namespace paths (`\??\`, `\\.\`,
`\\?\UNC\`, `\\?\GLOBALROOT`) and UNC hosts (`\\host\share`) are refused on
every platform before anything is opened, so a crafted path cannot make the
machine authenticate to another host.

## Writing

[`write_file`](../reference/tools.md#write_file) creates a file (and its
parent directories) or replaces it whole. Replacing an existing file needs
the session to have read all of it, and the file to be unchanged since that
read or the session's own last write; otherwise the result is an error with
`stale_write_blocked: true` and the file keeps what is on disk. Writes are
atomic (temporary file renamed into place) and keep the file's permissions.

Refused for every write tool, whatever the approval mode:

- `/etc/`, `/boot/`, `/usr/lib/systemd/`, `/private/etc/`,
  `/private/var/db/` and container runtime sockets;
- every profile's `config.toml`, `secrets.toml` and `grants.toml` under the
  Prometheus home, also through a symbolic link
  (`refusing to write Prometheus control file: grants.toml`);
- replacing a binary file with text.

Writes to instruction files (`AGENTS.md`, `CLAUDE.md`, `SOUL.md`,
`IDENTITY.md`, `.cursorrules`, `.prometheus/`) and to `~/.ssh/config` always
ask the human, also under `--yolo`, and are denied where no human can answer
(cron, single-query and unattended runs).

## Patching

[`patch`](../reference/tools.md#patch) has two shapes; it accepts either,
whichever the model was shown.

- `mode = "replace"` (default): `path`, `old_string`, `new_string`,
  `replace_all`. The text is located by the first of these strategies that
  finds anything: exact, line-trimmed, trailing whitespace, indentation,
  unicode quotes and dashes, escaped `\n`/`\t`, collapsed whitespace, first
  and last line anchors, then overall similarity. A non-exact match is
  reported as `match_strategy` and keeps the file's indentation. More than
  one match without `replace_all` fails with
  `old_string matches <n> locations; add context or set replace_all=true`;
  no match fails showing the closest region of the file.
- `mode = "v4a"`: the patch text, in the `*** Begin Patch` / `*** End Patch`
  format with `*** Add File:`, `*** Update File:` (optionally
  `*** Move to:`) and `*** Delete File:` sections. Every file is computed
  before any is written: a hunk that does not apply fails the whole patch,
  naming the file, and nothing changes.

The result holds a unified `diff` of the change.

## Syntax checks after a write

After every successful [`write_file`](../reference/tools.md#write_file) and
[`patch`](../reference/tools.md#patch) the new content is parsed
in-process and the result carries `lint: {status, output}`: `ok`, `error`
with the parser's message and line, or `skipped`. Rust, Python,
JavaScript, TypeScript (and JSX/TSX), JSON, TOML and YAML are checked. A
checker never fails or undoes the write: a file type without a checker, a
file over 2 MiB, a checker that crashes or runs past 2 seconds all report
`skipped`. Language-server diagnostics arrive with the LSP integration in a
later release.

## Searching

[`search_files`](../reference/tools.md#search_files) searches file content
with a regex (`target = "content"`, the default) or file names with a glob
(`target = "files"`), below `path` (default: the working directory). It
honours `.gitignore` (also outside a git repository), skips VCS directories
and binary files, and takes:

| Argument | Default | Meaning |
|---|---|---|
| `file_glob` | none | only files whose names match, e.g. `*.rs` |
| `output_mode` | `content` | `content` (`<path>:<line>:<text>`), `files_only` or `count` (per-file counts, no content) |
| `context` | 0 | lines around each match (at most 10), shown as `<path>-<line>-<text>` |
| `limit` | 50 | entries per page (at most 500) |
| `offset` | 0 | entries skipped |
| `order` | `discovery` | or `modified`, newest first |

Lines are cut to 500 characters. The result gives `total_matches`, and
`truncated` with the next `offset` when more entries remain. An invalid
regex fails with `invalid pattern: <reason>` before anything is searched.

## Web search and extraction

Each web tool uses the backend configured for it: `web.search_backend` or
`web.extract_backend`, else `web.backend`, else `auto` (the default).
`auto` takes, per tool, the first backend in the order `tavily`,
`perplexity`, `exa`, `parallel`, `firecrawl`, `nous`, `searxng`,
`brave-free`, then `keyless` (only with `web.keyless.enabled = true`) whose
credentials are present (a secret in the secrets store, or the backend's
URL setting) and that serves the tool; backends this build does not ship
are skipped. `xai` and `openai-native` are never chosen by `auto`, so an
xAI key alone leaves the web tools unavailable. `prometheus tools explain
web_search` shows the resolved backend (`backend = ...`), and every result
names it in `backend`. This build ships:

| Backend | Search | Extract | Configuration |
|---|---|---|---|
| `firecrawl` | yes | yes | secret `FIRECRAWL_API_KEY` for the hosted API, or `web.firecrawl.base_url` for a self-hosted one |
| `searxng` | yes | no | `web.searxng.url` (the instance must allow `format=json`) |

```toml
[web]
search_backend = "searxng"
extract_backend = "firecrawl"

[web.searxng]
url = "http://127.0.0.1:8888"
```

Without a configured backend the tools are not exposed, and a search-only
backend chosen for extraction leaves extraction unavailable. Both tools
need the `net.connect:*` capability.

```console
$ prometheus tools explain web_search
<...>
exposure = unavailable:no web backend configured
$ prometheus config set web.backend searxng
$ prometheus config set web.searxng.url http://127.0.0.1:8888
$ prometheus tools explain web_extract
<...>
exposure = unavailable:backend 'searxng' is search-only; set web.extract_backend
$ prometheus tools explain read_file
<...>
exposure = exposed
```

[`web_search`](../reference/tools.md#web_search) passes the query verbatim
and returns at most `limit` (default 5, at most 100) results with title,
URL, snippet and position, plus the `backend` that answered; a backend
failure is an error naming the backend, never an empty list.

[`web_extract`](../reference/tools.md#web_extract) takes 1 to 5 URLs and
returns, per URL, `{url, title, content, status}` as Markdown with images
replaced by `[IMAGE: <alt>]`. A page within `char_limit` (default
`web.extract_char_limit`, 15,000; clamped to 2,000-500,000) characters is
returned whole. A longer page is returned as a deterministic window, about
75% head and 25% tail cut on line boundaries, followed by a `[TRUNCATED]`
footer naming the stored full text and the exact
[`read_file`](../reference/tools.md#read_file) call that pages through the
omitted lines; the same page always yields the same bytes and nothing is
summarized. Full texts are stored under `<profile>/cache/web/` (0600, one
file per URL, format and backend), capped at 2,000,000 characters with a
final line stating the cap, and deleted after 7 days (the daemon prunes on
start and hourly). Each URL is checked before the
backend sees it: only `http(s)`, and a host that resolves to a private,
loopback or link-local address (unless `security.egress.allow_private` or
a grant allows it), to a cloud metadata address, or does not resolve is
`blocked`. Each backend call is bounded by `web.extract_timeout_secs`
(`timeout`); one failing URL never fails the others.

## Images and vision

[`vision_analyze`](../reference/tools.md#vision_analyze) (toolset `vision`,
part of `core`) answers a `question` about an image given as an http(s)
URL, a `data:` URL or a local path (read with the same guards as
[`read_file`](../reference/tools.md#read_file)). Downloads pass the same
destination checks as `web_extract` and stop at 50 MiB; animations with
more than 100 frames or more than 100,000,000 pixels over all frames are
refused. `region = [x1, y1, x2, y2]` crops the image in original pixels
first, so small text stays readable.

How an image reaches the model follows `vision.image_input_mode`:

| Mode | Pixels sent | Otherwise |
|---|---|---|
| `auto` (default) | when the main model supports images and `[auxiliary.vision]` names no provider or model | a description from the `vision` auxiliary route |
| `native` | always | - |
| `text` | never | a description from the `vision` auxiliary route |

The same rule covers images you attach,
[`vision_analyze`](../reference/tools.md#vision_analyze) results and other
tools' images. On a main route that sees images and carries them in tool
results (Anthropic Messages), the tool returns the image itself
(and says so when the image is already attached to your message);
otherwise the `vision` auxiliary route answers in text. With neither, the
tool is unavailable with `no vision-capable route`. Routes accept only
PNG, JPEG, GIF and WebP inline: an SVG is rasterized to PNG, and an image
that cannot be converted becomes a short note instead of failing the
request. The choice is made when a message is first sent and never
changes for it, so the cached prompt prefix stays the same.
