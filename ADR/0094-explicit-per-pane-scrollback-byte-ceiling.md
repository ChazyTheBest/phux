---
audience: contributors
stability: stable
last-reviewed: 2026-09-02
---

# 0094 — Per-pane scrollback is bounded in bytes, by phux, explicitly

**TL;DR.** `defaults.history-limit` is only libghostty's *line* limit, and the
engine prunes on whichever of its line and byte limits is reached first. A
terminal built through the C API keeps Ghostty's 10_000-byte constructor
default, so that byte limit — not `history-limit` — decided how deep a phux
pane's history was: about 810 rows at 80 columns and 295 at 200, whatever the
config said. phux now installs the byte limit itself, as a named constant with
a measured cost, because retained history is not free at attach time: the
engine materialises every retained page when a client bootstraps.

Status: Accepted
Date: 2026-09-02

## Context

`TerminalActor::build` passed `defaults.history-limit` (50000) as
libghostty-vt's `TerminalOptions::max_scrollback`, which sets
`GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES`. Nothing set the sibling byte
limit, so it stayed at Ghostty's `Terminal.Options.max_scrollback_bytes`
default of 10_000 bytes, floored by the engine at two standard pages. Pruning
is page-granular, so the effective retention was one standard page of history
regardless of the configured line count. Measured on the pinned engine, at
100_000 written lines:

| `max_scrollback` | byte limit | rows retained (80 cols) | rows retained (200 cols) |
|---|---|---|---|
| 50000 | engine default | 810 | 295 |
| 2000  | engine default | 810 | 295 |
| 50000 | 2 MiB | 2669 | 943 |
| 50000 | 10 MiB | 14403 | 5703 |

The line limit was inert. The reason it cannot simply be made to work is on
the other side of the ledger: the native bootstrap's `detach_ready` acquires
the client-pullable history lease by encoding **every** retained history page
into owned records, synchronously, in one actor turn, on the attach critical
path. Measured at 200x50:

| byte limit | rows retained | `detach_ready` | transient host bytes |
|---|---|---|---|
| engine default | 229 | 2 us | ~0 |
| 2 MiB | 943 | 8 ms | 2.3 MB |
| 4 MiB | 2133 | 22 ms | 6.1 MB |
| 10 MiB | 5703 | 65 ms | 17.4 MB |
| 32 MiB | 19031 | 222 ms | 47.7 MB |

Retained history and attach latency therefore trade directly, per pane, per
attach, on the single server thread.

## Decision

`TerminalActor::build` calls `set_scrollback_max_bytes(MAX_SCROLLBACK_BYTES)`
immediately after constructing the canonical terminal.
`MAX_SCROLLBACK_BYTES` is 2 MiB.

`defaults.history-limit` keeps its line semantics and its 50000 default. No
config key changes, gains a sibling, or changes units.

## Rationale

Two MiB is the smallest value that is a real bound at every grid width — the
engine floors its own byte limit at two standard pages, so anything lower is
indistinguishable from leaving the default in place — while keeping the
per-pane history lease inside single-digit milliseconds. It roughly triples
real retained history on a wide grid, and it makes the ceiling phux's stated
decision rather than a constant inherited from an engine constructor.

The bound is expressed in bytes rather than lines because bytes are what the
host actually spends. A row's cost depends on width, styles, graphemes and
hyperlinks, so a line limit bounds memory only for one particular kind of
content; a byte limit bounds it for all of them.

## Tradeoffs

Retention costs attach latency and peak RSS, permanently. Measured on an
isolated server with four panes each carrying 60k lines of output at 188
columns, fresh -> seeded -> attached -> detached -> settled RSS:

| build | fresh | seeded | attached | settled | client handshake |
|---|---|---|---|---|---|
| no explicit byte limit | 15.7 MB | 22.7 MB | 58.6 MB | 58.6 MB | 0.2 ms + 46 ms |
| 2 MiB | 15.7 MB | 27.3 MB | 89.8 MB | 89.8 MB | 0.1 ms + 122 ms |

RSS does not come back after detach in either case: the engine's history
records are freed but the host allocator keeps the pages, so peak is what
matters, not steady state. Choosing 2 MiB therefore spends about 31 MB of
permanent high-water RSS and about 75 ms of four-pane attach latency to turn
295 retained rows into 943. Raising it further multiplies both.

Lifting the ceiling toward herdr's 10 MiB is a one-constant change, but it is
gated on the engine leasing history lazily instead of materialising it at
READY. Until then the cost curve above is the whole argument.

## Alternatives considered

- **Leave it alone.** Cheapest attach, lowest RSS, but `history-limit` stays
  a value the engine ignores and a 200-column pane keeps 295 rows of history.
- **Make `history-limit` mean bytes, or add a `history-bytes` sibling.** A
  breaking config change and a second knob for a bound the user cannot
  usefully tune, because its real cost is attach latency rather than memory.
  Revisit if and when the history lease becomes lazy.
- **Bound `DetachOptions` instead of retention.** The engine treats an
  exceeded detach budget as a hard error rather than a truncation, so a pane
  with deep history would fail its bootstrap instead of shipping less history.
- **Publish `BOOTSTRAP_READY` before acquiring the lease.** The engine's
  `detach_ready` consumes a capture that holds the terminal's mutation
  exclusion, so deferring it would stall live output for every client. This is
  the right long-term shape and belongs with the engine work in
  `phux-slogic`.
