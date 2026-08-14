---
audience: humans, agents, consumers, contributors
stability: evolving
last-reviewed: 2026-08-14
---

# Claude Code integration

**TL;DR.** The first-party `phux` Claude Code plugin exposes the authoritative
`phux mcp` tool server and publishes lifecycle identity and attention events for
Claude sessions running inside phux panes. It is versioned independently from
the phux binaries and distributed through this repository's Claude marketplace.

## Install

Install `phux` and `phux-mcp` first, then register and install the marketplace
plugin:

```sh
claude plugin marketplace add phall1/phux
claude plugin install phux@phux
```

The plugin requires Claude Code 2.1.232 or a compatible newer release, and phux
0.16.0 or newer on `PATH`. Start a local phux server before using MCP tools.

For development, load the checked-out package directly:

```sh
cd integrations/claude
npm ci
npm run gates
claude --plugin-dir .
```

## Runtime contract

The plugin contributes two native Claude components:

- `.mcp.json` launches `phux mcp`, so Claude receives the same version-matched
  tool catalog as every other MCP host.
- Lifecycle hooks declare `name=claude` and `kind=claude` once at session start,
  call `phux ask` for permission and elicitation prompts, and clear the record at
  session end.

Hooks are best effort and silent. They act only when `PHUX_TERMINAL_ID` identifies
Claude's own pane, never guess from phux focus, and never write a lifecycle
`state`. The server-side Claude detector remains authoritative for working and
blocked state.

The built-in `phux agent install-claude` shim remains available for users who
want plain `claude` to create or enter a phux session automatically. The plugin
does not replace that launch behavior; it provides native tools and lifecycle
integration for Claude sessions regardless of how they were started.

## Validation and versioning

`integrations/claude/package.json`, the plugin manifest, and the repository
marketplace entry share one component version. CI runs Anthropic's strict plugin
validator, package-shape tests, exact hook argv tests, and a high-severity npm
audit. Release Please owns `claude-plugin-vX.Y.Z`; the component release workflow
archives the exact tagged plugin and publishes the draft GitHub release only
after validation.
