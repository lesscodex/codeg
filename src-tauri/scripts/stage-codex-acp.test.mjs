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

import { computeSourceDigest, stageCodexAcp } from "./stage-codex-acp.mjs"

const SUBMODULE_RELATIVE = "src-tauri/vendor/codex-acp"
const GITLINK = "9d3eee456d798a3512ed52eab9e354d7e4f7bb21"
const PACKAGE_NAME = "@agentclientprotocol/codex-acp"
const PACKAGE_VERSION = "2.5.0"

function fixture({
  initialized = true,
  buildOutput = true,
  lockVersion = PACKAGE_VERSION,
} = {}) {
  const root = mkdtempSync(join(tmpdir(), "codeg-codex-acp-stage-"))
  const submodule = join(root, SUBMODULE_RELATIVE)
  mkdirSync(submodule, { recursive: true })
  if (initialized) writeFileSync(join(submodule, ".git"), "gitdir: fixture\n")
  writeFileSync(join(submodule, "build.mjs"), "// fixture build\n")
  writeFileSync(join(submodule, "tsconfig.json"), "{}\n")
  mkdirSync(join(submodule, "src"), { recursive: true })
  writeFileSync(join(submodule, "src/index.ts"), "export default 1\n")

  const packageJson = {
    name: PACKAGE_NAME,
    version: PACKAGE_VERSION,
    type: "module",
    main: "dist/index.js",
    bin: { "codex-acp": "dist/index.js" },
    dependencies: { "@openai/codex": "^0.165.0" },
  }
  writeFileSync(join(submodule, "package.json"), JSON.stringify(packageJson))
  writeFileSync(
    join(submodule, "package-lock.json"),
    JSON.stringify({
      name: PACKAGE_NAME,
      version: lockVersion,
      lockfileVersion: 3,
      packages: {
        "": {
          name: PACKAGE_NAME,
          version: lockVersion,
          dependencies: packageJson.dependencies,
        },
        "node_modules/@openai/codex": {
          version: "0.165.0",
        },
      },
    })
  )

  if (buildOutput) {
    mkdirSync(join(submodule, "dist"), { recursive: true })
    writeFileSync(join(submodule, "dist/index.js"), "export default 1\n")
  }
  mkdirSync(join(submodule, "node_modules/@openai/codex/bin"), {
    recursive: true,
  })
  writeFileSync(
    join(submodule, "node_modules/@openai/codex/bin/codex.js"),
    "export default 1\n"
  )
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
      () => stageCodexAcp({ repoRoot: root, run: runner().run, log: () => {} }),
      /missing or uninitialized/
    )
  } finally {
    cleanup(root)
  }
})

test("fails when the submodule HEAD does not match the parent gitlink", () => {
  const { root } = fixture()
  try {
    const fake = runner({ head: "b".repeat(40) })
    assert.throws(
      () => stageCodexAcp({ repoRoot: root, run: fake.run, log: () => {} }),
      /does not match parent gitlink/
    )
  } finally {
    cleanup(root)
  }
})

test("fails when Docker source metadata does not match the pinned gitlink", () => {
  const { root } = fixture()
  try {
    assert.throws(
      () =>
        stageCodexAcp({
          repoRoot: root,
          sourceGitlink: "b".repeat(40),
          run: runner().run,
          log: () => {},
        }),
      /does not match pinned gitlink/
    )
  } finally {
    cleanup(root)
  }
})

test("fails when package and lockfile versions disagree", () => {
  const { root } = fixture({ buildOutput: false, lockVersion: "9.9.9" })
  try {
    assert.throws(
      () => stageCodexAcp({ repoRoot: root, run: runner().run, log: () => {} }),
      /metadata mismatch/
    )
  } finally {
    cleanup(root)
  }
})

test("fails when the upstream build output is missing", () => {
  const { root } = fixture({ buildOutput: false })
  try {
    assert.throws(
      () => stageCodexAcp({ repoRoot: root, run: runner().run, log: () => {} }),
      /build output is missing/
    )
  } finally {
    cleanup(root)
  }
})

test("stages the runtime entry with bundled production dependencies", () => {
  const { root, submodule } = fixture()
  writeFileSync(join(submodule, "README.md"), "not staged\n")
  try {
    const fake = runner()
    const result = stageCodexAcp({
      repoRoot: root,
      run: fake.run,
      log: () => {},
    })
    const staged = result.stageDir
    assert.equal(
      readFileSync(join(staged, "dist/index.js"), "utf8"),
      "export default 1\n"
    )
    assert.equal(
      JSON.parse(readFileSync(join(staged, "package.json"))).version,
      PACKAGE_VERSION
    )
    assert.equal(existsSync(join(staged, "src")), false)
    assert.equal(existsSync(join(staged, "README.md")), false)
    assert.equal(existsSync(join(staged, "package-lock.json")), false)
    assert.equal(
      existsSync(join(staged, "node_modules/@openai/codex/bin/codex.js")),
      true
    )
    assert.deepEqual(
      JSON.parse(
        readFileSync(join(staged, "runtime-prerequisites.json"), "utf8")
      ),
      {
        schemaVersion: 1,
        adapter: {
          name: PACKAGE_NAME,
          version: PACKAGE_VERSION,
          gitlink: GITLINK,
          entry: "dist/index.js",
        },
        runtime: {
          node: {
            required: true,
            executable: "node",
          },
          codex: {
            bundled: true,
            package: "@openai/codex",
            lockedVersion: "0.165.0",
            executablePath: {
              required: false,
              environmentVariable: "CODEX_PATH",
            },
          },
        },
      }
    )

    const npmCommands = fake.calls
      .filter(({ command }) => command === "npm")
      .map(({ args }) => args)
    assert.deepEqual(npmCommands, [
      ["ci"],
      ["run", "build"],
      ["prune", "--omit=dev"],
    ])
  } finally {
    cleanup(root)
  }
})

test("stages the same layout from a pinned Docker source context", () => {
  const { root, submodule } = fixture()
  try {
    const result = stageCodexAcp({
      repoRoot: root,
      sourceGitlink: GITLINK,
      expectedSourceDigest: computeSourceDigest(fs, submodule),
      run: runner().run,
      log: () => {},
    })
    const manifest = JSON.parse(
      readFileSync(join(result.stageDir, "runtime-prerequisites.json"), "utf8")
    )
    assert.equal(manifest.adapter.gitlink, GITLINK)
    assert.equal(manifest.runtime.codex.bundled, true)
    assert.equal(manifest.runtime.codex.executablePath.required, false)
  } finally {
    cleanup(root)
  }
})

test("fails when Docker build inputs differ from the pinned source", () => {
  const { root, submodule } = fixture()
  try {
    const expectedSourceDigest = computeSourceDigest(fs, submodule)
    writeFileSync(join(submodule, "src/index.ts"), "changed source\n")
    assert.throws(
      () =>
        stageCodexAcp({
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
