# skills/

The two release binaries compile their agent guides with `include_str!`:

- `phux/SKILL.md` -> `phux --skill[=SCOPE]` and `phux skill [SCOPE]`.
- `phux-mcp/SKILL.md` -> `phux-mcp --skill`.

The text an agent reads therefore belongs to the binary it is driving. There
is no separately shipped copy to fall out of date. The phux source is one
marked superset: region markers derive `quick`, `agent`, `terminal`, and
`full`; bare `--skill` means `full`.

Editing it is editing the CLI's agent-facing contract. Three tests in
`crates/phux/src/help_inventory.rs` hold it to the clap tree:

- every visible top-level verb is mentioned,
- every visible `phux agent` subcommand is mentioned,
- every selector sigil the parser accepts is taught.

The MCP skill is checked against the live `tools/list` catalog and delegates
exact argument schemas to `phux-mcp --schema`, which returns that same catalog.
`just skill-contract` runs both real binaries with poisoned config/socket state
and checks frontmatter, exact output, help discovery, exclusivity, and closed
pipes. Adding a verb, tool, or sigil without teaching it fails CI. Each skill
is a build artifact of its surface, not a hope.

The example skills under `examples/skills/` are separate, hand-maintained
illustrations (CLI + MCP orchestration, the `phux-terminal` read/act loop).
They are examples, not the contract. `phux --skill` and `phux-mcp --skill` win.
