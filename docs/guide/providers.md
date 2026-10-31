# Provider credentials and sign-ins

Every model call carries one credential: an **API key** from the
profile's secrets store, a **subscription sign-in** (Claude Max, a ChatGPT
plan) that Prometheus signed in to itself, or a login **borrowed** from
another agent CLI on the same machine. This guide shows how to set each
one up and what Prometheus does with them. How a provider and model are
chosen is in [Models and providers](models.md).

Every command in a `console` block below is run by the documentation tests
against a fresh home, in order. Lines starting with `$` are commands; the
lines after a command are (part of) its output, with `<...>` standing for
text that varies. Sign-in commands contact the provider and are shown in
plain blocks without output.

## API keys

`prometheus auth add <provider>` reads a key from a hidden prompt, or from
stdin when it is piped. A key is never accepted as an argument, so it
never lands in shell history or a process listing:

```console
$ prometheus auth add openai sk-abc123  # exits 2
error: API keys are never accepted as arguments; they are read from the hidden prompt or stdin
$ echo -n "$TOKEN" | prometheus auth add openrouter
stored API key 'default' of openrouter as secret OPENROUTER_API_KEY
```

A key is an ordinary secret: the `default` label is the provider's first
secret name (`OPENROUTER_API_KEY`), so `prometheus secrets set
OPENROUTER_API_KEY` does the same thing. A second key gets a label and
is stored as `<NAME>_<LABEL>`:

```console
$ echo -n "$TOKEN" | prometheus auth add openrouter --label work
stored API key 'work' of openrouter as secret OPENROUTER_API_KEY_WORK
$ prometheus auth list
openrouter default api_key file ok
openrouter work api_key file ok
```

`auth list [--json]` and `auth status <provider> [--json]` show provider,
label, kind (`api_key` or `subscription`), source, status (`ok`,
`benched`, `dead`), cooldown and request count, never key or token
material. They are peeks: they never refresh, rotate or bench anything.
Keys are sent only to the hosts they are bound to (see
[Credentials stay on their hosts](models.md#credentials-stay-on-their-hosts)).

## Credential pools

With two or more keys for one provider (`default` plus labelled keys), the
keys form a pool and every request takes one by
`providers.<id>.pool_strategy`:

| Strategy | Picks |
|---|---|
| `fill_first` (default) | the first usable key, in `auth status` order |
| `round_robin` | the key that served least recently (A, B, A, B...) |
| `random` | any usable key |
| `least_used` | the key with the fewest requests |

```console
$ prometheus model explain --provider openrouter
credential ok pool:openrouter#1
$ prometheus config set providers.openrouter.pool_strategy round_robin
$ prometheus config set providers.openrouter.pool_strategy weighted  # exits 2
error: providers.openrouter.pool_strategy: unknown value "weighted" (allowed: "fill_first", "round_robin", "random", "least_used")
```

Request counts and benches live in the profile's `state.db`, so every
process of the profile (a gateway and a CLI session) shares them and they
survive restarts. A key that fails is **benched** and skipped:

- until the provider's reset time, when it sent one that is in the future;
- otherwise 300 seconds after HTTP 401, and 3600 seconds after HTTP 429,
  HTTP 402 or a billing failure;
- at most 60 seconds when it is the provider's only credential, unless the
  failure is billing.

The same-route retry of the failed request already takes the next key.
When every credential of a provider is benched, requests fail with
`auth.credentials_benched` (exit 69), or the fallback chain takes over.
The provider's other secret names (a shared key next to a plan key) are
not pool members: they are alternatives, used in order when no pool key
is usable. `auth remove` deletes a key:

```console
$ prometheus auth remove openrouter work
removed api_key 'work' of openrouter
```

## Subscription sign-ins

Some providers can be used with a consumer subscription instead of an API
key. `auth status` shows the sign-in flows a provider offers and its plan
notes, which are also printed before every sign-in:

```console
$ prometheus auth status anthropic
sign-in flows: browser
note: Claude Pro plans are not eligible for use outside Anthropic's own apps; sign in with a Claude Max plan, or use an API key: prometheus auth add anthropic --type api_key
$ prometheus auth status openai-codex
sign-in flows: device, browser
note: Needs a ChatGPT Plus, Pro, Business, Edu or Enterprise plan; requests count against the plan's Codex usage limits.
```

**Claude Max** (`anthropic`) signs in through the browser with PKCE; the
redirect goes to a loopback port on this machine, and over SSH the command
prints the tunnel to open. A plan Anthropic refuses for this use fails
with `auth.plan_not_eligible`, and the sign-in stays valid:

```sh
prometheus auth add anthropic --type oauth
```

**ChatGPT plans** (`openai-codex`) sign in with a device code by default:
the command prints a URL and a code to enter in any browser. `--browser`
uses PKCE on `localhost:1455`; when that port is busy the sign-in
continues with the device code. Requests use the `responses` wire format
with the plan's account id. An existing Codex CLI sign-in can be copied
instead (the two then share one rotating refresh token, so the other copy
stops working after either refreshes):

```sh
prometheus auth add openai-codex
prometheus auth add openai-codex --browser --label work
prometheus auth import codex-cli
```

When a provider has both a sign-in and an API key,
`providers.<id>.prefer_credential` decides (`subscription` by default); a
sign-in whose plan was refused yields to the key. Every call records its
billing source (`subscription:<provider>/<label>` or
`api_key:<provider>[/<label>]`).

### Where tokens live and how they are refreshed

A sign-in's access token, refresh token, expiry, account id and plan live
only in the profile's secrets store (`secrets.toml`, mode 0600, or the OS
keychain): never in `config.toml`, `state.db`, logs or trajectories, and
never shared between profiles.

An access token is refreshed a minute before it expires and when the
provider answers HTTP 401, and only the credential that request used is
refreshed. Refreshes of one sign-in take a lock shared by every process of
the profile and re-read the stored token first, so when a gateway and a
CLI session both find the token expired, only one refresh request is
sent and both use the rotated token. A rate-limited token endpoint
(HTTP 429) benches the sign-in.

When the provider refuses the refresh token for good (revoked, expired or
reused), the sign-in is quarantined as `dead`: its refresh token is deleted
and never sent again, and requests fail with `auth.reauth_required`:

```text
Sign-in for openai-codex (personal) expired or was revoked. Run 'prometheus auth add openai-codex' to sign in again.
```

Signing in again with the same label (`prometheus auth add openai-codex
--label personal`) makes it `ok` again.

## Borrowed CLI logins

If Claude Code or the Codex CLI is signed in on this machine and the
profile has no sign-in of its own for that provider, Prometheus borrows
that login: Claude Code's (`~/.claude/.credentials.json`, or its macOS
keychain item) for `anthropic`, the Codex CLI's (`~/.codex/auth.json`) for
`openai-codex`. It is read in place at every use and never copied into the
secrets store; `auth list` shows it as `borrowed:claude-code` or
`borrowed:codex-cli`, and its first use in a session is announced. An
owned sign-in always wins, and then the other CLI's store is not read.

A borrowed login is refreshed only after a request that used it failed
with HTTP 401, and the rotated tokens are written back into the other
CLI's own file (only the token fields change), so that CLI keeps working.
Prometheus never deletes a borrowed login (`auth remove` refuses it). To
stop borrowing, and never read or refresh another CLI's credentials:

```console
$ prometheus config set auth.adopt_external_logins false
$ prometheus config get auth.adopt_external_logins
false
```
