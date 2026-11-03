import assert from "node:assert/strict"
import * as fs from "node:fs"
import { dirname, join } from "node:path"
import os from "node:os"
import test from "node:test"

import { sourceDigest, stage } from "./stage-opencode-acp.mjs"

const SUBMODULE_RELATIVE = "src-tauri/vendor/opencode-acp"
const GITLINK = "4c3fedb08a4b47099570d06774c795188282fa84"

function fixture() {
  const root = fs.mkdtempSync(join(os.tmpdir(), "stage-opencode-acp-"))
  const source = join(root, SUBMODULE_RELATIVE)
  fs.mkdirSync(join(source, "packages", "cli"), { recursive: true })
  fs.writeFileSync(join(source, ".git"), "gitdir: fixture\n")
  fs.writeFileSync(
    join(source, "package.json"),
    JSON.stringify({
      name: "opencode",
      version: "2.2.0",
      packageManager: "bun@1.4.4",
    })
  )
  fs.writeFileSync(
    join(source, "packages", "cli", "package.json"),
    JSON.stringify({ name: "@opencode/cli", version: "2.2.0" })
  )
  fs.writeFileSync(join(source, "bun.lock"), "lockfile\n")
  fs.writeFileSync(join(source, "LICENSE"), "MIT\n")
  fs.writeFileSync(join(source, "README.md"), "OpenCode\n")
  fs.writeFileSync(join(source, "source.ts"), "export const source = true\n")
  return { root, source, digest: sourceDigest(fs, source) }
}

function fakeBuilder(
  target,
  { gitlink = GITLINK, head = GITLINK, status = "" } = {}
) {
  const calls = []
  return {
    calls,
    run(command, args) {
      calls.push([command, args])
      if (command === "git" && args.includes("ls-tree"))
        return `160000 commit ${gitlink}\t${SUBMODULE_RELATIVE}\n`
      if (command === "git" && args.includes("rev-parse")) return `${head}\n`
      if (command === "git" && args.includes("status")) return status
      if (command !== "bun")
        throw new Error(`unexpected command: ${command} ${args.join(" ")}`)
      if (!args.some((value) => value.startsWith("--outdir="))) return ""
      const outdir = args
        .find((value) => value.startsWith("--outdir="))
        .slice("--outdir=".length)
      const executable = join(
        outdir,
        target.directory,
        "bin",
        target.executable
      )
      fs.mkdirSync(dirname(executable), { recursive: true })
      fs.writeFileSync(executable, "binary\n")
      return ""
    },
  }
}

test("stages the pinned official Linux OpenCode executable and provenance", () => {
  const input = fixture()
  try {
    const builder = fakeBuilder({
      directory: "cli-linux-x64",
      executable: "opencode",
    })
    const result = stage({
      repoRoot: input.root,
      run: builder.run,
      platform: "linux",
      arch: "x64",
      expectedSourceDigest: input.digest,
    })
    assert.equal(fs.readFileSync(result.entry, "utf8"), "binary\n")
    assert.equal(
      fs.readFileSync(join(result.stageDir, "LICENSE"), "utf8"),
      "MIT\n"
    )
    assert.equal(
      fs.readFileSync(join(result.stageDir, "README.md"), "utf8"),
      "OpenCode\n"
    )
    assert.deepEqual(
      JSON.parse(
        fs.readFileSync(
          join(result.stageDir, "runtime-prerequisites.json"),
          "utf8"
        )
      ),
      {
        schemaVersion: 1,
        adapter: {
          name: "OpenCode",
          version: "2.2.0",
          gitlink: GITLINK,
          sourceDigest: input.digest,
          entry: "bin/opencode",
        },
        runtime: { bundled: true },
      }
    )
    const bunCalls = builder.calls.filter(([command]) => command === "bun")
    assert.deepEqual(bunCalls[0], ["bun", ["install", "--frozen-lockfile"]])
    assert.equal(bunCalls[1][0], "bun")
  } finally {
    fs.rmSync(input.root, { recursive: true, force: true })
  }
})

