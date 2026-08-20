import fs from "node:fs";
import { builtinModules } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { createServer } from "vite";

const errorPrefix = "web-rsc-mode-guard:";
const scriptPath = fileURLToPath(import.meta.url);
const defaultRepoRoot = path.resolve(path.dirname(scriptPath), "../..");
const scannedExtensions = new Set([
  ".js",
  ".jsx",
  ".mjs",
  ".cjs",
  ".ts",
  ".tsx",
  ".mts",
  ".cts",
]);
const unsupportedExecutableExtensions = new Set([".mdx"]);
const inertSourceExtensions = new Set([
  ".avif",
  ".css",
  ".eot",
  ".gif",
  ".ico",
  ".jpeg",
  ".jpg",
  ".json",
  ".less",
  ".mp3",
  ".mp4",
  ".ogg",
  ".otf",
  ".png",
  ".sass",
  ".scss",
  ".svg",
  ".ttf",
  ".wav",
  ".webm",
  ".webp",
  ".woff",
  ".woff2",
]);
const nodeBuiltins = new Set(
  builtinModules.flatMap((name) => [name, `node:${name}`]),
);
const allowedTransitiveImports = new Set(["@codemirror/theme-one-dark"]);
const forbiddenRscIdentifiers = new Set([
  "RSCHydratedRouter",
  "unstable_RSCHydratedRouter",
  "RSCStaticRouter",
  "unstable_RSCStaticRouter",
  "createCallServer",
  "unstable_createCallServer",
  "getRSCStream",
  "unstable_getRSCStream",
  "matchRSCServerRequest",
  "unstable_matchRSCServerRequest",
  "routeRSCServerRequest",
  "unstable_routeRSCServerRequest",
  "reactRouterRSC",
  "unstable_reactRouterRSC",
]);

export class GuardError extends Error {}

function fail(message) {
  throw new GuardError(`${errorPrefix} ${message}`);
}

function relativePath(webRoot, filePath) {
  return path.relative(webRoot, filePath).split(path.sep).join("/");
}

function isInside(root, candidate) {
  const relative = path.relative(root, candidate);
  return relative === "" || (!relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative));
}

function assertInsideWebRoot(candidate, webRoot, nodeModulesRoot, context, rejectNodeModules) {
  const resolved = path.resolve(candidate);
  if (!isInside(webRoot, resolved)) {
    fail(`${context} escapes the guarded web root: ${resolved}`);
  }
  if (rejectNodeModules && isInside(nodeModulesRoot, resolved)) {
    fail(`${context} reaches skipped node_modules: ${resolved}`);
  }
}

function assertSupportedResolvedFormat(filePath, context) {
  const extension = path.extname(filePath).toLowerCase();
  if (
    scannedExtensions.has(extension) ||
    extension === ".html" ||
    inertSourceExtensions.has(extension)
  ) {
    return;
  }
  fail(`${context} resolves to an unrecognized source format: ${filePath}`);
}

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

function isBuiltinSpecifier(specifier) {
  return specifier.startsWith("node:") || nodeBuiltins.has(specifier);
}

function isLocalSpecifier(specifier) {
  return specifier.startsWith(".") || specifier.startsWith("@/");
}

function literalText(node) {
  if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) {
    return node.text;
  }
  return null;
}

function importedName(node) {
  if (ts.isIdentifier(node) || ts.isStringLiteral(node)) {
    return node.text;
  }
  return null;
}

function scriptKind(filePath) {
  switch (path.extname(filePath).toLowerCase()) {
    case ".tsx":
      return ts.ScriptKind.TSX;
    case ".jsx":
      return ts.ScriptKind.JSX;
    case ".ts":
    case ".mts":
    case ".cts":
      return ts.ScriptKind.TS;
    default:
      return ts.ScriptKind.JS;
  }
}

function parseSource(filePath, source) {
  const sourceFile = ts.createSourceFile(
    filePath,
    source,
    ts.ScriptTarget.Latest,
    true,
    scriptKind(filePath),
  );
  if (sourceFile.parseDiagnostics?.length) {
    fail(`${relativePath(path.dirname(filePath), filePath)} has a syntax error`);
  }
  return sourceFile;
}

function isServerEntry(filePath) {
  const basename = path.basename(filePath);
  return (
    /^react-router\.config\./.test(basename) ||
    /^entry\.(?:server|rsc)\./.test(basename) ||
    /\.(?:server|rsc)\.[^.]+$/.test(basename)
  );
}

