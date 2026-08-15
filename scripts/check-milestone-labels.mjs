#!/usr/bin/env node
// Verify every non-closed bead carries exactly one release-milestone label
// (`rc-1.0` or `post-1.0`), by querying the LIVE Dolt store through `bd`.
//
// WHY THIS EXISTS: phux-i7vu labelled all 21 then-unlabelled open beads and
// wrote down the criterion (rc-1.0 if it touches or falsifies a surface
// ADR-0071 point 1 freezes; post-1.0 otherwise). Nothing kept that true. The
// tracker's own milestone view -- "what is left for 1.0" -- is a label query,
// so an unlabelled bead is not merely untriaged, it is INVISIBLE to the
// query. A milestone view that silently undercounts is worse than no view,
// because people plan against it.
//
// WHY IT DOES NOT READ .beads/issues.jsonl: that file is a passive export,
// and in this repo it is also DELIBERATELY SCRUBBED -- commit 51a33f4c moved
// private beads out of the public repo and slimmed the tracked JSONL, and
// `export.auto` does not make it a mirror. Measured on 2026-08-15: the export
// held 785 records while the live store held 929. A check reading the export
// would therefore be wrong in BOTH directions -- passing while the store has
// unlabelled beads, failing on records the store no longer has. A gate that
// can be wrong in both directions trains people to ignore it, so this one
// reads the store or reads nothing, and says which in its own output.
//
// WHY IT IS NOT IN `just ci`: CI checks out the repo and gets no Dolt store
// at all (`.beads/dolt/` and `.beads/embeddeddolt/` are gitignored). Verified
// against a store-less checkout: `bd list` exits 1 with "no beads database
// found". There is nothing authoritative for CI to query, so this is a local,
// advisory gate -- `just milestone-check`, run at session close. Making it a
// required check would only reintroduce the JSONL fallback this exists to
// avoid.
//
// WHY "NON-CLOSED" AND NOT A STATUS ALLOWLIST: the obvious query,
// `bd list --status open,in_progress`, is what phux-i7vu verified with, and
// it has a hole. The store also holds `deferred` and `routed` beads -- both
// live work, and `routed` is not even in `bd list --help`'s documented status
// vocabulary. Any hardcoded allowlist silently drops whatever bd adds next,
// which is the same undercount in a new place. So this enumerates every bead
// and treats "not closed" as in scope. Relatedly: repeating `-s/--status`
// SILENTLY OVERWRITES rather than unioning (bd's own help says so), which is
// its own way to undercount by 39 beads without any error.
//
// FAILURE POLICY: an unlabelled or double-labelled non-closed bead is a hard
// failure (exit 1). No `bd` on PATH, or no local store, is SKIPPED and exit 0
// -- but it prints why, and it never prints a verdict about labels it could
// not check.

