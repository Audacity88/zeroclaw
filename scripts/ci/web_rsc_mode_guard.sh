#!/usr/bin/env bash
set -euo pipefail

repo_root="${ZEROCLAW_RSC_GUARD_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
expires="2026-09-01"
today="$(date -u +%F)"

fail() {
  echo "web-rsc-mode-guard: $*" >&2
  exit 1
}

if [[ -n "${ZEROCLAW_RSC_GUARD_EXPIRES_OVERRIDE:-}" ]]; then
  [[ -n "${ZEROCLAW_RSC_GUARD_ROOT:-}" ]] || fail "expiry override is fixture-only"
  expires="$ZEROCLAW_RSC_GUARD_EXPIRES_OVERRIDE"
fi
if [[ -n "${ZEROCLAW_RSC_GUARD_TODAY:-}" ]]; then
  [[ -n "${ZEROCLAW_RSC_GUARD_ROOT:-}" ]] || fail "current-date override is fixture-only"
  today="$ZEROCLAW_RSC_GUARD_TODAY"
fi

node - "$repo_root" "$today" "$expires" <<'NODE'
const fs = require("node:fs");
const { builtinModules } = require("node:module");
const path = require("node:path");

const repoRoot = path.resolve(process.argv[2]);
const today = process.argv[3];
const expires = process.argv[4];
const webRoot = path.join(repoRoot, "web");
const webSourceRoot = path.join(webRoot, "src");
const ciWorkflowPath = path.join(
  repoRoot,
  ".github",
  "workflows",
  "ci.yml",
);
const expectedGhsa = "GHSA-qwww-vcr4-c8h2";
const expectedDependencyReviewRef =
  "3b139cfc5fae8b618d3eae3675e383bb1769c019";

function fail(message) {
  console.error(`web-rsc-mode-guard: ${message}`);
  process.exit(1);
}

function parseDate(label, value) {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(value)) {
    fail(`invalid ${label} date: ${value}`);
  }

  const [year, month, day] = value.split("-").map(Number);
  const timestamp = Date.UTC(year, month - 1, day);
  if (new Date(timestamp).toISOString().slice(0, 10) !== value) {
    fail(`invalid ${label} date: ${value}`);
  }
  return timestamp;
}

if (parseDate("current", today) >= parseDate("expiry", expires)) {
  fail(`${expectedGhsa} exception expired on ${expires}`);
}

if (!fs.existsSync(ciWorkflowPath)) {
  fail("missing .github/workflows/ci.yml");
}

function workflowJob(lines, name) {
  const start = lines.findIndex((line) => line === `  ${name}:`);
  if (start === -1) {
    fail(`missing ${name} job in .github/workflows/ci.yml`);
  }
  let end = lines.length;
  for (let index = start + 1; index < lines.length; index += 1) {
    if (/^  [A-Za-z0-9_-]+:\s*$/.test(lines[index])) {
      end = index;
      break;
    }
  }
  return { start, end, lines: lines.slice(start, end) };
}