function moduleSpecifier(node, filePath, description) {
  const specifier = literalText(node);
  if (specifier === null) {
    fail(`${relativePath(path.dirname(filePath), filePath)} uses a nonliteral ${description}`);
  }
  return specifier;
}

function addModuleRecord(records, node, filePath, kind) {
  const specifier = moduleSpecifier(node, filePath, `${kind} module specifier`);
  const relative = relativePath(path.dirname(filePath), filePath);
  if (forbiddenSpecifier(specifier)) {
    fail(`${relative} imports RSC/server-capable module ${specifier}`);
  }
  records.push({ filePath, specifier, kind });
}

function inspectSource(filePath, source, records) {
  const sourceFile = parseSource(filePath, source);
  const relative = path.basename(filePath);

  function visit(node) {
    if (ts.isIdentifier(node) && forbiddenRscIdentifiers.has(node.text)) {
      fail(`${relative} contains an unstable RSC API identifier: ${node.text}`);
    }

    if (
      ts.isExpressionStatement(node) &&
      (ts.isStringLiteral(node.expression) ||
        ts.isNoSubstitutionTemplateLiteral(node.expression)) &&
      (node.expression.text === "use server" || node.expression.text === "react-server")
    ) {
      fail(`${relative} contains a server directive`);
    }

    if (ts.isImportDeclaration(node)) {
      addModuleRecord(records, node.moduleSpecifier, filePath, "static import");
      const specifier = literalText(node.moduleSpecifier);
      const bindings = node.importClause?.namedBindings;
      if (
        bindings &&
        ts.isNamespaceImport(bindings) &&
        specifier === "react-router-dom"
      ) {
        fail(`${relative} uses a namespace import from react-router-dom`);
      }
      if (specifier === "react-router-dom" && bindings && ts.isNamedImports(bindings)) {
        for (const element of bindings.elements) {
          const name = importedName(element.propertyName ?? element.name);
          if (name && forbiddenRscIdentifiers.has(name)) {
            fail(`${relative} imports unstable RSC API ${name} from react-router-dom`);
          }
        }
      }
    }

    if (ts.isExportDeclaration(node) && node.moduleSpecifier) {
      const specifier = moduleSpecifier(node.moduleSpecifier, filePath, "export module specifier");
      if (
        specifier === "react-router-dom" &&
        (!node.exportClause || ts.isNamespaceExport(node.exportClause))
      ) {
        fail(`${relative} re-exports the full react-router-dom namespace`);
      }
      if (
        specifier === "react-router-dom" &&
        node.exportClause &&
        ts.isNamedExports(node.exportClause)
      ) {
        for (const element of node.exportClause.elements) {
          const name = importedName(element.propertyName ?? element.name);
          if (name && forbiddenRscIdentifiers.has(name)) {
            fail(`${relative} re-exports unstable RSC API ${name} from react-router-dom`);
          }
        }
      }
      addModuleRecord(records, node.moduleSpecifier, filePath, "export");
    }

    if (ts.isImportEqualsDeclaration(node) && ts.isExternalModuleReference(node.moduleReference)) {
      addModuleRecord(records, node.moduleReference.expression, filePath, "import-equals");
    }

    if (ts.isImportTypeNode(node)) {
      const argument = ts.isLiteralTypeNode(node.argument)
        ? node.argument.literal
        : node.argument;
      addModuleRecord(records, argument, filePath, "import type");
    }

    if (ts.isCallExpression(node)) {
      const isDynamicImport = node.expression.kind === ts.SyntaxKind.ImportKeyword;
      const isRequire = ts.isIdentifier(node.expression) && node.expression.text === "require";
      if (isDynamicImport || isRequire) {
        if (node.arguments.length !== 1) {
          fail(`${relative} uses a nonliteral dynamic module specifier`);
        }
        const argument = node.arguments[0];
        const specifier = literalText(argument);
        if (specifier === null) {
          fail(`${relative} uses a nonliteral dynamic module specifier`);
        }
        if (specifier === "react-router-dom") {
          fail(`${relative} dynamically imports react-router-dom`);
        }
        addModuleRecord(
          records,
          argument,
          filePath,
          isDynamicImport ? "dynamic import" : "require",
        );
      }
    }

    ts.forEachChild(node, visit);
  }

  visit(sourceFile);
}

function inspectHtml(filePath, source, records) {
  const scriptPattern = /<script\b[^>]*>([\s\S]*?)<\/script\s*>/gi;
  for (const match of source.matchAll(scriptPattern)) {
    inspectSource(filePath, match[1], records);
  }
}