import { execFileSync } from "node:child_process";
import { readFileSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const PREFIX = "check-milestone-labels";
const MILESTONES = ["rc-1.0", "post-1.0"];
const root = dirname(dirname(fileURLToPath(import.meta.url)));

function skip(reason, detail) {
  process.stdout.write(`${PREFIX}: SKIPPED (${reason})\n`);
  process.stdout.write(
    `${PREFIX}: this check reads the live Dolt store and deliberately does NOT\n` +
      `${PREFIX}: fall back to .beads/issues.jsonl, which is a passive, scrubbed\n` +
      `${PREFIX}: export. No verdict about milestone labels is implied either way.\n`,
  );
  if (detail) {
    for (const line of detail.trim().split("\n")) {
      process.stdout.write(`${PREFIX}:   ${line}\n`);
    }
  }
  process.exit(0);
}

function bd(args) {
  return execFileSync("bd", args, {
    cwd: root,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    maxBuffer: 256 * 1024 * 1024,
  });
}

// `bd` is optional tooling; a contributor without it is not a failure.
let version;
try {
  version = bd(["version"]).trim();
} catch {
  skip("no `bd` on PATH");
}

// `bd info` is the provenance source: it names the store this check read.
// Resolution walks up from the worktree, so in a linked worktree this is the
// MAIN repository's store, not a per-worktree copy. Printing it is the point
// -- the reader must be able to see exactly what was queried.
let info;
try {
  info = bd(["info"]);
} catch (error) {
  skip("no local beads store", String(error.stderr ?? error.message));
}

const field = (name) => (info.match(new RegExp(`^${name}:\\s*(.+)$`, "m")) ?? [])[1]?.trim();
const storePath = field("Database") ?? "(unreported)";
const storeMode = field("Mode") ?? "(unreported)";
const storeCount = field("Issue Count") ?? "(unreported)";

// --limit 0 is unlimited; the default is 50 and would silently truncate.
// The three --include-* flags defeat bd's default hiding of gate,
// infrastructure, and template beads -- this gate must not inherit someone
// else's idea of what is worth showing.
let issues;
try {
  issues = JSON.parse(
    bd([
      "list",
      "--all",
      "--limit",
      "0",
      "--include-gates",
      "--include-infra",
      "--include-templates",
      "--json",
    ]),
  );
} catch (error) {
  process.stderr.write(`${PREFIX}: could not read the beads store\n${error.stderr ?? error.message}\n`);
  process.exit(1);
}

if (!Array.isArray(issues)) {
  process.stderr.write(`${PREFIX}: expected a JSON array from \`bd list --json\`\n`);
  process.exit(1);
}

const live = issues.filter((issue) => issue.status !== "closed");
const tally = (values) =>
  [...values.reduce((counts, value) => counts.set(value, (counts.get(value) ?? 0) + 1), new Map())]
    .sort((a, b) => b[1] - a[1] || String(a[0]).localeCompare(String(b[0])))
    .map(([value, count]) => `${value} ${count}`)
    .join(", ");

// Report the export's divergence from the store rather than trusting it. A
// reader who sees these two numbers differ cannot mistake the file for the
// tracker.
let exportNote = ".beads/issues.jsonl absent";
try {
  const path = join(root, ".beads/issues.jsonl");
  const records = readFileSync(path, "utf8").split("\n").filter((line) => line.trim() !== "").length;
  exportNote =
    `.beads/issues.jsonl holds ${records} records (store holds ${storeCount}); ` +
    `mtime ${statSync(path).mtime.toISOString()} is a checkout artifact, not an export time`;
} catch {
  // An absent export is fine: this check never reads it for the verdict.
}

process.stdout.write(
  `${PREFIX}: read the LIVE beads store; the tracked JSONL export was NOT consulted\n` +
    `${PREFIX}:   bd:            ${version}\n` +
    `${PREFIX}:   store:         ${storePath} (mode: ${storeMode})\n` +
    `${PREFIX}:   records:       ${storeCount} total, ${live.length} non-closed\n` +
    `${PREFIX}:   statuses:      ${tally(live.map((issue) => issue.status))}\n` +
    `${PREFIX}:   types:         ${tally(live.map((issue) => issue.issue_type))}\n` +
    `${PREFIX}:   export (skipped): ${exportNote}\n`,
);

const labelsOf = (issue) => MILESTONES.filter((label) => (issue.labels ?? []).includes(label));
const missing = live.filter((issue) => labelsOf(issue).length === 0);
const doubled = live.filter((issue) => labelsOf(issue).length > 1);

const describe = (issue) =>
  `${PREFIX}:   ${issue.id}  [${issue.status}/${issue.issue_type}]  ${issue.title ?? ""}\n`;

if (missing.length === 0 && doubled.length === 0) {
  const rc = live.filter((issue) => (issue.labels ?? []).includes("rc-1.0")).length;
  process.stdout.write(
    `${PREFIX}: OK -- all ${live.length} non-closed beads carry exactly one milestone ` +
      `label (${rc} rc-1.0, ${live.length - rc} post-1.0)\n`,
  );
  process.exit(0);
}

if (missing.length > 0) {
  process.stderr.write(
    `${PREFIX}: ${missing.length} non-closed bead(s) carry neither rc-1.0 nor post-1.0, ` +
      `so the 1.0 cut line undercounts by that much:\n`,
  );
  for (const issue of missing) process.stderr.write(describe(issue));
}

if (doubled.length > 0) {
  process.stderr.write(`${PREFIX}: ${doubled.length} non-closed bead(s) carry BOTH milestone labels:\n`);
  for (const issue of doubled) process.stderr.write(describe(issue));
}

process.stderr.write(
  `${PREFIX}: label each with \`bd update <id> --add-label rc-1.0\` (or post-1.0).\n` +
    `${PREFIX}: criterion (phux-i7vu, from ADR-0071 point 1): rc-1.0 when it changes a\n` +
    `${PREFIX}: frozen surface -- CLI grammar, exit codes, --json documents, config\n` +
    `${PREFIX}: schema, action/hook/widget vocabulary, MCP tool names and arguments,\n` +
    `${PREFIX}: documented file and socket locations -- or when leaving it undone makes\n` +
    `${PREFIX}: one of those surfaces wrong at the freeze. post-1.0 otherwise.\n`,
);
process.exit(1);