function workflowSteps(lines, job) {
  const stepsKeys = [];
  for (let index = job.start + 1; index < job.end; index += 1) {
    if (/^    steps:\s*(?:#.*)?$/.test(lines[index])) {
      stepsKeys.push(index);
    }
  }
  if (stepsKeys.length !== 1) {
    fail(`${job.lines[0].trim()} must contain exactly one direct steps block`);
  }

  const stepsStart = stepsKeys[0];
  let stepsEnd = job.end;
  for (let index = stepsStart + 1; index < job.end; index += 1) {
    const line = lines[index];
    if (line.trim() && !line.trimStart().startsWith("#") && line.match(/^ */)[0].length <= 4) {
      stepsEnd = index;
      break;
    }
  }

  const starts = [];
  for (let index = stepsStart + 1; index < stepsEnd; index += 1) {
    if (/^      -\s+/.test(lines[index])) {
      starts.push(index);
    }
  }
  return starts.map((start, index) => {
    const end = starts[index + 1] ?? stepsEnd;
    return { start, end, lines: lines.slice(start, end) };
  });
}

function stepFieldValues(lines, step, key) {
  const escapedKey = key.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const values = [];
  const inline = step.lines[0].match(
    new RegExp(`^ {6}-\\s+${escapedKey}:\\s*(.*?)\\s*(?:#.*)?$`),
  );
  if (inline) {
    values.push({ index: step.start, value: inline[1] });
  }
  for (let index = step.start + 1; index < step.end; index += 1) {
    const match = lines[index].match(
      new RegExp(`^ {8}${escapedKey}:\\s*(.*?)\\s*(?:#.*)?$`),
    );
    if (match) {
      values.push({ index, value: match[1] });
    }
  }
  return values;
}

function requireSingleStepField(lines, step, key) {
  const values = stepFieldValues(lines, step, key);
  if (values.length !== 1) {
    fail(`workflow step must contain exactly one direct ${key} field`);
  }
  return values[0];
}

function directStepKeys(lines, step) {
  const keys = [];
  const inline = step.lines[0].match(/^ {6}-\s+([^:#]+):/);
  if (inline) {
    keys.push(inline[1].trim());
  }
  for (let index = step.start + 1; index < step.end; index += 1) {
    const match = lines[index].match(/^ {8}([^ :#][^:#]*):/);
    if (match) {
      keys.push(match[1].trim());
    }
  }
  return keys;
}

function requireAllowedKeys(context, keys, allowed) {
  const unexpected = keys.filter((key) => !allowed.has(key));
  if (unexpected.length > 0) {
    fail(`${context} contains unsupported direct fields: ${unexpected.join(", ")}`);
  }
}

const ciWorkflow = fs.readFileSync(ciWorkflowPath, "utf8");
const ciLines = ciWorkflow.split(/\r?\n/);
if (ciLines.some((line) => /^defaults:\s*(?:#.*)?$/.test(line))) {
  fail("required CI cannot set workflow-wide run defaults");
}
const reviewJob = workflowJob(ciLines, "npm-dependency-review");
const reviewKeys = reviewJob.lines
  .slice(1)
  .map((line) => line.match(/^ {4}([^ :#][^:#]*):/))
  .filter(Boolean)
  .map((match) => match[1].trim());
requireAllowedKeys(
  "npm-dependency-review job",
  reviewKeys,
  new Set(["name", "if", "runs-on", "timeout-minutes", "steps"]),
);
const reviewIf = reviewJob.lines.filter((line) => /^    if:/.test(line));
if (
  reviewIf.length !== 1 ||
  reviewIf[0].trim() !== "if: github.event_name == 'pull_request'"
) {
  fail("npm-dependency-review must run only on pull requests and fail closed");
}
const actionSteps = workflowSteps(ciLines, reviewJob).filter((step) =>
  stepFieldValues(ciLines, step, "uses").some(({ value }) =>
    value.startsWith("actions/dependency-review-action@"),
  ),
);
if (actionSteps.length !== 1) {
  fail("required CI must contain exactly one dependency-review action step");
}
const actionStep = actionSteps[0];
requireAllowedKeys(
  "dependency-review action step",
  directStepKeys(ciLines, actionStep),
  new Set(["name", "uses", "with"]),
);
const actionUses = requireSingleStepField(ciLines, actionStep, "uses").value;
const actionRef = actionUses.match(
  /^actions\/dependency-review-action@([^\s#]+)(?:\s+#.*)?$/,
)?.[1];
if (actionRef !== expectedDependencyReviewRef) {
  fail("dependency-review action must use the approved pinned revision");
}

const withFields = stepFieldValues(ciLines, actionStep, "with");
if (withFields.length !== 1 || withFields[0].value !== "") {
  fail("dependency-review action must contain exactly one with block");
}
const withStart = withFields[0].index;
let withEnd = actionStep.end;
for (let index = withStart + 1; index < actionStep.end; index += 1) {
  const line = ciLines[index];
  if (line.trim() && !line.trimStart().startsWith("#") && line.match(/^ */)[0].length <= 8) {
    withEnd = index;
    break;
  }
}
const withLines = ciLines.slice(withStart + 1, withEnd);
const allowGhsas = withLines
  .map((line) => line.match(/^ {10}allow-ghsas:\s*([^#\r\n]+?)\s*(?:#.*)?$/))
  .filter(Boolean)
  .map((match) => match[1].trim().replace(/^["']|["']$/g, ""));
const severities = withLines
  .map((line) => line.match(/^ {10}fail-on-severity:\s*([^#\r\n]+?)\s*(?:#.*)?$/))
  .filter(Boolean)
  .map((match) => match[1].trim().replace(/^["']|["']$/g, ""));
if (severities.length !== 1 || severities[0] !== "high") {
  fail("dependency-review action must keep fail-on-severity at high");
}
const allAllowGhsas = [
  ...ciWorkflow.matchAll(/^\s*allow-ghsas:\s*([^#\r\n]+?)\s*(?:#.*)?$/gm),
];
if (
  allowGhsas.length !== 1 ||
  allowGhsas[0] !== expectedGhsa ||
  allAllowGhsas.length !== 1
) {
  fail(`dependency-review action must allow exactly ${expectedGhsa}`);
}

const gateJob = workflowJob(ciLines, "gate");
const gateKeys = gateJob.lines
  .slice(1)
  .map((line) => line.match(/^ {4}([^ :#][^:#]*):/))
  .filter(Boolean)
  .map((match) => match[1].trim());
requireAllowedKeys(
  "CI Required Gate job",
  gateKeys,
  new Set(["name", "if", "needs", "runs-on", "timeout-minutes", "steps"]),
);
const gateIf = gateJob.lines.filter((line) => /^    if:/.test(line));
if (
  gateIf.length !== 1 ||
  gateIf[0].trim() !== "if: always()"
) {
  fail("CI Required Gate must always run and fail closed");
}
const gateNeeds = gateJob.lines.find((line) => /^    needs:\s*\[/.test(line));
const gateNeedNames = gateNeeds
  ?.match(/\[([^\]]*)\]/)?.[1]
  .split(",")
  .map((name) => name.trim())
  .filter(Boolean);
if (
  !gateNeedNames ||
  gateNeedNames.filter((name) => name === "npm-dependency-review").length !== 1
) {
  fail("CI Required Gate must depend on npm-dependency-review");
}
const gateSteps = workflowSteps(ciLines, gateJob);
const gateStepMatches = gateSteps.filter(
  (step) =>
    stepFieldValues(ciLines, step, "name").length === 1 &&
    stepFieldValues(ciLines, step, "name")[0].value === "Check results",
);
if (gateStepMatches.length !== 1 || gateSteps[0] !== gateStepMatches[0]) {
  fail("CI Required Gate must start with exactly one Check results step");
}
const gateStep = gateStepMatches[0];
requireAllowedKeys(
  "CI Required Gate check step",
  directStepKeys(ciLines, gateStep),
  new Set(["name", "run"]),
);
const gateRun = requireSingleStepField(ciLines, gateStep, "run");
if (gateRun.value !== "|") {
  fail("CI Required Gate check step must contain exactly one run block");
}
const gateScriptLines = ciLines
  .slice(gateRun.index + 1, gateStep.end)
  .filter((line) => line.trim())
  .map((line) => line.trim());
const requiredReviewResult =
  `if [[ "\${{ github.event_name }}" == "pull_request" && "\${{ needs.npm-dependency-review.result }}" != "success" ]]; then`;
const requiredReviewPrefix = [
  requiredReviewResult,
  'echo "::error::npm dependency review did not complete successfully"',
  "exit 1",
  "fi",
];
if (
  requiredReviewPrefix.some((line, index) => gateScriptLines[index] !== line)
) {
  fail("CI Required Gate must require successful npm dependency review on pull requests");
}

const packagePath = path.join(webRoot, "package.json");
if (!fs.existsSync(packagePath)) {
  fail("missing web/package.json");
}

const pkg = JSON.parse(fs.readFileSync(packagePath, "utf8"));
if (!pkg.dependencies?.["react-router-dom"]) {
  fail("react-router-dom must remain a direct runtime dependency");
}

const dependencySections = [
  "dependencies",
  "devDependencies",
  "optionalDependencies",
  "peerDependencies",
];
const declaredPackages = new Set();
const forbiddenPackages = [];
for (const section of dependencySections) {
  for (const [name, version] of Object.entries(pkg[section] ?? {})) {
    declaredPackages.add(name);
    if (
      name === "react-router" ||
      name.startsWith("react-router/") ||
      name.startsWith("@react-router/") ||
      name === "@vitejs/plugin-rsc" ||
      name.startsWith("react-server-dom-")
    ) {
      forbiddenPackages.push(`${section}:${name}`);
    }
    if (typeof version === "string" && version.startsWith("npm:")) {
      const target = version.slice(4).match(/^(@[^/]+\/[^@]+|[^@]+)(?:@.*)?$/)?.[1];
      if (!target || target === "react-router-dom" || forbiddenSpecifier(target)) {
        forbiddenPackages.push(`${section}:${name}->${version}`);
      }
    }
  }
}

if (forbiddenPackages.length > 0) {
  fail(
    `RSC-capable dependencies require removing the advisory exception: ${forbiddenPackages.join(", ")}`,
  );
}

const scannedExtensions = new Set([
  ".html",
  ".js",
  ".jsx",
  ".mjs",
  ".cjs",
  ".ts",
  ".tsx",
  ".mts",
  ".cts",
  ".mdx",
]);
const ignoredPaths = new Set([
  path.join(webRoot, "dist"),
  path.join(webRoot, "node_modules"),
]);
const nodeModulesRoot = path.join(webRoot, "node_modules");
const nodeBuiltins = new Set(builtinModules.flatMap((name) => [name, `node:${name}`]));
const allowedTransitiveImports = new Set(["@codemirror/theme-one-dark"]);
const forbiddenRscApi = /\b(?:unstable_)?(?:RSCHydratedRouter|RSCStaticRouter|createCallServer|getRSCStream|matchRSCServerRequest|routeRSCServerRequest|reactRouterRSC)\b/;

function forbiddenSpecifier(specifier) {
  return (
    specifier === "react-router" ||
    specifier.startsWith("react-router/") ||
    specifier.startsWith("react-router-dom/") ||
    specifier.startsWith("@react-router/") ||
    specifier === "@vitejs/plugin-rsc" ||
    specifier.startsWith("react-server-dom-")
  );
}

function packageName(specifier) {
  if (specifier.startsWith("@")) {
    return specifier.split("/").slice(0, 2).join("/");
  }
  return specifier.split("/", 1)[0];
}

function requireInsideWebRoot(resolved, relative, specifier) {
  if (resolved !== webRoot && !resolved.startsWith(`${webRoot}${path.sep}`)) {
    fail(`${relative} imports outside the guarded web root: ${specifier}`);
  }
  if (resolved === nodeModulesRoot || resolved.startsWith(`${nodeModulesRoot}${path.sep}`)) {
    fail(`${relative} imports into skipped node_modules: ${specifier}`);
  }
}

function inspectViteAliases(source, relative) {
  if (/\[\s*["'](?:resolve|alias)["']\s*\]\s*:/.test(source)) {
    fail(`${relative} uses a computed resolve or alias property`);
  }
  if (/\.\.\./.test(source)) {
    fail(`${relative} uses an unsupported object spread`);
  }
  const aliasTokens = [...source.matchAll(/\balias\b/g)];
  const aliasBlocks = [...source.matchAll(/\balias\s*:\s*\{([\s\S]*?)\n\s*\},/g)];
  if (aliasTokens.length !== 1 || aliasBlocks.length !== 1) {
    fail(`${relative} uses an unsupported alias declaration`);
  }

  const compact = aliasBlocks[0][1].replace(/\s+/g, "");
  if (!/^["']@["']:path\.resolve\(__dirname,["']\.\/src["']\),?$/.test(compact)) {
    fail(`${relative} must keep the sole @ alias rooted at web/src`);
  }
}

function stripJavaScriptComments(source) {
  const output = [...source];
  const stack = [{ state: "code", templateDepth: null }];
  let index = 0;

  while (index < source.length) {
    const frame = stack[stack.length - 1];
    const char = source[index];
    const next = source[index + 1];

    if (frame.state === "line-comment") {
      if (char === "\n") {
        stack.pop();
      } else {
        output[index] = " ";
      }
      index += 1;
      continue;
    }
    if (frame.state === "block-comment") {
      if (char === "*" && next === "/") {
        output[index] = " ";
        output[index + 1] = " ";
        stack.pop();
        index += 2;
      } else {
        if (char !== "\n") {
          output[index] = " ";
        }
        index += 1;
      }
      continue;
    }
    if (frame.state === "single-quote" || frame.state === "double-quote") {
      const quote = frame.state === "single-quote" ? "'" : '"';
      if (char === "\\") {
        index += 2;
      } else {
        if (char === quote) {
          stack.pop();
        }
        index += 1;
      }
      continue;
    }
    if (frame.state === "template") {
      if (char === "\\") {
        index += 2;
      } else if (char === "`") {
        stack.pop();
        index += 1;
      } else if (char === "$" && next === "{") {
        stack.push({ state: "code", templateDepth: 1 });
        index += 2;
      } else {
        index += 1;
      }
      continue;
    }

    if (char === "/" && next === "/") {
      output[index] = " ";
      output[index + 1] = " ";
      stack.push({ state: "line-comment", templateDepth: null });
      index += 2;
    } else if (char === "/" && next === "*") {
      output[index] = " ";
      output[index + 1] = " ";
      stack.push({ state: "block-comment", templateDepth: null });
      index += 2;
    } else if (char === "'") {
      stack.push({ state: "single-quote", templateDepth: null });
      index += 1;
    } else if (char === '"') {
      stack.push({ state: "double-quote", templateDepth: null });
      index += 1;
    } else if (char === "`") {
      stack.push({ state: "template", templateDepth: null });
      index += 1;
    } else if (frame.templateDepth !== null && char === "{") {
      frame.templateDepth += 1;
      index += 1;
    } else if (frame.templateDepth !== null && char === "}") {
      frame.templateDepth -= 1;
      if (frame.templateDepth === 0) {
        stack.pop();
      }
      index += 1;
    } else {
      index += 1;
    }
  }

  return output.join("");
}

function inspectFile(filePath) {
  const relative = path.relative(webRoot, filePath).split(path.sep).join("/");
  const basename = path.basename(filePath);
  if (
    /^react-router\.config\./.test(basename) ||
    /^entry\.(?:server|rsc)\./.test(basename) ||
    /\.(?:server|rsc)\.[^.]+$/.test(basename)
  ) {
    fail(`${relative} is a server/RSC entry surface`);
  }

  const source = fs.readFileSync(filePath, "utf8");
  const importSource = stripJavaScriptComments(source);
  for (const match of importSource.matchAll(/["'`]([^"'`]+)["'`]/g)) {
    if (forbiddenSpecifier(match[1])) {
      fail(`${relative} contains RSC/server-capable module literal ${match[1]}`);
    }
  }
  const moduleCallPattern = /\b(?:import|require)\s*\(([^)]*)\)/g;
  for (const match of importSource.matchAll(moduleCallPattern)) {
    const argument = match[1].trim();
    if (!/^(?:"[^"]+"|'[^']+'|`[^`$]+`)$/.test(argument)) {
      fail(`${relative} uses a non-literal dynamic module specifier`);
    }
  }
  const specifierPatterns = [
    /(?:^|[;\n])\s*(?:import|export)\b[^;]*?\sfrom\s*["']([^"']+)["']/g,
    /(?:^|[;\n])\s*import\s*["']([^"']+)["']/g,
    /\bimport\s*\(\s*["'`]([^"'`]+)["'`]\s*\)/g,
    /\brequire\s*\(\s*["']([^"']+)["']\s*\)/g,
  ];
  for (const pattern of specifierPatterns) {
    for (const match of importSource.matchAll(pattern)) {
      const specifier = match[1];
      if (forbiddenSpecifier(specifier)) {
        fail(`${relative} imports RSC/server-capable module ${specifier}`);
      }
      if (specifier.startsWith("@/")) {
        const resolved = path.resolve(webSourceRoot, specifier.slice(2));
        requireInsideWebRoot(resolved, relative, specifier);
      } else if (specifier.startsWith(".")) {
        const resolved = path.resolve(path.dirname(filePath), specifier);
        requireInsideWebRoot(resolved, relative, specifier);
      } else if (
        !specifier.startsWith("node:") &&
        !nodeBuiltins.has(specifier) &&
        !declaredPackages.has(packageName(specifier)) &&
        !allowedTransitiveImports.has(packageName(specifier))
      ) {
        fail(`${relative} imports undeclared package or local alias ${specifier}`);
      }
    }
  }

  if (/\bimport\s*\*\s+as\s+\w+\s+from\s*["']react-router-dom["']/.test(source)) {
    fail(`${relative} uses a namespace import from react-router-dom`);
  }
  if (/\b(?:import|require)\s*\(\s*["'`]react-router-dom["'`]\s*\)/.test(source)) {
    fail(`${relative} dynamically imports react-router-dom`);
  }
  if (/\bexport\s*\*\s*from\s*["']react-router-dom["']/.test(source)) {
    fail(`${relative} re-exports the full react-router-dom namespace`);
  }
  if (forbiddenRscApi.test(source) || /["']use server["']|["']react-server["']/.test(source)) {
    fail(`${relative} contains an unstable RSC API or server directive`);
  }
  if (/^vite\.config\./.test(basename)) {
    inspectViteAliases(importSource, relative);
  }
}

function walk(directory) {
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const entryPath = path.join(directory, entry.name);
    if (entry.isDirectory() && ignoredPaths.has(entryPath)) {
      continue;
    }
    if (entry.isSymbolicLink()) {
      fail(`${path.relative(webRoot, entryPath)} is a symbolic link outside the scanned file contract`);
    } else if (entry.isDirectory()) {
      walk(entryPath);
    } else if (entry.isFile() && scannedExtensions.has(path.extname(entry.name))) {
      inspectFile(entryPath);
    }
  }
}

if (!fs.existsSync(webRoot)) {
  fail("missing web directory");
}
walk(webRoot);
NODE

echo "web-rsc-mode-guard: client-only React Router boundary verified through $expires"