function collectSourceFiles(webRoot, webSourceRoot) {
  const files = [];
  const ignoredPaths = new Set([
    path.join(webRoot, "dist"),
    path.join(webRoot, "node_modules"),
  ]);

  function walk(directory) {
    const entries = fs.readdirSync(directory, { withFileTypes: true });
    entries.sort((left, right) => left.name.localeCompare(right.name));
    for (const entry of entries) {
      const entryPath = path.join(directory, entry.name);
      if (entry.isSymbolicLink()) {
        fail(`${relativePath(webRoot, entryPath)} is a symbolic link`);
      }
      if (entry.isDirectory()) {
        if (ignoredPaths.has(entryPath)) {
          continue;
        }
        walk(entryPath);
        continue;
      }
      if (entry.isFile() && scannedExtensions.has(path.extname(entry.name).toLowerCase())) {
        files.push(entryPath);
      } else if (
        entry.isFile() &&
        unsupportedExecutableExtensions.has(path.extname(entry.name).toLowerCase())
      ) {
        fail(`${relativePath(webRoot, entryPath)} uses an unsupported executable source format`);
      } else if (entry.isFile() && path.extname(entry.name).toLowerCase() === ".html") {
        files.push(entryPath);
      } else if (
        entry.isFile() &&
        isInside(webSourceRoot, entryPath) &&
        !inertSourceExtensions.has(path.extname(entry.name).toLowerCase())
      ) {
        fail(`${relativePath(webRoot, entryPath)} uses an unrecognized source format`);
      }
    }
  }

  walk(webRoot);
  return files;
}

function declaredPackageNames(packagePath) {
  if (!fs.existsSync(packagePath)) {
    fail("missing web/package.json");
  }
  const packageJson = JSON.parse(fs.readFileSync(packagePath, "utf8"));
  if (!packageJson.dependencies?.["react-router-dom"]) {
    fail("react-router-dom must remain a direct runtime dependency");
  }
  const declared = new Set();
  for (const section of [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
  ]) {
    for (const name of Object.keys(packageJson[section] ?? {})) {
      declared.add(name);
    }
  }
  return declared;
}

function rawResolvedId(id) {
  return typeof id === "string" ? id : id?.id;
}

