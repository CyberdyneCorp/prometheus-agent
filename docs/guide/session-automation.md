# Session automation: /goal, /loop and /heartbeat

Session automation keeps one session working without you re-prompting it:

- `/goal` sets a standing goal. After every turn an auxiliary *judge*
  decides whether it is achieved; until it is, the session gets another
  turn on its own.
- `/loop` repeats a prompt (or a slash command) on a cadence.
- `/heartbeat` checks in while the session is idle.

Each session holds at most one goal, one loop and one heartbeat. They live
in the profile's `state.db` with the session, so they survive `/resume`,
a daemon restart and context compaction. `/new` (`/reset`), deleting the
session, and switching a messaging conversation to another session drop
them; resuming that archived conversation later does not bring them back.

Automation is not the scheduler: it never creates a scheduled job and
never touches a kanban board. Use [scheduled jobs](cron.md) for
unattended work that needs its own session.

## How automated turns run

Every automated wakeup — a goal continuation, a loop wakeup, a heartbeat
— is an ordinary user turn. It never changes the system prompt, the
tools, memory or earlier messages, so the provider's prompt cache keeps
working. Its stored user message carries `origin` (`goal`, `loop` or
`heartbeat`) in its metadata.

Automation only fires when the session is idle: no turn runs and nothing
you typed waits. What you type always goes first; then, in order, an
active goal's continuation, a loop wakeup, a heartbeat. At most one
automated turn starts per idle moment. While a goal is active (and not
waiting, see below) loops and heartbeats wait; when it is paused, waiting,
done or cleared, they use the idle time.

The line interface drives the automation of the session it runs
in-process. The daemon drives every session it serves, whether or not a
client is attached: a TUI or desktop session's loop keeps firing after
you close the app, and the replies are in the session when you come
back. A session with an active loop or heartbeat is not closed by the
idle reaper.

## Goals

```text
/goal port the parser to the new AST
```

sets (or replaces) the goal and starts its first turn at once. The other
forms:

| Command | What it does |
|---|---|
| `/goal`, `/goal status` | the goal, its status and turns used (`4/20`) |
| `/goal show` | the goal with its completion contract, subgoals and gates |
| `/goal -- <text>` | a goal that starts with a control word (`/goal -- pause the nightly cron`) |
| `/goal draft <text>` | asks the judge to write a completion contract first |
| `/goal pause`, `/goal resume` | stop and restart the judge loop (`resume` resets the turn count) |
| `/goal clear` (`stop`, `done`) | drop the goal |
| `/goal wait <pid> [reason]` | park until that local process exits |
| `/goal unwait` | stop waiting |
| `/goal gate add <command>`, `list`, `remove <N>`, `clear` | quality gates |

A control word followed by more words runs the control command and
ignores the rest (`/goal pause the nightly cron` pauses). `draft` is a
command only as a whole word: `/goal drafting docs` sets that text.

While a turn runs, `status`, `pause`, `clear`, `wait`, `unwait` and
`gate list|remove|clear` only change the goal's state; setting a new goal
is refused with `A turn is running; use /stop first, then set the new
goal.`

### The judge

After each turn of a session with an active goal, the `goal_judge`
auxiliary task gets the goal, its contract and subgoals, the last 4000
characters of the agent's final answer and the session's own background
processes, and answers one line of JSON:

```json
{"verdict": "continue", "reason": "3 files remain"}
```

- `continue`: another turn runs, shown as `Continuing toward goal (1/20):
  3 files remain`;
- `done`: `Goal achieved: …`;
- `blocked`: the goal pauses (`Goal judged unachievable — paused: …`);
  it is never reported achieved;
- `wait`: the goal parks (below).

The judge runs on the session's main model unless `[auxiliary.goal_judge]`
names another. If it fails, times out or answers something that is not a
verdict, the goal keeps going (the error is logged at WARN). The legacy
`{"done": true, "reason": "…"}` form is accepted.

### Turn budget

Continuation turns count against `goals.max_turns` (20). When the count
reaches it the goal pauses:

```text
Goal paused — 20/20 turns used. Use /goal resume to keep going, or /goal clear to stop.
```

### Completion contracts and subgoals

Lines of the goal text that start with one of these prefixes fill its
completion contract; every other line stays in the goal:

| Prefix | Field |
|---|---|
| `verify:`, `verified by:` | verification |
| `constraints:`, `preserve:` | constraints |
| `boundaries:`, `scope:` | boundaries |
| `stop when:`, `blocked:` | stop when |

With a verification set, the judge only says `done` with concrete
evidence for it. `/goal draft <text>` asks the judge to write the whole
contract (outcome, verification, constraints, boundaries, stop when);
when it cannot, the text becomes a plain goal and the first turn starts
anyway.

`/subgoal <text>` adds a numbered criterion to the active goal (`/subgoal`
lists them, `/subgoal remove <N>`, `/subgoal clear`). The next
continuation lists them under `Additional criteria the user added
mid-loop`, and the judge says `done` only when every one is met. Adding
one does not reset the turn count.

### Quality gates

