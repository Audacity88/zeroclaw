import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { runGuard } from "./web-rsc-mode-guard.mjs";

const fixtureRoots = [];
const fixtureParent = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const validConfig = `
import { fileURLToPath } from "node:url";

export default {
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
};
`;
const validSource = `
import { BrowserRouter } from "react-router-dom";

export { BrowserRouter };
`;

function createFixture(testContext, { source = validSource, config = validConfig } = {}) {
  const repoRoot = fs.mkdtempSync(path.join(fixtureParent, ".rsc-guard-fixture-"));
  const webRoot = path.join(repoRoot, "web");
  fs.mkdirSync(path.join(webRoot, "src"), { recursive: true });
  fs.writeFileSync(
    path.join(webRoot, "package.json"),
    `${JSON.stringify(
      {
        dependencies: { "react-router-dom": "7.18.2" },
        devDependencies: { vite: "8.0.16" },
      },
      null,
      2,
    )}\n`,
  );
  fs.writeFileSync(path.join(webRoot, "vite.config.mjs"), config);
  fs.writeFileSync(path.join(webRoot, "src", "main.tsx"), source);
  fixtureRoots.push(repoRoot);
  testContext.after(() => fs.rmSync(repoRoot, { recursive: true, force: true }));
  return { repoRoot, webRoot };
}

async function expectFailure(testContext, repoRoot, pattern) {
  await assert.rejects(() => runGuard(repoRoot), pattern);
}

