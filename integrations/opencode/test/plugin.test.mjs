import assert from "node:assert/strict";
import test from "node:test";

import PhuxPlugin, {
  createPhuxPlugin,
  DEFAULT_SHORT_TIMEOUT_MS,
  MAX_MODEL_BYTES,
  MAX_MODEL_LINES,
  PhuxCli,
  PhuxPlugin as NamedPhuxPlugin,
} from "../dist/index.js";

const screen = {
  schema_version: 1,
  pane: 7,
  cols: 80,
  rows: 2,
  cursor: null,
  lines: ["hello", "prompt"],
  scrollback: ["older"],
};

function completed(stdout = "", exitCode = 0) {
  return { termination: "completed", exitCode, stdout, stderr: "" };
}

function toolContext(sessionID = "public-session") {
  return { sessionID, messageID: "message", agent: "build", id: "call" };
}

async function activate(options = {}) {
  const tools = {};
  const hooks = {};
  let disposed = 0;
  const plugin = createPhuxPlugin(options);
  const cleanup = await plugin.setup({
    options: {},
    tool: {
      async transform(callback) {
        callback({ add(definition) { tools[definition.name] = definition; } });
        return { async dispose() { disposed += 1; } };
      },
    },
    session: {
      async hook(name, callback) {
        hooks[name] = callback;
        return { async dispose() { disposed += 1; } };
      },
    },
    event: {
      subscribe({ signal }) {
        return {
          async *[Symbol.asyncIterator]() {
            await new Promise((resolve) => signal.addEventListener("abort", resolve, { once: true }));
          },
        };
      },
    },
  });
  return {
    tools,
    hooks,
    async dispose() {
      await cleanup();
      assert.equal(disposed, 2, "V2 registrations must be released on plugin cleanup");
    },
  };
}

test("exports one named OpenCode V2 plugin and registers six structural tools", async () => {
  assert.equal(PhuxPlugin, NamedPhuxPlugin);
  assert.equal(PhuxPlugin.id, "phux.terminal");
  assert.equal(typeof PhuxPlugin.setup, "function");

  let calls = 0;
  const active = await activate({
    cli: new PhuxCli({ runner: async () => { calls += 1; return completed(); } }),
    env: { PHUX_CONTEXT_AWARENESS: "0" },
  });
  assert.deepEqual(Object.keys(active.tools).sort(), [
    "phux_create",
    "phux_list",
    "phux_run",
    "phux_send_keys",
    "phux_snapshot",
    "phux_wait",
  ]);
  for (const [name, definition] of Object.entries(active.tools)) {
    assert.equal(definition.name, name);
    assert.equal(definition.input.type, "object");
    assert.equal(definition.input.additionalProperties, false);
    assert.equal(typeof definition.execute, "function");
  }
  assert.equal(typeof active.hooks.context, "function");
  assert.equal(calls, 0);
  await active.dispose();
  assert.equal(calls, 0);
});

test("V2 tools preserve target precedence, command shape, deadlines, and bounded results", async () => {
  const requests = [];
  const cli = new PhuxCli({
    executable: "/opt/bin/phux",
    socket: "/tmp/phux.sock",
    runner: async (request) => {
      requests.push(request);
      switch (request.args[0]) {
        case "ls":
          return completed(JSON.stringify({ schema_version: 1, sessions: [{ name: "shared", windows: 1, attached: false }] }));
        case "new":
          return completed(JSON.stringify({ session: "made", terminal_id: 44 }));
        case "snapshot":
          return completed(JSON.stringify(screen));
        case "send-keys":
          return completed();
        case "run":
          return completed(JSON.stringify({ command: request.args.at(-1), exit_code: 0, output: "x".repeat(50_000), duration_ms: 12, truncated: false }));
        case "wait":
          return completed(JSON.stringify(screen));
        case "agent":
          return completed(JSON.stringify({ schema_version: 1, agents: [] }));
        default:
          throw new Error(`unexpected args: ${request.args.join(" ")}`);
      }
    },
  });
  const active = await activate({ cli, env: { PHUX_TARGET: "@9", PHUX_CONTEXT_AWARENESS: "0" } });
  const context = toolContext();

  const listed = await active.tools.phux_list.execute({}, context);
  assert.equal(listed.metadata.count, 1);
  await active.tools.phux_snapshot.execute({}, context);
  await active.tools.phux_snapshot.execute({ target: "@10" }, context);
  await active.tools.phux_create.execute({ name: "made" }, context);
  await active.tools.phux_send_keys.execute({ keys: ["C-c", "literal"] }, context);
  const run = await active.tools.phux_run.execute({ command: "printf '%s' one two" }, context);
  const waited = await active.tools.phux_wait.execute({}, context);

  const snapshots = requests.filter((request) => request.args[0] === "snapshot");
  assert.equal(snapshots[0].args.at(-1), "@9");
  assert.equal(snapshots[1].args.at(-1), "@10");
  const send = requests.find((request) => request.args[0] === "send-keys");
  assert.deepEqual(send.args.slice(-3), ["@44", "C-c", "literal"]);
  const runRequest = requests.find((request) => request.args[0] === "run");
  assert.equal(runRequest.args.at(-2), "@44");
  assert.equal(runRequest.args.at(-1), "printf '%s' one two");
  assert.equal(runRequest.timeoutMs, undefined);
  assert.equal(Buffer.byteLength(run.content.split("\n").slice(1).join("\n")), MAX_MODEL_BYTES);
  assert.ok(run.content.split("\n").length <= MAX_MODEL_LINES + 1);
  assert.match(run.content, /OpenCode adapter truncated terminal output/);
  assert.equal(run.metadata.modelOutputTruncated, true);
  const waitRequest = requests.find((request) => request.args[0] === "wait");
  assert.equal(waitRequest.args.includes("--until"), false);
  assert.equal(waitRequest.args.includes("--idle"), false);
  assert.equal(waitRequest.args.includes("--timeout"), false);
  assert.equal(waitRequest.timeoutMs, undefined);
  assert.equal(waited.metadata.outcome, "satisfied");
  assert.equal(snapshots[0].timeoutMs, DEFAULT_SHORT_TIMEOUT_MS);
  await active.dispose();
});