function resolvedIdPath(rawId) {
  const withoutQuery = rawId.split(/[?#]/, 1)[0];
  if (withoutQuery.startsWith("file://")) {
    return fileURLToPath(withoutQuery);
  }
  return path.resolve(withoutQuery);
}

function aliasEntries(alias) {
  if (Array.isArray(alias)) {
    return alias;
  }
  if (alias && typeof alias === "object") {
    return Object.entries(alias).map(([find, replacement]) => ({ find, replacement }));
  }
  return [];
}

function aliasMatchesAt(entry) {
  if (entry.find === "@") {
    return true;
  }
  if (entry.find instanceof RegExp) {
    entry.find.lastIndex = 0;
    return entry.find.test("@");
  }
  return false;
}

function verifyEffectiveAlias(server, webRoot, webSourceRoot, nodeModulesRoot) {
  const entries = aliasEntries(server.config.resolve?.alias);
  const matching = entries.filter(aliasMatchesAt);
  if (matching.length !== 1) {
    fail("effective Vite configuration must contain exactly one @ alias");
  }
  const replacement = matching[0].replacement;
  if (typeof replacement !== "string") {
    fail("effective Vite @ alias must have a string replacement");
  }
  const aliasRoot = path.resolve(webRoot, replacement);
  const checkedAliasRoot = fs.existsSync(aliasRoot) ? fs.realpathSync(aliasRoot) : aliasRoot;
  assertInsideWebRoot(
    checkedAliasRoot,
    webRoot,
    nodeModulesRoot,
    "effective Vite @ alias",
    true,
  );
  if (checkedAliasRoot !== webSourceRoot) {
    fail("effective Vite @ alias must be rooted at web/src");
  }
}

async function verifyImportBoundary(
  server,
  record,
  webRoot,
  webSourceRoot,
  nodeModulesRoot,
  declared,
) {
  const packageNameValue = packageName(record.specifier);
  const local = isLocalSpecifier(record.specifier);
  if (
    !local &&
    !isBuiltinSpecifier(record.specifier) &&
    !declared.has(packageNameValue) &&
    !allowedTransitiveImports.has(packageNameValue)
  ) {
    fail(`${relativePath(webRoot, record.filePath)} imports undeclared package or local alias ${record.specifier}`);
  }
  if (isBuiltinSpecifier(record.specifier)) {
    return;
  }

  const resolved = await server.pluginContainer.resolveId(record.specifier, record.filePath);
  const rawId = rawResolvedId(resolved);
  if (local) {
    if (!rawId) {
      const lexicalPath = record.specifier.startsWith("@/")
        ? path.resolve(webSourceRoot, record.specifier.slice(2))
        : path.resolve(path.dirname(record.filePath), record.specifier);
      assertInsideWebRoot(
        lexicalPath,
        webRoot,
        nodeModulesRoot,
        `${relativePath(webRoot, record.filePath)} unresolved import ${record.specifier}`,
        true,
      );
      return;
    }
    if (rawId.startsWith("\0")) {
      fail(`${relativePath(webRoot, record.filePath)} resolves to a virtual module: ${record.specifier}`);
    }
    const resolvedPath = resolvedIdPath(rawId);
    assertInsideWebRoot(
      fs.existsSync(resolvedPath) ? fs.realpathSync(resolvedPath) : resolvedPath,
      webRoot,
      nodeModulesRoot,
      `${relativePath(webRoot, record.filePath)} import ${record.specifier}`,
      true,
    );
    assertSupportedResolvedFormat(
      resolvedPath,
      `${relativePath(webRoot, record.filePath)} import ${record.specifier}`,
    );
    return;
  }

  if (!rawId) {
    fail(`${relativePath(webRoot, record.filePath)} cannot resolve package import ${record.specifier}`);
  }
  if (rawId.startsWith("\0")) {
    fail(`${relativePath(webRoot, record.filePath)} resolves to a virtual module: ${record.specifier}`);
  }
  const resolvedPath = resolvedIdPath(rawId);
  const canonicalPath = fs.existsSync(resolvedPath)
    ? fs.realpathSync(resolvedPath)
    : resolvedPath;
  if (!isInside(webRoot, canonicalPath)) {
    fail(`${relativePath(webRoot, record.filePath)} resolves outside the guarded web root: ${record.specifier}`);
  }
}

async function loadViteServer(webRoot) {
  return createServer({
    root: webRoot,
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
}

export async function runGuard(repoRoot = process.env.ZEROCLAW_RSC_GUARD_ROOT ?? defaultRepoRoot) {
  const resolvedRepoRoot = path.resolve(repoRoot);
  const webPath = path.join(resolvedRepoRoot, "web");
  if (!fs.existsSync(webPath) || !fs.lstatSync(webPath).isDirectory()) {
    fail("missing web directory");
  }
  if (fs.lstatSync(webPath).isSymbolicLink()) {
    fail("web directory is a symbolic link");
  }
  const webRoot = fs.realpathSync(webPath);
  const webSourceRoot = fs.realpathSync(path.join(webRoot, "src"));
  const nodeModulesRoot = path.join(webRoot, "node_modules");
  const declared = declaredPackageNames(path.join(webRoot, "package.json"));
  const records = [];
  for (const filePath of collectSourceFiles(webRoot, webSourceRoot)) {
    if (isServerEntry(filePath)) {
      fail(`${relativePath(webRoot, filePath)} is a server/RSC entry surface`);
    }
    const source = fs.readFileSync(filePath, "utf8");
    if (path.extname(filePath).toLowerCase() === ".html") {
      inspectHtml(filePath, source, records);
    } else {
      inspectSource(filePath, source, records);
    }
  }

  let server;
  try {
    server = await loadViteServer(webRoot);
    if (path.resolve(server.config.root) !== webRoot) {
      fail("effective Vite root escapes the guarded web root");
    }
    verifyEffectiveAlias(server, webRoot, webSourceRoot, nodeModulesRoot);
    for (const record of records) {
      await verifyImportBoundary(
        server,
        record,
        webRoot,
        webSourceRoot,
        nodeModulesRoot,
        declared,
      );
    }
  } finally {
    if (server) {
      await server.close();
    }
  }
}

const isMain = process.argv[1] && path.resolve(process.argv[1]) === scriptPath;
if (isMain) {
  try {
    await runGuard();
    console.log(`${errorPrefix} client-only React Router boundary verified`);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    console.error(message.startsWith(errorPrefix) ? message : `${errorPrefix} ${message}`);
    process.exitCode = 1;
  }
}
