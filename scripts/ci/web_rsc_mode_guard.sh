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

const ciWorkflow = fs.readFileSync(ciWorkflowPath, "utf8");
const ciLines = ciWorkflow.split(/\r?\n/);
const reviewJob = workflowJob(ciLines, "npm-dependency-review");
const reviewIf = reviewJob.lines.filter((line) => /^\s+if:/.test(line));
if (
  reviewIf.length !== 1 ||
  reviewIf[0].trim() !== "if: github.event_name == 'pull_request'" ||
  reviewJob.lines.some((line) => /^\s+continue-on-error:/.test(line))
) {
  fail("npm-dependency-review must run only on pull requests and fail closed");
}
const actionPattern = /^(\s*)-\s+uses:\s*actions\/dependency-review-action@([^\s#]+)(?:\s+#.*)?$/;
const actionSteps = [];
for (let index = reviewJob.start; index < reviewJob.end; index += 1) {
  const match = ciLines[index].match(actionPattern);
  if (match) {
    actionSteps.push({ index, indent: match[1].length });
  }
}
if (actionSteps.length !== 1) {
  fail("required CI must contain exactly one dependency-review action step");
}
const actionRef = ciLines[actionSteps[0].index].match(actionPattern)?.[2];
if (actionRef !== expectedDependencyReviewRef) {
  fail("dependency-review action must use the approved pinned revision");
}

const actionStep = actionSteps[0];
let actionEnd = reviewJob.end;
for (let index = actionStep.index + 1; index < reviewJob.end; index += 1) {
  const nextStep = ciLines[index].match(/^(\s*)-\s+/);
  if (nextStep && nextStep[1].length <= actionStep.indent) {
    actionEnd = index;
    break;
  }
}
const actionLines = ciLines.slice(actionStep.index + 1, actionEnd);
if (actionLines.some((line) => /^\s+(?:if|continue-on-error):/.test(line))) {
  fail("dependency-review action cannot be skipped or failure-tolerant");
}
const withIndexes = actionLines
  .map((line, index) => ({ match: line.match(/^(\s*)with:\s*(?:#.*)?$/), index }))
  .filter(({ match }) => match);
if (withIndexes.length !== 1) {
  fail("dependency-review action must contain exactly one with block");
}
const withIndent = withIndexes[0].match[1].length;
const allowGhsas = actionLines
  .slice(withIndexes[0].index + 1)
  .map((line) => line.match(/^(\s*)allow-ghsas:\s*([^#\r\n]+?)\s*(?:#.*)?$/))
  .filter((match) => match && match[1].length > withIndent)
  .map((match) => match[2].trim().replace(/^["']|["']$/g, ""));
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
const gateNeeds = gateJob.lines.find((line) => /^\s+needs:\s*\[/.test(line));
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
const gateSource = gateJob.lines.join("\n");
const requiredReviewResult =
  `if [[ "\${{ github.event_name }}" == "pull_request" && "\${{ needs.npm-dependency-review.result }}" != "success" ]]; then`;
if (!gateSource.includes(requiredReviewResult)) {
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
  const importSource = source
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/^\s*\/\/.*$/gm, "");
  for (const match of importSource.matchAll(/\b(?:import|require)\s*\(([^)]*)\)/g)) {
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