test("V2 context hook keeps one stable fleet part and advances only on changes", async () => {
  let state = "working";
  let calls = 0;
  const cli = new PhuxCli({ runner: async (request) => {
    calls += 1;
    assert.deepEqual(request.args.slice(0, 3), ["agent", "list", "--json"]);
    return completed(JSON.stringify({
      schema_version: 1,
      agents: [{
        terminal: "@7",
        session: "shared",
        window: "window-0",
        agent: { id: "codex", label: "Codex", kind: "codex" },
        state,
        confidence: 1,
        attention: "normal",
        title: "do not inject screen title",
        cwd: "/repo",
        sources: [{ kind: "screen", signal: "secret screen evidence", confidence: 1, observed: "contents" }],
        explanation: "screen contents",
      }],
    }));
  } });
  const active = await activate({ cli, env: { PHUX_TARGET: "@7", PHUX_TERMINAL_ID: "65" } });

  const first = { sessionID: "session-one", system: [], messages: [], tools: {}, agent: "build", model: {} };
  await active.hooks.context(first);
  assert.equal(first.system.length, 1);
  assert.match(first.system[0].text, /kind="checkpoint" seq="1"/);
  assert.match(first.system[0].text, /"self":"@65"/);
  assert.doesNotMatch(first.system[0].text, /do not inject screen title|secret screen evidence|"contents"/);

  const unchanged = { ...first, system: [] };
  await active.hooks.context(unchanged);
  assert.equal(unchanged.system[0].text, first.system[0].text, "unchanged context remains an exact cacheable suffix");
  state = "idle";
  const changed = { ...first, system: [] };
  await active.hooks.context(changed);
  assert.match(changed.system[0].text, /kind="delta" seq="2"/);
  assert.match(changed.system[0].text, /"state":"idle"/);
  assert.equal(calls, 3);
  await active.dispose();
});

test("context awareness can be disabled without probing phux", async () => {
  let calls = 0;
  const active = await activate({
    cli: new PhuxCli({ runner: async () => { calls += 1; return completed(); } }),
    env: { PHUX_CONTEXT_AWARENESS: "0" },
  });
  const event = { sessionID: "off", system: [], messages: [], tools: {}, agent: "build", model: {} };
  await active.hooks.context(event);
  assert.deepEqual(event.system, []);
  assert.equal(calls, 0);
  await active.dispose();
});

test("public V2 schemas are closed and encode required and bounded arguments", async () => {
  const active = await activate({
    cli: new PhuxCli({ runner: async () => completed() }),
    env: { PHUX_CONTEXT_AWARENESS: "0" },
  });
  assert.deepEqual(active.tools.phux_run.input.required, ["command"]);
  assert.equal(active.tools.phux_run.input.properties.command.pattern, "\\S");
  assert.equal(active.tools.phux_wait.input.properties.timeout_seconds.minimum, 1);
  assert.equal(active.tools.phux_wait.input.additionalProperties, false);
  await assert.rejects(
    active.tools.phux_wait.execute({ target: "@1", until: "done", idle_ms: 10 }, toolContext()),
    /either until or idle_ms/,
  );
  await active.dispose();
});
