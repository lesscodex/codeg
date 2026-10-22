import assert from "node:assert/strict"
import { test } from "node:test"
import * as fs from "node:fs"
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"

import { computeSourceDigest, stagePiAcp } from "./stage-pi-acp.mjs"

const SUBMODULE_RELATIVE = "src-tauri/vendor/pi-acp"
const GITLINK = "e943617d0790ad3f9d170b6680253f8f449a092e"
const PACKAGE_VERSION = "0.0.34"

function fixture({
  initialized = true,
  buildOutput = true,
  lockVersion = PACKAGE_VERSION,
} = {}) {
  const root = mkdtempSync(join(tmpdir(), "codeg-pi-acp-stage-"))
  const submodule = join(root, SUBMODULE_RELATIVE)
  mkdirSync(submodule, { recursive: true })
  if (initialized) writeFileSync(join(submodule, ".git"), "gitdir: fixture\n")
  mkdirSync(join(submodule, "src"), { recursive: true })
  writeFileSync(join(submodule, "src/index.ts"), "export default 1\n")
  writeFileSync(join(submodule, "tsconfig.json"), "{}\n")
  writeFileSync(join(submodule, "tsup.config.ts"), "export default {}\n")

  const packageJson = {
    name: "pi-acp",
    version: PACKAGE_VERSION,
    engines: { node: ">=22.19.0" },
    type: "module",
    main: "dist/index.js",
    bin: { "pi-acp": "dist/index.js" },
    dependencies: {
      "@agentclientprotocol/sdk": "^0.26.0",
      "cross-spawn": "^7.0.6",
      zod: "^3.25.0",
    },
  }
  writeFileSync(join(submodule, "package.json"), JSON.stringify(packageJson))
  writeFileSync(
    join(submodule, "package-lock.json"),
    JSON.stringify({
      name: "pi-acp",
      version: lockVersion,
      lockfileVersion: 3,
      packages: {
        "": {
          name: "pi-acp",
          version: lockVersion,
          dependencies: packageJson.dependencies,
          engines: packageJson.engines,
        },
        "node_modules/@agentclientprotocol/sdk": { version: "0.26.0" },
        "node_modules/cross-spawn": { version: "7.0.6" },
        "node_modules/zod": { version: "3.25.0" },
      },
    })
  )

  if (buildOutput) {
    mkdirSync(join(submodule, "dist"), { recursive: true })
    writeFileSync(join(submodule, "dist/index.js"), "export default 1\n")
    writeFileSync(
      join(submodule, "dist/pi-mcp-extension.js"),
      "export default 1\n"
    )
  }
  for (const dependency of ["@agentclientprotocol/sdk", "cross-spawn", "zod"]) {
    mkdirSync(join(submodule, "node_modules", dependency), { recursive: true })
    writeFileSync(
      join(submodule, "node_modules", dependency, "package.json"),
      "{}\n"
    )
  }
  return { root, submodule }
}

function runner({ head = GITLINK, gitlink = GITLINK } = {}) {
  const calls = []
  const run = (command, args, options) => {
    calls.push({ command, args, options })
    if (command === "git" && args.includes("ls-tree")) {
      return `160000 commit ${gitlink}\t${SUBMODULE_RELATIVE}\n`
    }
    if (command === "git" && args.includes("rev-parse")) return `${head}\n`
    if (command === "git" && args.includes("status")) return ""
    if (command === "npm") return ""
    throw new Error(`unexpected command: ${command} ${args.join(" ")}`)
  }
  return { calls, run }
}

function cleanup(root) {
  rmSync(root, { recursive: true, force: true })
}

test("fails for a missing or uninitialized submodule", () => {
  const { root } = fixture({ initialized: false })
  try {
    assert.throws(
      () => stagePiAcp({ repoRoot: root, run: runner().run, log: () => {} }),
      /missing or uninitialized/
    )
  } finally {
    cleanup(root)
  }
})

test("fails when the parent gitlink is not the maintained revision", () => {
  const { root } = fixture()
  try {
    assert.throws(
      () =>
        stagePiAcp({
          repoRoot: root,
          run: runner({ gitlink: "b".repeat(40) }).run,
          log: () => {},
        }),
      /does not match pinned gitlink/
    )
  } finally {
    cleanup(root)
  }
})

test("fails when package and lockfile versions disagree", () => {
  const { root } = fixture({ lockVersion: "9.9.9" })
  try {
    assert.throws(
      () => stagePiAcp({ repoRoot: root, run: runner().run, log: () => {} }),
      /metadata mismatch/
    )
  } finally {
    cleanup(root)
  }
})

test("fails when the MCP extension build output is missing", () => {
  const { root, submodule } = fixture()
  rmSync(join(submodule, "dist/pi-mcp-extension.js"))
  try {
    assert.throws(
      () => stagePiAcp({ repoRoot: root, run: runner().run, log: () => {} }),
      /MCP extension build output is missing/
    )
  } finally {
    cleanup(root)
  }
})

test("stages the complete dist layout and production dependencies", () => {
  const { root, submodule } = fixture()
  try {
    const fake = runner()
    const result = stagePiAcp({ repoRoot: root, run: fake.run, log: () => {} })
    const staged = result.stageDir
    assert.equal(
      readFileSync(join(staged, "dist/index.js"), "utf8"),
      "export default 1\n"
    )
    assert.equal(
      readFileSync(join(staged, "dist/pi-mcp-extension.js"), "utf8"),
      "export default 1\n"
    )
    assert.equal(
      JSON.parse(readFileSync(join(staged, "package.json"))).version,
      PACKAGE_VERSION
    )
    assert.equal(existsSync(join(staged, "src")), false)
    assert.equal(existsSync(join(staged, "package-lock.json")), false)
    assert.equal(
      existsSync(join(staged, "node_modules/zod/package.json")),
      true
    )
    assert.deepEqual(
      JSON.parse(readFileSync(join(staged, "runtime-prerequisites.json"))),
      {
        schemaVersion: 1,
        adapter: {
          name: "pi-acp",
          version: PACKAGE_VERSION,
          gitlink: GITLINK,
          entry: "dist/index.js",
          mcpExtension: "dist/pi-mcp-extension.js",
        },
        runtime: {
          node: { required: true, engine: ">=22.19.0", executable: "node" },
          pi: {
            required: true,
            minimumVersion: "1.4.0",
            executable: "pi",
            environmentVariable: "PI_ACP_PI_COMMAND",
            package: "@earendil-works/pi-coding-agent",
          },
        },
      }
    )
    assert.deepEqual(
      fake.calls
        .filter(({ command }) => command === "npm")
        .map(({ args }) => args),
      [["ci"], ["run", "build"], ["prune", "--omit=dev"]]
    )
    assert.equal(
      computeSourceDigest(fs, submodule),
      computeSourceDigest(fs, submodule)
    )
  } finally {
    cleanup(root)
  }
})

test("validates the Docker source digest", () => {
  const { root, submodule } = fixture()
  try {
    const expectedSourceDigest = computeSourceDigest(fs, submodule)
    writeFileSync(join(submodule, "src/index.ts"), "changed source\n")
    assert.throws(
      () =>
        stagePiAcp({
          repoRoot: root,
          sourceGitlink: GITLINK,
          expectedSourceDigest,
          run: runner().run,
          log: () => {},
        }),
      /does not match pinned source digest/
    )
  } finally {
    cleanup(root)
  }
})
