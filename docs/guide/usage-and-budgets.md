# Usage, costs and budgets

Every model call (main, auxiliary, compression, subagent, cron...) and
every paid tool call appends one row to the profile's usage ledger, the
`usage_ledger` table of `state.db`. Rows are never changed; only deleting
a session removes its rows. Each row records the session and its **root
session**, so continuations after compression, forks and delegated
subagents all count towards the lineage they belong to.

Every command below is run by the documentation tests against a fresh home,
in order. Lines starting with `$` are commands; the lines after a command
are (part of) its output, with `<...>` standing for text that varies.

## How calls are priced

A call's `cost_usd` comes from, in order:

1. the cost the provider reported (`cost_status = exact`);
2. the model's price table, per million input, output, cache-read,
   cache-write and reasoning tokens (`estimated`): set it with
   `[models."<provider>/<model>".price]` in `config.toml` (reasoning
   tokens are part of the output tokens and cost the output price unless
   `price.reasoning` is set);
3. nothing: the call is **unpriced** (`cost_usd` empty,
   `cost_status = unknown`).

Calls covered by a subscription record `cost_status = subscription` and
cost 0; calls answered from the replay cache cost 0. An unpriced call is
shown as `—`, never as `$0.00`, and every total reports how many calls
were unpriced (`unpriced_calls`), so a local model without a price is
never mistaken for a free one.

## The usage command

`prometheus usage` prints totals of calls, tokens by class, `cost_usd` and
`unpriced_calls` for the last 30 days, one row per day:

```console
$ prometheus usage
profile default since <since>, by day (<zone>)
total: 0 calls, cost_usd $0.0000, unpriced_calls: 0
```

- `--since S` / `--until S`: `30m`, `2h`, `7d` ago, or an RFC 3339 time;
- `--by root|session|model|provider|purpose|day|profile`: one row per
  group; the rows always add up to the total;
- `--session <id>`: that session and every session below it (for a root
  session: its whole lineage);
- `--all-profiles`: every profile, each read through its own scope, with a
  grand total;
- `--json`: one `prometheus.usage.v1` document.

Days (here and for daily and monthly budgets) are calendar days in the
profile's time zone, `cron.timezone` (an IANA name such as
`Europe/Lisbon`; unset uses the computer's zone):

```console
$ prometheus config set cron.timezone Europe/Lisbon
$ prometheus usage --by purpose --since 7d
profile default since <since>, by purpose (Europe/Lisbon)
$ prometheus usage --since yesterday # exits 2
error: invalid --since value 'yesterday': use 30m, 2h, 7d or an RFC 3339 time
```

## Budgets

Budgets are hard limits in the `[budgets]` section; `0` turns one off:

| Key | Limits |
|---|---|
| `turn_tokens`, `turn_usd` | one turn, with its compression, auxiliary and delegated calls |
| `session_tokens`, `session_usd` | a session and the sessions below it |
| `root_tokens`, `root_usd` | a whole root lineage |
| `daily_usd`, `monthly_usd` | the profile's calendar day and month |
| `unattended_root_usd` (default 5.00) | the lineage of a cron job, unattended gateway session, background delegation or harness evolution run |

Before every model call Prometheus estimates its worst case (input tokens
at the input price plus the requested maximum output at the output price).
If any budget would be exceeded the call is **not sent**: the turn ends
with a `budget_exceeded` error naming the scope, the limit and what was
spent, a `budget.exceeded` event is recorded, and a one-shot CLI run exits
3. When a USD budget applies to a model without a price, the call is
refused too (`budget.unpriced_model`) unless `budgets.allow_unpriced = true`,
which enforces only the token budgets for that model and logs one warning
per session.

When spending first crosses `budgets.warn_ratio` (default `0.8`) of a
limit, you get one notice and one `budget.warning` event per scope and
period.

Only you change budgets, with `prometheus config set` or, inside a
session, `/budget set <scope> <usd>` (and `/budget` to see each limit with
its spend). A raised budget applies to the very next call. Agent tools are
refused (`budget.agent_write`, exit 77) whenever they try to change a
`budgets.*` key, including by editing `config.toml`.

```console
$ prometheus config set budgets.root_usd 1.00
$ prometheus config get budgets.root_usd
1.0
```

## No telemetry

Prometheus sends nothing about you anywhere: no usage statistics, crash
reports, trajectories or metrics leave the computer except to the model
providers and tools you configure. There is no built-in telemetry
endpoint. `telemetry.enabled = true` (default `false`) only takes effect
with an `https://` `telemetry.endpoint` you set yourself (plain `http://`
only to a loopback address); any other endpoint is refused with
`refusing to send telemetry to <endpoint>` in the log, and nothing is
sent.
