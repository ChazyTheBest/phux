# phux for Claude Code

This first-party Claude Code plugin exposes the bundled `phux mcp` server and
publishes lifecycle metadata for Claude sessions running inside phux panes. It
requires `phux` and `phux-mcp` on `PATH` and a running local phux server.

```sh
claude plugin marketplace add phall1/phux
claude plugin install phux@phux
```

The MCP server starts with the plugin and contributes the authoritative phux
tool catalog. Lifecycle hooks declare Claude's identity once at session start,
emit attention asks for permission and elicitation prompts, and clear the
declaration at session end. The server-side Claude detector remains authoritative
for working and blocked state; the plugin never declares a state.

For development:

```sh
npm ci
npm run gates
claude --plugin-dir .
```
