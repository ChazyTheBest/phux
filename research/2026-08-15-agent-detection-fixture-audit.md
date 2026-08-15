---
audience: contributors, agents
stability: evolving
last-reviewed: 2026-08-15
---

# Agent-detection fixture audit, and the checklist a re-capture must follow

**TL;DR.** An audit of all fifteen committed detection fixtures, recording what
each one actually depicts rather than what its filename claims. Four of the
five `idle_prompt.txt` fixtures are startup splash screens, not post-turn idle,
so the evidence base does not contain the condition a positive `idle` rule
would assert — confirmed firsthand here, not inherited. It also records a
capture finding with teeth: Claude Code's busy OSC 0 title glyphs changed
between 2.1.207 and 2.1.228, which silently killed the `title-busy-spinner`
rule while every test stayed green. This note is the audit and the checklist;
**the re-capture itself remains outstanding** (phux-w7z2.40).

## Why this exists

phux-w7z2.28 asked for a positive `idle` rule and was ruled against on
evidence. Part of that evidence was a claim about the fixture base: that most
`idle_prompt.txt` files do not depict post-turn idle at all. That claim was
load-bearing for a decision, and it lived only in an issue tracker. This note
moves it into the repository, restated from a direct reading of every fixture,
so the next person to reopen the question argues with the files rather than
with a memory of them.

ADR-0046's Tradeoffs already record what authoring rules against an imagined
TUI costs: the first detection draft was written against a Claude TUI nobody
had captured, and every screen rule in it matched nothing, silently. The
audit below is the cheap standing defence against a repeat.

## What each committed fixture actually shows

Fifteen fixtures, five kinds, under
`crates/phux-server/src/agent_detect/fixtures/<kind>/`. Viewport is 80 columns
in all of them; row counts vary between 18 and 25, which is itself a finding
(see the checklist).

### The `idle_prompt.txt` set — only one depicts post-turn idle

| Kind | What it actually depicts | Post-turn idle? |
|---|---|---|
| `claude` | Startup splash: the `▐▛███▜▌` banner, `Claude Code v2.1.207`, empty transcript, empty prompt box | No — startup |
| `codex` | Startup splash: the `>_ OpenAI Codex (v0.145.0)` welcome box, usage-limit notice, `Find and fix a bug in @filename` placeholder | No — startup |
| `opencode` | Startup splash: the large ASCII `opencode` logo and an empty composer | No — startup |
| `omp` | An *interrupted* run — the tool panel ends in `KeyboardInterrupt: Execution interrupted` | No — interrupted |
| `pi` | A completed answer in the transcript above a returned prompt | **Yes** |

So for four of five kinds, the file named `idle_prompt.txt` is evidence about
what the CLI looks like *before it has ever done anything*, which is a
different screen from the one an `idle` rule would need to match. `pi` is the
sole exception.

This matters beyond the idle question: a splash screen is also the least
representative screen for testing that a `working` or `blocked` rule does
*not* fire, because it shares almost no chrome with a mid-session screen.

### The `working` set

| Kind | Discriminating evidence on screen |
|---|---|
| `claude` | `✻ Kneading… (1s · thinking with high effort)` status line. Top and bottom chrome are otherwise identical to the idle fixture |
| `codex` | `• Working (0s • esc to interrupt)` footer |
| `opencode` | `⬝⬝⬝⬝⬝⬝⬝⬝  esc interrupt` footer |
| `omp` | `⣷ running` in the tool-panel title, plus a `⠏ … ⟦esc⟧` spinner row |
| `pi` | Nothing structural below the transcript — the after-last-rule region is identical to its idle fixture |

`pi` is the documented reason a screen-derived `idle` rule is not authorable
for that kind: its idle and working screens differ only in transcript prose.
The `claude` pair is close behind — the prompt box and the two status rows are
byte-identical in both, and only the mid-screen elapsed-status line separates
them.

### The `blocked` set

| Kind | Discriminating evidence on screen |
|---|---|
| `claude` | `Do you want to proceed?` stem plus a `❯ 1. Yes` numbered option list |
| `codex` | Numbered approval options ending `Press enter to confirm or esc to cancel` |
| `opencode` | `Allow once   Allow always   Reject` action row |
| `omp` | `❯ Approve` / `Deny` with an `up/down navigate  enter select` hint |
| `pi` | `Trust project folder?` with a `Do not trust` option list |

Every `blocked` fixture depicts a genuine modal awaiting input, so this is the
one state whose evidence base is sound across all five kinds. Note that `pi`'s
is a *trust prompt*, which appears once per folder at startup rather than
mid-turn — adequate for the rule it backs, but not evidence about mid-turn
tool approval in `pi`.

## The capture finding this audit turned up

**Claude Code's busy title glyphs drifted, and nothing noticed.**

