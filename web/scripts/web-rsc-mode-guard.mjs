import fs from "node:fs";
import { builtinModules } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { createServer, loadConfigFromFile } from "vite";

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
const allowedReactRouterDomValueExports = new Set([
  "BrowserRouter",
  "Link",
  "MemoryRouter",
  "NavLink",
  "Navigate",
  "Outlet",
  "Route",
  "Routes",
  "useLocation",
  "useNavigate",
  "useParams",
  "useSearchParams",
]);
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
const forbiddenCodeGenerationIdentifiers = new Set(["eval", "Function"]);
const timerNames = new Set(["setTimeout", "setInterval"]);
const globalObjectNames = new Set(["globalThis", "self", "window"]);
const expectedVitePluginNames = [
  "vite:react-babel",
  "vite:react:refresh-wrapper",
  "vite:react:config-post",
  "vite:react-refresh-fbm",
  "vite:react-refresh",
  "vite:react-virtual-preamble",
  "@tailwindcss/vite:scan",
  "@tailwindcss/vite:generate:serve",
  "@tailwindcss/vite:generate:build",
  "zeroclaw-dev-app-prefix",
];
const serveOnlyVitePluginName = "zeroclaw-dev-app-prefix";

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

function assertOutsideSkippedDist(candidate, distRoot, context) {
  const resolved = path.resolve(candidate);
  if (isInside(distRoot, resolved)) {
    fail(`${context} reaches skipped web/dist: ${resolved}`);
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

function unwrapTimerHandler(node) {
  let current = node;
  while (
    current &&
    (ts.isParenthesizedExpression(current) ||
      ts.isAsExpression(current) ||
      ts.isTypeAssertionExpression(current) ||
      ts.isNonNullExpression(current) ||
      current.kind === ts.SyntaxKind.SatisfiesExpression)
  ) {
    current = current.expression;
  }
  return current;
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

  function isScopeNode(node) {
    return (
      ts.isSourceFile(node) ||
      ts.isBlock(node) ||
      ts.isModuleBlock(node) ||
      ts.isClassLike(node) ||
      ts.isFunctionLike(node) ||
      ts.isForStatement(node) ||
      ts.isForInStatement(node) ||
      ts.isForOfStatement(node) ||
      ts.isCatchClause(node)
    );
  }

  function nearestScope(node) {
    let current = node;
    while (current && !isScopeNode(current)) {
      current = current.parent;
    }
    return current ?? sourceFile;
  }

  function outerScope(scope) {
    return scope === sourceFile ? null : nearestScope(scope.parent);
  }

  const bindingsByScope = new Map();
  const promiseResolverCandidates = [];

  function addBinding(scope, name, callable) {
    if (!name) {
      return;
    }
    let bindings = bindingsByScope.get(scope);
    if (!bindings) {
      bindings = new Map();
      bindingsByScope.set(scope, bindings);
    }
    if (bindings.has(name)) {
      const binding = bindings.get(name);
      binding.callable &&= callable;
      return binding;
    }
    const binding = { callable, mutated: false };
    bindings.set(name, binding);
    return binding;
  }

  function addBindingNames(scope, name, callable = false) {
    if (ts.isIdentifier(name)) {
      return [addBinding(scope, name.text, callable)];
    }
    const bindings = [];
    for (const element of name.elements) {
      if (!ts.isOmittedExpression(element)) {
        bindings.push(...addBindingNames(scope, element.name, callable));
      }
    }
    return bindings;
  }

  function resolveBindingFromScope(scope, name) {
    let current = scope;
    while (current) {
      const binding = bindingsByScope.get(current)?.get(name);
      if (binding) {
        return binding;
      }
      current = outerScope(current);
    }
    return null;
  }

  function variableScope(declaration) {
    const declarationList = declaration.parent;
    if (declarationList.flags & ts.NodeFlags.Var) {
      let current = declaration.parent;
      while (current && !ts.isSourceFile(current) && !ts.isFunctionLike(current)) {
        current = current.parent;
      }
      return current ?? sourceFile;
    }
    return nearestScope(declaration);
  }

  function isPromiseResolverParameter(parameter) {
    const functionLike = parameter.parent;
    if (
      !ts.isFunctionLike(functionLike) ||
      functionLike.parameters[0] !== parameter
    ) {
      return false;
    }
    let promiseConstructor = functionLike.parent;
    while (promiseConstructor && ts.isParenthesizedExpression(promiseConstructor)) {
      promiseConstructor = promiseConstructor.parent;
    }
    return (
      ts.isNewExpression(promiseConstructor) &&
      ts.isIdentifier(promiseConstructor.expression) &&
      promiseConstructor.expression.text === "Promise" &&
      promiseConstructor.arguments?.[0] &&
      unwrapTimerHandler(promiseConstructor.arguments[0]) === functionLike
    );
  }

  function collectBindings(node) {
    if (ts.isFunctionDeclaration(node) && node.name) {
      addBinding(nearestScope(node.parent), node.name.text, true);
    }
    if (ts.isFunctionExpression(node) && node.name) {
      addBinding(node, node.name.text, false);
    }
    if (ts.isClassDeclaration(node) && node.name) {
      addBinding(nearestScope(node.parent), node.name.text, false);
    }
    if (ts.isClassExpression(node) && node.name) {
      addBinding(node, node.name.text, false);
    }
    if (ts.isImportClause(node)) {
      if (node.name) {
        addBinding(sourceFile, node.name.text, false);
      }
      const named = node.namedBindings;
      if (named && ts.isNamespaceImport(named)) {
        addBinding(sourceFile, named.name.text, false);
      } else if (named && ts.isNamedImports(named)) {
        for (const element of named.elements) {
          addBinding(sourceFile, element.name.text, false);
        }
      }
    }
    if (ts.isParameter(node)) {
      const bindings = addBindingNames(node.parent, node.name);
      if (isPromiseResolverParameter(node)) {
        promiseResolverCandidates.push({ binding: bindings[0], functionLike: node.parent });
      }
    }
    if (ts.isVariableDeclaration(node)) {
      const initializer = node.initializer && unwrapTimerHandler(node.initializer);
      const declarationList = node.parent;
      addBindingNames(
        variableScope(node),
        node.name,
        ts.isIdentifier(node.name) &&
          Boolean(
            declarationList.flags & ts.NodeFlags.Const &&
              initializer &&
              (ts.isArrowFunction(initializer) || ts.isFunctionExpression(initializer)),
          ),
      );
    }
    ts.forEachChild(node, collectBindings);
  }

  function markAssignedBindings(node, scope) {
    const target = unwrapTimerHandler(node);
    if (ts.isIdentifier(target)) {
      const binding = resolveBindingFromScope(scope, target.text);
      if (binding) {
        binding.mutated = true;
      }
      return;
    }
    if (ts.isArrayLiteralExpression(target)) {
      for (const element of target.elements) {
        if (!ts.isOmittedExpression(element)) {
          markAssignedBindings(element, scope);
        }
      }
      return;
    }
    if (ts.isObjectLiteralExpression(target)) {
      for (const property of target.properties) {
        if (ts.isShorthandPropertyAssignment(property)) {
          markAssignedBindings(property.name, scope);
        } else if (ts.isPropertyAssignment(property)) {
          markAssignedBindings(property.initializer, scope);
        } else if (ts.isSpreadAssignment(property)) {
          markAssignedBindings(property.expression, scope);
        }
      }
      return;
    }
    if (ts.isSpreadElement(target)) {
      markAssignedBindings(target.expression, scope);
    }
  }

  function collectAssignedNames(node) {
    if (ts.isIdentifier(node)) {
      const binding = resolveBindingFromScope(nearestScope(node.parent), node.text);
      if (binding) {
        binding.mutated = true;
      }
      return;
    }
    markAssignedBindings(node, nearestScope(node.parent));
  }

  function collectMutations(node) {
    if (
      ts.isBinaryExpression(node) &&
      node.operatorToken.kind >= ts.SyntaxKind.FirstAssignment &&
      node.operatorToken.kind <= ts.SyntaxKind.LastAssignment
    ) {
      collectAssignedNames(node.left);
    } else if (
      (ts.isForInStatement(node) || ts.isForOfStatement(node)) &&
      !ts.isVariableDeclarationList(node.initializer)
    ) {
      markAssignedBindings(node.initializer, nearestScope(node.initializer));
    } else if (
      (ts.isPrefixUnaryExpression(node) || ts.isPostfixUnaryExpression(node)) &&
      (node.operator === ts.SyntaxKind.PlusPlusToken ||
        node.operator === ts.SyntaxKind.MinusMinusToken)
    ) {
      collectAssignedNames(node.operand);
    }
    ts.forEachChild(node, collectMutations);
  }

  function isCallableIdentifier(node) {
    let scope = nearestScope(node.parent);
    while (scope) {
      const bindings = bindingsByScope.get(scope);
      if (bindings?.has(node.text)) {
        const binding = bindings.get(node.text);
        return binding.callable && !binding.mutated;
      }
      scope = outerScope(scope);
    }
    return false;
  }

  function isCallableTimerHandler(node) {
    const handler = unwrapTimerHandler(node);
    return (
      ts.isArrowFunction(handler) ||
      ts.isFunctionExpression(handler) ||
      (ts.isIdentifier(handler) && isCallableIdentifier(handler))
    );
  }

  function timerName(expression) {
    const timerExpression = unwrapTimerHandler(expression);
    if (
      ts.isIdentifier(timerExpression) &&
      timerNames.has(timerExpression.text) &&
      !resolveBindingFromScope(nearestScope(timerExpression.parent), timerExpression.text)
    ) {
      return timerExpression.text;
    }
    if (
      ts.isPropertyAccessExpression(timerExpression) &&
      ts.isIdentifier(timerExpression.expression) &&
      globalObjectNames.has(timerExpression.expression.text) &&
      !resolveBindingFromScope(
        nearestScope(timerExpression.expression.parent),
        timerExpression.expression.text,
      ) &&
      timerNames.has(timerExpression.name.text)
    ) {
      return timerExpression.name.text;
    }
    return null;
  }

  function isInsideTypeNode(node) {
    let current = node.parent;
    while (current && current !== sourceFile) {
      if (ts.isTypeNode(current)) {
        return true;
      }
      current = current.parent;
    }
    return false;
  }

  function isDirectCallCallee(node) {
    let current = node;
    while (
      current.parent &&
      (ts.isParenthesizedExpression(current.parent) ||
        ts.isAsExpression(current.parent) ||
        ts.isTypeAssertionExpression(current.parent) ||
        ts.isNonNullExpression(current.parent) ||
        current.parent.kind === ts.SyntaxKind.SatisfiesExpression)
    ) {
      current = current.parent;
    }
    return ts.isCallExpression(current.parent) && current.parent.expression === current;
  }

  function isUnshadowedGlobalIdentifier(node) {
    return (
      ts.isIdentifier(node) &&
      globalObjectNames.has(node.text) &&
      !resolveBindingFromScope(nearestScope(node.parent), node.text)
    );
  }

  function isReflectiveGlobalAccess(node) {
    return (
      ts.isCallExpression(node) &&
      ts.isPropertyAccessExpression(node.expression) &&
      ts.isIdentifier(node.expression.expression) &&
      node.expression.expression.text === "Reflect" &&
      !resolveBindingFromScope(nearestScope(node.expression.parent), "Reflect") &&
      node.arguments[0] &&
      isUnshadowedGlobalIdentifier(unwrapTimerHandler(node.arguments[0]))
    );
  }

  function objectGlobalPropertyAccess(node) {
    if (
      !ts.isCallExpression(node) ||
      !ts.isPropertyAccessExpression(node.expression) ||
      !ts.isIdentifier(node.expression.expression) ||
      node.expression.expression.text !== "Object" ||
      resolveBindingFromScope(nearestScope(node.expression.parent), "Object") ||
      !node.arguments[0] ||
      !isUnshadowedGlobalIdentifier(unwrapTimerHandler(node.arguments[0]))
    ) {
      return null;
    }
    return node.expression.name.text;
  }

  function isPropertyNameIdentifier(node) {
    const parent = node.parent;
    if (
      (ts.isImportSpecifier(parent) ||
        ts.isExportSpecifier(parent) ||
        ts.isBindingElement(parent)) &&
      parent.propertyName === node
    ) {
      return true;
    }
    return (
      (ts.isMethodDeclaration(parent) ||
        ts.isPropertyAccessExpression(parent) ||
        ts.isPropertyDeclaration(parent) ||
        ts.isGetAccessorDeclaration(parent) ||
        ts.isSetAccessorDeclaration(parent) ||
        ts.isMethodSignature(parent) ||
        ts.isPropertySignature(parent) ||
        ts.isPropertyAssignment(parent) ||
        ts.isJsxAttribute(parent)) &&
      parent.name === node
    );
  }

  function isAllowedGlobalObjectUse(node) {
    let current = node;
    while (
      current.parent &&
      (ts.isParenthesizedExpression(current.parent) ||
        ts.isAsExpression(current.parent) ||
        ts.isTypeAssertionExpression(current.parent) ||
        ts.isNonNullExpression(current.parent) ||
        current.parent.kind === ts.SyntaxKind.SatisfiesExpression)
    ) {
      current = current.parent;
    }
    const parent = current.parent;
    if (
      (ts.isPropertyAccessExpression(parent) || ts.isElementAccessExpression(parent)) &&
      parent.expression === current
    ) {
      return true;
    }
    if (ts.isTypeOfExpression(parent)) {
      return true;
    }
    if (
      ts.isBinaryExpression(parent) &&
      parent.operatorToken.kind === ts.SyntaxKind.InKeyword &&
      parent.right === current
    ) {
      return true;
    }
    if (ts.isCallExpression(parent) && parent.arguments[0] === current) {
      const access = objectGlobalPropertyAccess(parent);
      return access === "defineProperty" && isSafeWindowDataDescriptor(parent);
    }
    return false;
  }

  function isSafeWindowDataDescriptor(node) {
    if (
      literalText(node.arguments[1]) !== "window" ||
      !node.arguments[2] ||
      !ts.isObjectLiteralExpression(node.arguments[2])
    ) {
      return false;
    }
    const allowedKeys = new Set(["value", "configurable", "enumerable", "writable"]);
    return node.arguments[2].properties.every(
      (property) =>
        ts.isPropertyAssignment(property) &&
        ts.isIdentifier(property.name) &&
        allowedKeys.has(property.name.text),
    );
  }

  function verifyTimerHandler(node, name) {
    if (!node.arguments[0] || !isCallableTimerHandler(node.arguments[0])) {
      fail(`${relative} ${name} handler is not syntactically proven callable`);
    }
  }

  collectBindings(sourceFile);
  for (const { binding, functionLike } of promiseResolverCandidates) {
    const scope = nearestScope(functionLike.parent);
    if (binding && !resolveBindingFromScope(scope, "Promise")) {
      binding.callable = true;
    }
  }
  collectMutations(sourceFile);

  function visit(node) {
    if (ts.isIdentifier(node)) {
      if (forbiddenCodeGenerationIdentifiers.has(node.text)) {
        fail(`${relative} contains a forbidden code-generation identifier: ${node.text}`);
      }
      if (forbiddenRscIdentifiers.has(node.text)) {
        fail(`${relative} contains an unstable RSC API identifier: ${node.text}`);
      }
      if (
        timerNames.has(node.text) &&
        !resolveBindingFromScope(nearestScope(node.parent), node.text) &&
        !isInsideTypeNode(node) &&
        !isPropertyNameIdentifier(node) &&
        !isDirectCallCallee(node)
      ) {
        fail(`${relative} uses an indirect global timer reference: ${node.text}`);
      }
      if (
        isUnshadowedGlobalIdentifier(node) &&
        !isInsideTypeNode(node) &&
        !isPropertyNameIdentifier(node) &&
        !isAllowedGlobalObjectUse(node)
      ) {
        fail(`${relative} uses an indirect global object reference: ${node.text}`);
      }
    }

    if (
      ts.isPropertyAccessExpression(node) &&
      timerName(node) &&
      !isInsideTypeNode(node) &&
      !isDirectCallCallee(node)
    ) {
      fail(`${relative} uses an indirect global timer reference: ${node.name.text}`);
    }

    if (
      ts.isPropertyAccessExpression(node) &&
      isUnshadowedGlobalIdentifier(node.expression) &&
      node.name.text === "Promise"
    ) {
      fail(`${relative} accesses replaceable global Promise state`);
    }

    if (isReflectiveGlobalAccess(node)) {
      fail(`${relative} uses forbidden reflective global access`);
    }

    const objectGlobalAccess = objectGlobalPropertyAccess(node);
    if (objectGlobalAccess) {
      if (objectGlobalAccess === "defineProperty") {
        if (!isSafeWindowDataDescriptor(node)) {
          fail(`${relative} uses forbidden Object global property access`);
        }
      } else if (
        [
          "assign",
          "defineProperties",
          "getOwnPropertyDescriptor",
          "getOwnPropertyDescriptors",
        ].includes(objectGlobalAccess)
      ) {
        fail(`${relative} uses forbidden Object global property access`);
      }
    }

    if (
      ts.isElementAccessExpression(node) &&
      isUnshadowedGlobalIdentifier(node.expression)
    ) {
      const key = ts.isStringLiteralLike(node.argumentExpression)
        ? node.argumentExpression.text
        : "computed property";
      fail(`${relative} uses forbidden computed global access: ${key}`);
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
          if (node.importClause?.isTypeOnly || element.isTypeOnly) {
            continue;
          }
          const name = importedName(element.propertyName ?? element.name);
          if (!name || !allowedReactRouterDomValueExports.has(name)) {
            fail(`${relative} imports an unapproved React Router value export: ${name ?? "unknown"}`);
          }
        }
      }
      if (specifier === "react-router-dom" && node.importClause?.name) {
        fail(`${relative} uses a default import from react-router-dom`);
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
          if (node.isTypeOnly || element.isTypeOnly) {
            continue;
          }
          const name = importedName(element.propertyName ?? element.name);
          if (!name || !allowedReactRouterDomValueExports.has(name)) {
            fail(`${relative} re-exports an unapproved React Router value export: ${name ?? "unknown"}`);
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
      const timer = timerName(node.expression);
      if (timer) {
        verifyTimerHandler(node, timer);
      }
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

function parseScriptAttributes(filePath, attributeText) {
  const attributes = [];
  let index = 0;
  while (index < attributeText.length) {
    while (/\s/.test(attributeText[index] ?? "")) {
      index += 1;
    }
    if (index >= attributeText.length) {
      break;
    }
    if (attributeText[index] === "/" && /^[\s/]*$/.test(attributeText.slice(index))) {
      break;
    }

    const name = attributeText.slice(index).match(/^[A-Za-z_:][A-Za-z0-9_.:-]*/)?.[0];
    if (!name) {
      fail(`${relativePath(path.dirname(filePath), filePath)} has malformed script tag attributes`);
    }
    index += name.length;
    while (/\s/.test(attributeText[index] ?? "")) {
      index += 1;
    }

    let value = null;
    if (attributeText[index] === "=") {
      index += 1;
      while (/\s/.test(attributeText[index] ?? "")) {
        index += 1;
      }
      const quote = attributeText[index];
      if (quote === '"' || quote === "'") {
        index += 1;
        const end = attributeText.indexOf(quote, index);
        if (end === -1) {
          fail(`${relativePath(path.dirname(filePath), filePath)} has malformed script src`);
        }
        value = attributeText.slice(index, end);
        index = end + 1;
      } else {
        const start = index;
        while (index < attributeText.length && !/\s/.test(attributeText[index])) {
          index += 1;
        }
        value = attributeText.slice(start, index);
        if (name.toLowerCase() === "src") {
          fail(`${relativePath(path.dirname(filePath), filePath)} has an unquoted script src`);
        }
      }
    }
    attributes.push({ name: name.toLowerCase(), value });
  }
  return attributes;
}

function verifyHtmlScriptTarget(filePath, src, webRoot, nodeModulesRoot, distRoot) {
  if (/[&\s]/.test(src)) {
    fail(`${relativePath(webRoot, filePath)} has an ambiguous script src`);
  }
  const rawPath = src.split(/[?#]/, 1)[0];
  if (!rawPath || rawPath.includes("\\")) {
    fail(`${relativePath(webRoot, filePath)} has an invalid local script src`);
  }
  let decodedPath;
  try {
    decodedPath = decodeURIComponent(rawPath);
  } catch {
    fail(`${relativePath(webRoot, filePath)} has malformed script src encoding`);
  }
  if (
    decodedPath.startsWith("//") ||
    /^[A-Za-z][A-Za-z0-9+.-]*:/.test(decodedPath)
  ) {
    fail(`${relativePath(webRoot, filePath)} has an external script src`);
  }
  const candidate = decodedPath.startsWith("/")
    ? path.resolve(webRoot, decodedPath.slice(1))
    : path.resolve(path.dirname(filePath), decodedPath);
  assertInsideWebRoot(candidate, webRoot, nodeModulesRoot, "HTML script src", true);
  assertOutsideSkippedDist(candidate, distRoot, "HTML script src");
  if (!fs.existsSync(candidate)) {
    fail(`${relativePath(webRoot, filePath)} references a missing script src: ${src}`);
  }
  const canonical = fs.realpathSync(candidate);
  assertInsideWebRoot(canonical, webRoot, nodeModulesRoot, "HTML script src", true);
  assertOutsideSkippedDist(canonical, distRoot, "HTML script src");
  if (!fs.statSync(canonical).isFile()) {
    fail(`${relativePath(webRoot, filePath)} references a non-file script src: ${src}`);
  }
  assertSupportedResolvedFormat(canonical, `${relativePath(webRoot, filePath)} script src ${src}`);
}

function inspectHtml(filePath, source, records, webRoot, nodeModulesRoot, distRoot) {
  const markupSource = source.replace(/<!--[\s\S]*?-->/g, (comment) => " ".repeat(comment.length));
  const openingPattern = /<script\b/gi;
  let markupStart = 0;
  while (true) {
    const opening = openingPattern.exec(markupSource);
    if (!opening) {
      if (/<base(?=[\s/>])/i.test(markupSource.slice(markupStart))) {
        fail(`${relativePath(webRoot, filePath)} contains a base element`);
      }
      break;
    }
    if (/<base(?=[\s/>])/i.test(markupSource.slice(markupStart, opening.index))) {
      fail(`${relativePath(webRoot, filePath)} contains a base element`);
    }
    let tagEnd = opening.index + opening[0].length;
    let quote = null;
    for (; tagEnd < markupSource.length; tagEnd += 1) {
      const character = markupSource[tagEnd];
      if (quote) {
        if (character === quote) {
          quote = null;
        }
      } else if (character === '"' || character === "'") {
        quote = character;
      } else if (character === ">") {
        break;
      }
    }
    if (tagEnd >= markupSource.length || quote) {
      fail(`${relativePath(webRoot, filePath)} has a malformed script tag`);
    }
    const attributes = parseScriptAttributes(
      filePath,
      markupSource.slice(opening.index + opening[0].length, tagEnd),
    );
    const srcAttributes = attributes.filter(({ name }) => name === "src");
    if (srcAttributes.length > 1) {
      fail(`${relativePath(webRoot, filePath)} has multiple script src attributes`);
    }
    if (srcAttributes.length === 1) {
      if (!srcAttributes[0].value) {
        fail(`${relativePath(webRoot, filePath)} has a malformed script src`);
      }
      verifyHtmlScriptTarget(
        filePath,
        srcAttributes[0].value,
        webRoot,
        nodeModulesRoot,
        distRoot,
      );
    }

    const closePattern = /<\/script\s*>/gi;
    closePattern.lastIndex = tagEnd + 1;
    const closing = closePattern.exec(markupSource);
    if (!closing) {
      fail(`${relativePath(webRoot, filePath)} has an unclosed script tag`);
    }
    inspectSource(filePath, source.slice(tagEnd + 1, closing.index), records);
    openingPattern.lastIndex = closing.index + closing[0].length;
    markupStart = openingPattern.lastIndex;
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
  distRoot,
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
      assertOutsideSkippedDist(
        lexicalPath,
        distRoot,
        `${relativePath(webRoot, record.filePath)} unresolved import ${record.specifier}`,
      );
      return;
    }
    if (rawId.startsWith("\0")) {
      fail(`${relativePath(webRoot, record.filePath)} resolves to a virtual module: ${record.specifier}`);
    }
    const resolvedPath = resolvedIdPath(rawId);
    assertOutsideSkippedDist(
      resolvedPath,
      distRoot,
      `${relativePath(webRoot, record.filePath)} import ${record.specifier}`,
    );
    const canonicalPath = fs.existsSync(resolvedPath)
      ? fs.realpathSync(resolvedPath)
      : resolvedPath;
    assertInsideWebRoot(
      canonicalPath,
      webRoot,
      nodeModulesRoot,
      `${relativePath(webRoot, record.filePath)} import ${record.specifier}`,
      true,
    );
    assertOutsideSkippedDist(
      canonicalPath,
      distRoot,
      `${relativePath(webRoot, record.filePath)} import ${record.specifier}`,
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
  assertOutsideSkippedDist(
    canonicalPath,
    distRoot,
    `${relativePath(webRoot, record.filePath)} import ${record.specifier}`,
  );
}

async function loadViteServer(webRoot) {
  return createServer({
    root: webRoot,
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
}

function propertyName(node) {
  if (ts.isIdentifier(node) || ts.isStringLiteralLike(node)) {
    return node.text;
  }
  return null;
}

function importedBinding(sourceFile, moduleName, imported) {
  const matches = [];
  for (const statement of sourceFile.statements) {
    if (
      !ts.isImportDeclaration(statement) ||
      literalText(statement.moduleSpecifier) !== moduleName ||
      !statement.importClause
    ) {
      continue;
    }
    if (imported === "default" && statement.importClause.name) {
      matches.push(statement.importClause.name.text);
    }
    const bindings = statement.importClause.namedBindings;
    if (imported !== "default" && bindings && ts.isNamedImports(bindings)) {
      for (const element of bindings.elements) {
        if (importedName(element.propertyName ?? element.name) === imported) {
          matches.push(element.name.text);
        }
      }
    }
  }
  if (matches.length !== 1) {
    fail(`Vite config must import ${imported} exactly once from ${moduleName}`);
  }
  return matches[0];
}

function unwrapParentheses(node) {
  let current = node;
  while (ts.isParenthesizedExpression(current)) {
    current = current.expression;
  }
  return current;
}

function verifyViteConfigPluginSource(configFile) {
  const sourceFile = parseSource(configFile, fs.readFileSync(configFile, "utf8"));
  const defineConfigName = importedBinding(sourceFile, "vite", "defineConfig");
  const reactName = importedBinding(sourceFile, "@vitejs/plugin-react", "default");
  const tailwindName = importedBinding(sourceFile, "@tailwindcss/vite", "default");
  const exports = sourceFile.statements.filter(
    (statement) => ts.isExportAssignment(statement) && !statement.isExportEquals,
  );
  if (exports.length !== 1) {
    fail("Vite config must contain exactly one default export");
  }
  const defineCall = unwrapParentheses(exports[0].expression);
  if (
    !ts.isCallExpression(defineCall) ||
    !ts.isIdentifier(defineCall.expression) ||
    defineCall.expression.text !== defineConfigName ||
    defineCall.arguments.length !== 1
  ) {
    fail("Vite config must export one direct defineConfig factory");
  }
  const factory = unwrapParentheses(defineCall.arguments[0]);
  if (!ts.isArrowFunction(factory) || ts.isBlock(factory.body)) {
    fail("Vite config must use an expression-bodied defineConfig factory");
  }
  const config = unwrapParentheses(factory.body);
  if (!ts.isObjectLiteralExpression(config)) {
    fail("Vite config factory must return one object literal");
  }
  if (
    config.properties.some(
      (property) =>
        ts.isSpreadAssignment(property) ||
        (property.name && ts.isComputedPropertyName(property.name)),
    )
  ) {
    fail("Vite config cannot use spread or computed top-level properties");
  }
  function rejectPrototypeProperties(node) {
    if (
      ts.isPropertyAssignment(node) &&
      node.name &&
      propertyName(node.name) === "__proto__"
    ) {
      fail("Vite config cannot use __proto__ properties");
    }
    ts.forEachChild(node, rejectPrototypeProperties);
  }
  rejectPrototypeProperties(config);
  const pluginProperties = config.properties.filter(
    (property) => property.name && propertyName(property.name) === "plugins",
  );
  if (
    pluginProperties.length !== 1 ||
    !ts.isPropertyAssignment(pluginProperties[0]) ||
    !ts.isArrayLiteralExpression(pluginProperties[0].initializer)
  ) {
    fail("Vite config must contain one literal plugins array");
  }
  const plugins = pluginProperties[0].initializer.elements;
  const expectedFactories = [reactName, tailwindName];
  if (plugins.length !== 3) {
    fail("Vite config must contain exactly the approved React, Tailwind, and serve-prefix plugins");
  }
  for (let index = 0; index < expectedFactories.length; index += 1) {
    const plugin = unwrapParentheses(plugins[index]);
    if (
      !ts.isCallExpression(plugin) ||
      !ts.isIdentifier(plugin.expression) ||
      plugin.expression.text !== expectedFactories[index] ||
      plugin.arguments.length !== 0
    ) {
      fail("Vite config must instantiate the approved React and Tailwind plugin factories directly");
    }
  }
  const localPlugin = unwrapParentheses(plugins[2]);
  if (!ts.isObjectLiteralExpression(localPlugin)) {
    fail("Vite config serve-prefix plugin must be an object literal");
  }
  const localProperties = new Map();
  for (const property of localPlugin.properties) {
    const name = property.name ? propertyName(property.name) : null;
    if (!name || localProperties.has(name)) {
      fail("Vite config serve-prefix plugin has an unsupported property");
    }
    localProperties.set(name, property);
  }
  if (
    localProperties.size !== 3 ||
    !localProperties.has("name") ||
    !localProperties.has("apply") ||
    !localProperties.has("configureServer")
  ) {
    fail("Vite config serve-prefix plugin must contain only name, apply, and configureServer");
  }
  const nameProperty = localProperties.get("name");
  const applyProperty = localProperties.get("apply");
  if (
    !ts.isPropertyAssignment(nameProperty) ||
    literalText(nameProperty.initializer) !== serveOnlyVitePluginName ||
    !ts.isPropertyAssignment(applyProperty) ||
    literalText(applyProperty.initializer) !== "serve" ||
    !ts.isMethodDeclaration(localProperties.get("configureServer"))
  ) {
    fail("Vite config serve-prefix plugin must retain its fixed name and serve-only hook");
  }
}

function verifyNoNestedVitePluginOptions(config) {
  const seen = new WeakSet();

  function visit(value, location, root) {
    if (typeof value === "function") {
      fail(`Vite configuration contains a function-valued container at ${location || "root"}`);
    }
    if (typeof value !== "object" || value === null) {
      return;
    }
    if (seen.has(value)) {
      return;
    }
    seen.add(value);
    const prototype = Object.getPrototypeOf(value);
    if (
      (!Array.isArray(value) && prototype !== Object.prototype && prototype !== null) ||
      (Array.isArray(value) && prototype !== Array.prototype)
    ) {
      fail(`Vite configuration contains a non-plain object at ${location || "root"}`);
    }
    for (const key of Object.keys(value)) {
      if (root && key === "plugins") {
        continue;
      }
      const child = value[key];
      const childLocation = location ? `${location}.${key}` : key;
      if (key === "plugins" && child != null && child !== false) {
        fail(`Vite configuration contains nested plugin options at ${childLocation}`);
      }
      visit(child, childLocation, false);
    }
  }

  visit(config, "", true);
}

function verifyServePrefixBehavior(plugin) {
  const middleware = [];
  plugin.configureServer({
    middlewares: {
      use(handler) {
        middleware.push(handler);
      },
    },
  });
  if (middleware.length !== 1 || typeof middleware[0] !== "function") {
    fail("Vite serve-prefix plugin must install exactly one middleware");
  }
  for (const [initial, expected] of [
    ["/_app/probe.js?mode=guard", "/probe.js?mode=guard"],
    ["/api/probe", "/api/probe"],
  ]) {
    const request = { url: initial };
    let nextCalls = 0;
    middleware[0](request, {}, () => {
      nextCalls += 1;
    });
    if (request.url !== expected || nextCalls !== 1) {
      fail("Vite serve-prefix middleware no longer mirrors the gateway strip-prefix behavior");
    }
  }
}

async function flattenUserPluginOptions(options, flattened = []) {
  const resolved = await options;
  if (resolved == null || resolved === false) {
    return flattened;
  }
  if (Array.isArray(resolved)) {
    for (const option of resolved) {
      await flattenUserPluginOptions(option, flattened);
    }
    return flattened;
  }
  flattened.push(resolved);
  return flattened;
}

async function loadViteUserConfig(webRoot, configFile, command) {
  const loaded = await loadConfigFromFile(
    { command, mode: command === "build" ? "production" : "development" },
    configFile,
    webRoot,
    "silent",
  );
  if (!loaded?.config) {
    fail("Vite build configuration could not be loaded");
  }
  return loaded.config;
}

async function verifyVitePlugins(config) {
  verifyNoNestedVitePluginOptions(config);
  const plugins = await flattenUserPluginOptions(config.plugins);
  const names = plugins.map((plugin) => plugin?.name);
  if (
    names.length !== expectedVitePluginNames.length ||
    names.some((name, index) => name !== expectedVitePluginNames[index])
  ) {
    fail("Vite configuration must resolve exactly the approved user plugin sequence");
  }
  for (const plugin of plugins) {
    if (!plugin || typeof plugin !== "object" || typeof plugin.name !== "string") {
      fail("Vite configuration contains an unnamed plugin option");
    }
    if (plugin.name === serveOnlyVitePluginName) {
      if (plugin.apply !== "serve") {
        fail(`${serveOnlyVitePluginName} must be serve-only`);
      }
      verifyServePrefixBehavior(plugin);
    }
  }
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
  const distRoot = path.join(webRoot, "dist");
  const declared = declaredPackageNames(path.join(webRoot, "package.json"));
  const records = [];
  for (const filePath of collectSourceFiles(webRoot, webSourceRoot)) {
    if (isServerEntry(filePath)) {
      fail(`${relativePath(webRoot, filePath)} is a server/RSC entry surface`);
    }
    const source = fs.readFileSync(filePath, "utf8");
    if (path.extname(filePath).toLowerCase() === ".html") {
      inspectHtml(filePath, source, records, webRoot, nodeModulesRoot, distRoot);
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
    if (typeof server.config.configFile !== "string") {
      fail("effective Vite configuration must use one config file");
    }
    const configFile = fs.realpathSync(server.config.configFile);
    assertInsideWebRoot(configFile, webRoot, nodeModulesRoot, "Vite config", true);
    verifyEffectiveAlias(server, webRoot, webSourceRoot, nodeModulesRoot);
    for (const record of records) {
      await verifyImportBoundary(
        server,
        record,
        webRoot,
        webSourceRoot,
        nodeModulesRoot,
        distRoot,
        declared,
      );
    }

    verifyViteConfigPluginSource(configFile);
    const serveConfig = await loadViteUserConfig(webRoot, configFile, "serve");
    const buildConfig = await loadViteUserConfig(webRoot, configFile, "build");
    await verifyVitePlugins(serveConfig);
    await verifyVitePlugins(buildConfig);
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