test("stages the Windows executable at the manifest path", () => {
  const input = fixture()
  try {
    const result = stage({
      repoRoot: input.root,
      run: fakeBuilder({
        directory: "cli-windows-x64",
        executable: "opencode.exe",
      }).run,
      platform: "win32",
      arch: "x64",
      expectedSourceDigest: input.digest,
    })
    assert.equal(result.entry, join(result.stageDir, "bin", "opencode.exe"))
    assert.equal(fs.readFileSync(result.entry, "utf8"), "binary\n")
    assert.equal(
      JSON.parse(
        fs.readFileSync(
          join(result.stageDir, "runtime-prerequisites.json"),
          "utf8"
        )
      ).adapter.entry,
      "bin/opencode.exe"
    )
  } finally {
    fs.rmSync(input.root, { recursive: true, force: true })
  }
})

test("normal checkout trusts Git identity instead of platform-specific bytes", () => {
  const input = fixture()
  try {
    fs.writeFileSync(
      join(input.source, "source.ts"),
      "export const source = true\r\n"
    )
    const result = stage({
      repoRoot: input.root,
      run: fakeBuilder({
        directory: "cli-windows-x64",
        executable: "opencode.exe",
      }).run,
      platform: "win32",
      arch: "x64",
      expectedSourceDigest: input.digest,
    })
    assert.equal(fs.readFileSync(result.entry, "utf8"), "binary\n")
  } finally {
    fs.rmSync(input.root, { recursive: true, force: true })
  }
})

test("normal checkout rejects mismatched or dirty Git identity", () => {
  const scenarios = [
    {
      options: { gitlink: "0".repeat(40) },
      error: /parent gitlink .* does not match pinned gitlink/,
    },
    {
      options: { head: "0".repeat(40) },
      error: /submodule HEAD .* does not match/,
    },
    {
      options: { status: " M source.ts\n" },
      error: /pinned submodule worktree is not clean/,
    },
  ]

  for (const scenario of scenarios) {
    const input = fixture()
    try {
      const builder = fakeBuilder(
        { directory: "cli-linux-x64", executable: "opencode" },
        scenario.options
      )
      assert.throws(
        () =>
          stage({
            repoRoot: input.root,
            run: builder.run,
            platform: "linux",
            arch: "x64",
            expectedSourceDigest: input.digest,
          }),
        scenario.error
      )
      assert.equal(
        builder.calls.some(([command]) => command === "bun"),
        false
      )
    } finally {
      fs.rmSync(input.root, { recursive: true, force: true })
    }
  }
})

test("rejects Docker source digest drift before installing dependencies", () => {
  const input = fixture()
  try {
    fs.writeFileSync(join(input.source, "source.ts"), "changed source\n")
    let called = false
    assert.throws(
      () =>
        stage({
          repoRoot: input.root,
          run: () => {
            called = true
            return ""
          },
          platform: "linux",
          arch: "x64",
          sourceGitlink: GITLINK,
          expectedSourceDigest: input.digest,
        }),
      /Docker source digest .* does not match pinned source digest/
    )
    assert.equal(called, false)
  } finally {
    fs.rmSync(input.root, { recursive: true, force: true })
  }
})

test("rejects Docker source metadata for a different gitlink", () => {
  const input = fixture()
  try {
    assert.throws(
      () =>
        stage({
          repoRoot: input.root,
          run: fakeBuilder({
            directory: "cli-linux-x64",
            executable: "opencode",
          }).run,
          sourceGitlink: "0".repeat(40),
          expectedSourceDigest: input.digest,
          platform: "linux",
          arch: "x64",
        }),
      /does not match pinned gitlink/
    )
  } finally {
    fs.rmSync(input.root, { recursive: true, force: true })
  }
})