`claude.toml`'s `title-busy-spinner` is the manifest's highest-priority rule
and its primary `working` signal. It shipped matching `^[⠂⠐]` — U+2802 and
U+2810, braille, verified against 2.1.207.

The raw capture committed at
`research/2026-08-12-osc-9-4-claude-code/claude-title-enabled.rawcap`
(Claude Code 2.1.228) contains twelve OSC 0 titles carrying exactly three
distinct prefixes:

- `U+2733` EIGHT SPOKED ASTERISK — quiet, unchanged
- `U+25D0` CIRCLE WITH LEFT HALF BLACK — busy
- `U+25D1` CIRCLE WITH RIGHT HALF BLACK — busy

No braille appears anywhere in that capture. On 2.1.228 the rule therefore
matched **nothing**, and Claude's primary working signal was dead.

Two things kept this invisible:

1. `working` was still detected, by the two lower-priority backstops added
   later (`osc-progress-working` and `screen-status-elapsed-backstop`). The
   symptom was masked by defence in depth working exactly as intended.
2. Every test for the rule built its input from `CLAUDE_TITLE_BUSY_A/B`,
   constants written from the same belief as the rule. Rule and test agreed
   with each other while both had drifted off the CLI, so the suite was green
   throughout. **A test that asserts a rule against a restatement of itself
   cannot detect drift.**

Both are now fixed: the rule matches the union of the two known glyph pairs,
and a new test
(`every_busy_title_in_the_committed_capture_reads_as_working`) replays the OSC
0 titles out of the committed capture, so the rule is answerable to bytes the
CLI actually emitted. It fails if the regex is reverted — verified, not
assumed.

**The generalisable lesson:** detection rules key on the most cosmetic,
least-contractual part of an agent CLI's output. They rot on the CLI's release
cadence, not on phux's, and they rot silently. Re-capture is maintenance, not
a one-off.

## Checklist for a re-capture

The deliverable of a capture session is the capture *plus* a written statement
of what it shows. A capture whose provenance nobody recorded is evidence
nobody can safely change a rule against.

For each kind, capture:

- [ ] **Post-turn idle** — after a real turn has completed. Not startup. If
      startup is captured too, name it `startup_splash.txt`, not
      `idle_prompt.txt`.
- [ ] **Three to four `working` frames**, spread across a single long turn,
      not one. Animated chrome means one frame is a sample of an unknown
      rotation — the 2.1.207 claude capture caught one verb ("Kneading") out
      of a rotation nobody enumerated, which is why its backstop rule had to
      match by structure rather than by literal.
- [ ] **Blocked**, mid-turn where the CLI has a mid-turn approval, so it is
      not only the once-per-folder trust prompt.
- [ ] **A fixed viewport, recorded in the note.** Today's fixtures vary from
      18 to 25 rows with no record of why, which makes any region-window
      reasoning (`bottom-lines(N)`) unreproducible.
- [ ] **The OSC 0 title alongside each screen**, and the OSC 9;4 payload if
      the CLI emits one. The `.txt` fixture format is rendered grid text and
      structurally cannot hold either. This is what made the glyph drift above
      invisible for a whole release cycle.
- [ ] **The CLI version**, recorded in the note and in the manifest comment
      for any rule the capture backs.

Rules of the exercise:

- **Do not pre-commit to the conclusion.** "These two screens are
  byte-identical and no honest rule exists" is a valid and valuable outcome,
  and for several kinds it is the *correct* one. Authoring a rule is permitted
  only where the captures support one.
- **Redaction is byte-length-preserving.** The committed `.rawcap` files
  replace the signed-in account with `user@redacted.example`, chosen to be
  identical in length so every column position, wrap point and escape-sequence
  offset survives. Preserve that property in anything re-captured; a
  length-changing redaction invalidates the capture as evidence about layout.
  See `research/2026-08-12-osc-9-4-claude-code.md` for the full rationale.
- **Check any `idle` rule against the `visible-idle` bypass.** A rule carrying
  `visible-idle = true` skips the working -> idle hold for its whole manifest
  (`agent_detect/mod.rs`, `settle_idle`), and `rules.rs` ORs the flag across
  every *matched* rule before the state-priority race — so an idle rule that
  loses the race has still armed the bypass. The one shipped exception,
  `claude`'s `osc-progress-idle`, is safe only because the `osc-progress`
  region holds a single latest payload, making it mutually exclusive with its
  `working` twin. A screen-derived idle rule has no such guarantee.

## Status

This note is the audit and the checklist. **The re-capture is not done.**
`gemini`, `cursor-agent` and `aider` are not installed in this environment and
cannot be captured here at all; `claude`, `codex`, `opencode` and `goose` are
present. The remaining work is a driven capture session per the checklist
above, which is what phux-w7z2.40 stays open for.