test.after(() => {
  for (const root of fixtureRoots) {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("regex literals cannot hide a following forbidden import", async (t) => {
  const { repoRoot } = createFixture(t, {
    source: `
const marker = /[/*]/;
import { StaticRouter } from "react-router-dom/server";

export { StaticRouter };
`,
  });
  await expectFailure(t, repoRoot, /web-rsc-mode-guard: .*react-router-dom\/server/);
});

test("a shadowed path.resolve cannot authorize an outside @ alias", async (t) => {
  const { repoRoot, webRoot } = createFixture(t, {
    config: `
import { fileURLToPath } from "node:url";

const path = {
  resolve: () => fileURLToPath(new URL("../outside", import.meta.url)),
};

export default {
  resolve: {
    alias: {
      "@": path.resolve(),
    },
  },
};
`,
  });
  fs.mkdirSync(path.join(repoRoot, "outside"));
  await expectFailure(t, repoRoot, /web-rsc-mode-guard: .*effective Vite @ alias/);
  assert.equal(fs.existsSync(path.join(webRoot, "src")), true);
});

test("post-declaration config mutation cannot move @ outside web", async (t) => {
  const { repoRoot } = createFixture(t, {
    config: `
import { fileURLToPath } from "node:url";

const config = {
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
};
config.resolve.alias["@"] = fileURLToPath(new URL("../outside", import.meta.url));

export default config;
`,
  });
  fs.mkdirSync(path.join(repoRoot, "outside"));
  await expectFailure(t, repoRoot, /web-rsc-mode-guard: .*effective Vite @ alias/);
});

test("the effective @ alias rooted at web/src passes", async (t) => {
  const { repoRoot, webRoot } = createFixture(t, {
    source: `
import { page } from "@/page";

export { page };
`,
  });
  fs.writeFileSync(path.join(webRoot, "src", "page.ts"), "export const page = true;\n");
  await runGuard(repoRoot);
});

test("an absent generated local module remains bounded by its lexical path", async (t) => {
  const { repoRoot } = createFixture(t, {
    source: `import type { components } from "./api-generated";\nexport const ready = true;`,
  });
  await runGuard(repoRoot);
});

test("forbidden server-capable module specifiers remain rejected", async (t) => {
  const cases = [
    [`import { reactRouter } from "@react-router/dev/vite";`, /@react-router\/dev\/vite/],
    [`import { StaticRouter } from "react-router-dom/server";`, /react-router-dom\/server/],
    [`import { router } from "react-router";`, /react-router/],
    [`import rsc from "@vitejs/plugin-rsc";`, /@vitejs\/plugin-rsc/],
    [`import stream from "react-server-dom-webpack/client";`, /react-server-dom-webpack/],
  ];
  for (const [source, pattern] of cases) {
    const { repoRoot } = createFixture(t, { source });
    await expectFailure(t, repoRoot, pattern);
  }
});

test("nonliteral dynamic import and require are rejected", async (t) => {
  const sources = [
    `const name = "react-router";\nawait import(name);`,
    `const name = "react-router";\nrequire(name);`,
    `await import("react-" + "router");`,
  ];
  for (const source of sources) {
    const { repoRoot } = createFixture(t, { source });
    await expectFailure(t, repoRoot, /web-rsc-mode-guard: .*nonliteral dynamic module specifier/);
  }
});

test("react-router-dom namespace, dynamic, and full re-export use are rejected", async (t) => {
  const sources = [
    `import * as router from "react-router-dom";\nexport { router };`,
    `const router = import("react-router-dom");\nexport { router };`,
    `export * from "react-router-dom";`,
    `export * as router from "react-router-dom";`,
  ];
  for (const source of sources) {
    const { repoRoot } = createFixture(t, { source });
    await expectFailure(t, repoRoot, /web-rsc-mode-guard: /);
  }
});

test("unstable RSC identifiers and server directives are rejected", async (t) => {
  const sources = [
    `const router = RSCStaticRouter;\nexport { router };`,
    `import { "unstable_createCallServer" as create } from "react-router-dom";\nexport { create };`,
    `export { "unstable_createCallServer" as create } from "react-router-dom";`,
    `"use server";\nexport const action = () => true;`,
    `"react-server";\nexport const action = () => true;`,
  ];
  for (const source of sources) {
    const { repoRoot } = createFixture(t, { source });
    await expectFailure(t, repoRoot, /web-rsc-mode-guard: /);
  }
});

test("server and RSC entry filenames are rejected", async (t) => {
  const { repoRoot, webRoot } = createFixture(t);
  fs.writeFileSync(path.join(webRoot, "entry.rsc.mjs"), "export default {};\n");
  await expectFailure(t, repoRoot, /web-rsc-mode-guard: .*server\/RSC entry surface/);
});

test("unsupported executable formats fail closed", async (t) => {
  const { repoRoot, webRoot } = createFixture(t);
  fs.writeFileSync(
    path.join(webRoot, "src", "server.mdx"),
    'import { StaticRouter } from "react-router-dom/server";\n',
  );
  await expectFailure(t, repoRoot, /web-rsc-mode-guard: .*unsupported executable source format/);
});

test("unrecognized component source formats fail closed", async (t) => {
  const { repoRoot, webRoot } = createFixture(t);
  fs.writeFileSync(
    path.join(webRoot, "src", "server.vue"),
    '<script>import { StaticRouter } from "react-router-dom/server";</script>\n',
  );
  await expectFailure(t, repoRoot, /web-rsc-mode-guard: .*unrecognized source format/);
});

test("imports cannot reach component formats elsewhere under web", async (t) => {
  for (const target of ["server.vue", "scripts/server.vue"]) {
    const { repoRoot, webRoot } = createFixture(t, {
      source: `import { server } from "../${target}";\nexport { server };`,
    });
    const targetPath = path.join(webRoot, target);
    fs.mkdirSync(path.dirname(targetPath), { recursive: true });
    fs.writeFileSync(
      targetPath,
      '<script>import { StaticRouter } from "react-router-dom/server";</script>\n',
    );
    await expectFailure(t, repoRoot, /web-rsc-mode-guard: .*unrecognized source format/);
  }
});

test("symbolic links are rejected", async (t) => {
  const { repoRoot, webRoot } = createFixture(t);
  const outside = path.join(repoRoot, "outside.ts");
  fs.writeFileSync(outside, "export const outside = true;\n");
  fs.symlinkSync(outside, path.join(webRoot, "src", "bridge.ts"));
  await expectFailure(t, repoRoot, /web-rsc-mode-guard: .*symbolic link/);
});

test("relative imports cannot escape web or enter node_modules", async (t) => {
  const outsideFixture = createFixture(t, {
    source: `import { bridge } from "../../outside/bridge";\nexport { bridge };`,
  });
  fs.mkdirSync(path.join(outsideFixture.repoRoot, "outside"));
  fs.writeFileSync(
    path.join(outsideFixture.repoRoot, "outside", "bridge.ts"),
    "export const bridge = true;\n",
  );
  await expectFailure(t, outsideFixture.repoRoot, /web-rsc-mode-guard: .*escapes the guarded web root/);

  const nodeModulesFixture = createFixture(t, {
    source: `import { server } from "../node_modules/local-rsc/index";\nexport { server };`,
  });
  fs.mkdirSync(path.join(nodeModulesFixture.webRoot, "node_modules", "local-rsc"), {
    recursive: true,
  });
  fs.writeFileSync(
    path.join(nodeModulesFixture.webRoot, "node_modules", "local-rsc", "index.ts"),
    "export const server = true;\n",
  );
  await expectFailure(t, nodeModulesFixture.repoRoot, /web-rsc-mode-guard: .*node_modules/);
});

test("alias imports cannot escape the effective web/src boundary", async (t) => {
  const { repoRoot, webRoot } = createFixture(t, {
    source: `import { bridge } from "@/../../outside/bridge";\nexport { bridge };`,
  });
  fs.mkdirSync(path.join(repoRoot, "outside"));
  fs.writeFileSync(path.join(repoRoot, "outside", "bridge.ts"), "export const bridge = true;\n");
  await expectFailure(t, repoRoot, /web-rsc-mode-guard: .*escapes the guarded web root/);
  assert.equal(fs.existsSync(path.join(webRoot, "vite.config.mjs")), true);
});

test("undeclared bare imports are rejected while declarative SPA imports pass", async (t) => {
  const invalid = createFixture(t, {
    source: `import { page } from "local-page";\nexport { page };`,
  });
  await expectFailure(t, invalid.repoRoot, /web-rsc-mode-guard: .*undeclared package or local alias/);

  const valid = createFixture(t, { source: validSource });
  await runGuard(valid.repoRoot);
});

test("declared packages must resolve to real modules inside web", async (t) => {
  const unresolved = createFixture(t, {
    source: `import { page } from "missing-page";\nexport { page };`,
  });
  const unresolvedPackage = JSON.parse(
    fs.readFileSync(path.join(unresolved.webRoot, "package.json"), "utf8"),
  );
  unresolvedPackage.dependencies["missing-page"] = "1.0.0";
  fs.writeFileSync(
    path.join(unresolved.webRoot, "package.json"),
    `${JSON.stringify(unresolvedPackage, null, 2)}\n`,
  );
  await expectFailure(t, unresolved.repoRoot, /web-rsc-mode-guard: .*cannot resolve package import/);

  const linked = createFixture(t, {
    source: `import { page } from "linked-page";\nexport { page };`,
  });
  const linkedPackage = JSON.parse(
    fs.readFileSync(path.join(linked.webRoot, "package.json"), "utf8"),
  );
  linkedPackage.dependencies["linked-page"] = "1.0.0";
  fs.writeFileSync(
    path.join(linked.webRoot, "package.json"),
    `${JSON.stringify(linkedPackage, null, 2)}\n`,
  );
  const outsidePackage = path.join(linked.repoRoot, "outside-package");
  fs.mkdirSync(outsidePackage);
  fs.writeFileSync(
    path.join(outsidePackage, "package.json"),
    `${JSON.stringify({ name: "linked-page", version: "1.0.0", module: "index.js" })}\n`,
  );
  fs.writeFileSync(path.join(outsidePackage, "index.js"), "export const page = true;\n");
  fs.mkdirSync(path.join(linked.webRoot, "node_modules"));
  fs.symlinkSync(outsidePackage, path.join(linked.webRoot, "node_modules", "linked-page"));
  await expectFailure(t, linked.repoRoot, /web-rsc-mode-guard: .*outside the guarded web root/);
});

test("virtual package modules fail closed", async (t) => {
  const { repoRoot, webRoot } = createFixture(t, {
    source: `import { page } from "virtual-page";\nexport { page };`,
    config: `
import { fileURLToPath } from "node:url";

export default {
  plugins: [{
    name: "virtual-page-test",
    resolveId(source) {
      return source === "virtual-page" ? "\\0virtual-page" : null;
    },
  }],
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
};
`,
  });
  const packageJson = JSON.parse(fs.readFileSync(path.join(webRoot, "package.json"), "utf8"));
  packageJson.dependencies["virtual-page"] = "1.0.0";
  fs.writeFileSync(
    path.join(webRoot, "package.json"),
    `${JSON.stringify(packageJson, null, 2)}\n`,
  );
  await expectFailure(t, repoRoot, /web-rsc-mode-guard: .*virtual module/);
});
