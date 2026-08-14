import { Plugin, type PluginOptions } from "@opencode-ai/plugin";

import { PhuxCli, type PhuxCliOptions } from "../../pi/src/adapter.js";
import {
  PhuxContextAwareness,
  contextAwarenessEnabled,
  normalizeTerminalIdentity,
} from "../../pi/src/awareness.js";
import {
  handleLifecycleEvent,
  OpenCodeLifecycle,
  type OpenCodeLifecycleEvent,
} from "./lifecycle.js";
import { createPhuxTools } from "./tools.js";

export { PhuxCli } from "../../pi/src/adapter.js";
export {
  PhuxContextAwareness,
  contextAwarenessEnabled,
  normalizeTerminalIdentity,
} from "../../pi/src/awareness.js";
export type {
  PhuxContextAwarenessOptions,
  PhuxContextEmission,
  PhuxContextIdentity,
} from "../../pi/src/awareness.js";
export type {
  AgentTargetOptions,
  CreateOptions,
  ExecutionOptions,
  PhuxCliOptions,
  PhuxProbe,
  RunOptions,
  SnapshotOptions,
  WaitOptions,
  WaitOutcome,
} from "../../pi/src/adapter.js";
export {
  boundedResult,
  createPhuxTools,
  DEFAULT_SHORT_TIMEOUT_MS,
  MAX_MODEL_BYTES,
  MAX_MODEL_LINES,
  resolveTarget,
} from "./tools.js";
export type {
  PhuxToolDefinition,
  PhuxToolMetadata,
  PhuxToolRuntime,
  ToolContext,
  ToolResult,
} from "./tools.js";
export { handleLifecycleEvent, OpenCodeLifecycle } from "./lifecycle.js";
export type {
  OpenCodeLifecycleAdapter,
  OpenCodeLifecycleEvent,
  OpenCodeLifecycleOptions,
  OpenCodeLifecycleState,
} from "./lifecycle.js";

/** Plugin settings plus injectable seams for library and contract tests. */
export interface PhuxOpenCodeOptions {
  readonly executable?: string;
  readonly socket?: string;
  readonly lifecycleTimeoutMs?: number;
  readonly contextAwareness?: boolean;
  readonly contextTimeoutMs?: number;
  readonly cli?: PhuxCli;
  readonly env?: NodeJS.ProcessEnv;
  readonly onLifecycleError?: (error: unknown) => void;
}

/** Build a V2 plugin with optional test-only defaults. */
export function createPhuxPlugin(defaults: PhuxOpenCodeOptions = {}): Plugin.Plugin {
  return Plugin.define({
    id: "phux.terminal",
    setup: async (context) => {
      const options = mergeOptions(defaults, context.options);
      const environment = options.env ?? process.env;
      const environmentTarget = readEnvironmentTarget(environment.PHUX_TARGET);
      const cli = options.cli ?? new PhuxCli(cliOptions(options, environment));
      let selectedTarget: string | undefined;
      const currentTarget = (): string | undefined => selectedTarget ?? environmentTarget;
      const lifecycle = new OpenCodeLifecycle({
        cli,
        target: currentTarget,
        ...(options.lifecycleTimeoutMs === undefined ? {} : { timeoutMs: options.lifecycleTimeoutMs }),
        ...(options.onLifecycleError === undefined ? {} : { onError: options.onLifecycleError }),
      });
      const awareness = new PhuxContextAwareness(cli, {
        enabled: options.contextAwareness ?? contextAwarenessEnabled(environment.PHUX_CONTEXT_AWARENESS),
        ...(options.contextTimeoutMs === undefined ? {} : { timeoutMs: options.contextTimeoutMs }),
      });
      const latestContext = new Map<string, string>();
      const contextIdentity = () => {
        const self = normalizeTerminalIdentity(environment.PHUX_TERMINAL_ID);
        const selected = currentTarget();
        return {
          ...(self === null ? {} : { self }),
          ...(selected === undefined ? {} : { selected }),
        };
      };

      const tools = createPhuxTools({
        cli,
        ...(environmentTarget === undefined ? {} : { environmentTarget }),
        getSelectedTarget: () => selectedTarget,
        selectTarget: (target) => {
          selectedTarget = target;
        },
        targetSelected: (toolContext) => {
          void lifecycle.targetSelected(toolContext.sessionID);
        },
      });

      const toolRegistration = await context.tool.transform((draft) => {
        for (const tool of Object.values(tools)) draft.add(tool);
      });
      const contextRegistration = await context.session.hook("context", async (event) => {
        const emission = await awareness.next(event.sessionID, contextIdentity());
        if (emission !== null) latestContext.set(event.sessionID, emission.text);
        const text = latestContext.get(event.sessionID);
        if (text === undefined) return;
        event.system.push({
          type: "text",
          text,
          metadata: { phuxContext: true },
        });
      });

      const controller = new AbortController();
      const eventTask = consumeEvents(
        context.event.subscribe({ signal: controller.signal }),
        lifecycle,
        awareness,
        latestContext,
        options.onLifecycleError,
      );

      return async () => {
        controller.abort();
        await Promise.all([
          toolRegistration.dispose(),
          contextRegistration.dispose(),
          eventTask,
          lifecycle.dispose(),
        ]);
      };
    },
  });
}

export const PhuxPlugin = createPhuxPlugin();
export default PhuxPlugin;

async function consumeEvents(
  events: AsyncIterable<unknown>,
  lifecycle: OpenCodeLifecycle,
  awareness: PhuxContextAwareness,
  latestContext: Map<string, string>,
  onError: ((error: unknown) => void) | undefined,
): Promise<void> {
  try {
    for await (const event of events) {
      if (!isLifecycleEvent(event)) continue;
      await handleLifecycleEvent(lifecycle, event);
      if (event.type === "session.deleted") {
        const info = event.properties.info;
        const sessionID = info !== null && typeof info === "object" ? (info as { readonly id?: unknown }).id : undefined;
        if (typeof sessionID !== "string") continue;
        awareness.delete(sessionID);
        latestContext.delete(sessionID);
      }
    }
  } catch (error) {
    if (!isAbortError(error)) onError?.(error);
  }
}

function isLifecycleEvent(value: unknown): value is OpenCodeLifecycleEvent {
  if (value === null || typeof value !== "object") return false;
  const candidate = value as { readonly type?: unknown; readonly properties?: unknown };
  return typeof candidate.type === "string" && candidate.properties !== null && typeof candidate.properties === "object";
}

function isAbortError(error: unknown): boolean {
  return error instanceof DOMException && error.name === "AbortError";
}

function mergeOptions(defaults: PhuxOpenCodeOptions, configured: PluginOptions): PhuxOpenCodeOptions {
  return { ...defaults, ...(configured as PhuxOpenCodeOptions) };
}

function cliOptions(options: PhuxOpenCodeOptions, environment: NodeJS.ProcessEnv): PhuxCliOptions {
  return {
    ...(options.executable === undefined ? {} : { executable: options.executable }),
    ...(options.socket === undefined ? {} : { socket: options.socket }),
    env: environment,
  };
}

function readEnvironmentTarget(value: string | undefined): string | undefined {
  if (value === undefined || value.trim().length === 0) return undefined;
  if (value.length > 512) throw new RangeError("PHUX_TARGET must be at most 512 characters");
  return value;
}