`/goal gate add <command>` adds a shell command that runs at every turn
boundary, before the judge, on the session's own terminal (its backend,
sandbox and working directory), with `goals.gate_timeout_secs` (300 s).
When a gate exits non-zero the judge is skipped and the next continuation
carries the exit code and the last 3072 bytes of the output. A failing
gate runs again at every boundary. After `goals.gate_max_retries` (3)
failures in a row the goal pauses, naming the gate and suggesting
`/goal gate remove <N>` or `/goal resume`.

On a messaging platform, adding a gate needs a configured gateway admin
of the chat (`admins` / `group_admins`); listing, removing and clearing
gates stay open to the session's users.

### Waiting

A `wait` verdict parks the goal on one of:

- `wait_on_session`: a background process of the session (released when
  it exits or its watch pattern matches);
- `wait_on_pid`: a local process (released when it exits);
- `wait_for_seconds`: a timer.

`/goal wait <pid>` parks it on a local process yourself; a pid that is
not running is refused (`process 99999 is not running`), and a judge's
`wait_on_pid` on a dead process just continues. A wait is capped at
`goals.max_wait_secs` (1800 s). While parked there is no judge call, no
continuation and no turn counted, and `/goal status` says what the goal
is parked on. `/goal unwait`, `pause`, `resume` and `clear` drop the
wait. Only processes the session started itself — not its subagents' —
are shown to the judge.

## Loops

```text
/loop 5m check the deploy and tell me if anything failed
/loop keep an eye on the migration --until the migration finished
/loop 2m /status --times 3
```

`/loop [interval] <prompt-or-slash-command> [--times N] [--until
<condition>]` (alias `/proactive`) replaces any loop; its first wakeup
runs at the next idle moment. A given interval (`30s`, `5m`, `2h`,
`1h30m`) is a fixed cadence and must be at least `loops.min_interval_secs`
(30 s). Without one the loop paces itself: it waits
`loops.self_paced_floor_secs` (60 s) and doubles the wait while the
replies stay the same (timestamps aside), up to
`loops.self_paced_ceiling_secs` (900 s), and drops back to the floor when
a reply changes.

A loop ends when a reply's last line is `LOOP_COMPLETE`, after `--times N`
wakeups, or with `/loop stop`. With `--until`, the judge is asked after
each wakeup whether the condition holds: yes ends the loop, an
unachievable condition pauses it, and a judge failure keeps it going. A
loop pauses after `loops.max_ticks` (100; `0` = unlimited) wakeups and
when you interrupt (Ctrl+C) one of its turns. `/loop status`, `/loop
pause` and `/loop resume` do what they say.

## Heartbeats

```text
/heartbeat every 10m check CI
```

`/heartbeat every <interval> <prompt>` (alias `/hb`; at least
`heartbeat.min_interval_secs`, 60 s; units `s`, `m`, `h`, `d`) fires only
when the session is idle and the interval has passed. However many
intervals passed while the session was busy, it fires once, and the next
fire is one interval after it; `/heartbeat resume` restarts the interval
(it never fires at once). Each fire asks the model to reply briefly and
to answer exactly `NO_REPLY` when nothing meaningful changed.
`/heartbeat status`, `pause`, `resume` and `clear` manage it.

## On messaging platforms

Automation set from a chat belongs to that chat. Its turns run in the
conversation like any other turn — after the chat's own messages — and
their replies are posted to the chat or thread without quoting the
message that set them. The status lines (`Goal set`, `Continuing toward
goal`, `Goal achieved`, `Goal paused`, `Goal judged unachievable —
paused`) are posted there too.

A heartbeat set from a chat fires from the gateway and answers in that
chat even while the session is open in the TUI or desktop. A heartbeat
turn shows no typing indicator, tool progress or streamed draft (approval
prompts and failures still do), and a final reply of exactly `NO_REPLY`
or `[SILENT]` sends nothing. When the daemon starts it restores every
chat's loops and heartbeats under the profile that owns them, from the
stored conversation, without a new message; a wakeup the chat cannot take
yet (a turn running, a message waiting) is not spent and is tried again
on a later poll.

## Status on every surface

The daemon publishes the `automation.state` event whenever a goal, loop
or heartbeat changes or a wakeup is scheduled: kind, status, counters
and `next_fire_at`. The line interface and the TUI show a live row in the
status line, for example:

```text
loop · every 5m · 2 runs · next in 4m
```

and `/agents` lists the session's goal, loop and heartbeat under
`Automation`.

## Configuration

```toml
[goals]
max_turns = 20           # 1-1000
gate_timeout_secs = 300
gate_max_retries = 3
max_wait_secs = 1800

[loops]
min_interval_secs = 30
max_ticks = 100          # 0 = unlimited
self_paced_floor_secs = 60
self_paced_ceiling_secs = 900

[heartbeat]
min_interval_secs = 60
```

The judge, contract drafts and `--until` checks use the `goal_judge`
auxiliary task: the session's main model unless `[auxiliary.goal_judge]`
sets a `provider` and `model` (see [models](models.md)).

Out-of-range values fail validation (exit 78) instead of being clamped,
and so does a self-paced floor above its ceiling:

```console
$ prometheus config set goals.max_turns 0 # exits 2
<...>goals.max_turns<...>
$ prometheus config set goals.max_turns 50
$ prometheus config get goals.max_turns
50
```
