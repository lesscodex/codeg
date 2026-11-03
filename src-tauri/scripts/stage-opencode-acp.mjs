#!/usr/bin/env bun

import { execFileSync } from "node:child_process"
import { createHash } from "node:crypto"
import * as fs from "node:fs"
import { dirname, join, resolve } from "node:path"
import { fileURLToPath } from "node:url"
import process from "node:process"
import { acpVersion } from "./acp-adapter-versions.mjs"

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url))
const REPO_ROOT = resolve(SCRIPT_DIR, "../..")
const SUBMODULE_RELATIVE = "src-tauri/vendor/opencode-acp"
const STAGING_RELATIVE = "src-tauri/build/staging/opencode-acp"
const PINNED_GITLINK = "2cfbac2686718fef890e03748a8a66d6946c7de0"
const PINNED_SOURCE_DIGEST =
  "b0deea870ab924613cb15f0d04cc5cae18837c48532e96323b8bd4ef3c44d03f"
const BUILD_RELATIVE = "packages/cli/dist-codeg"
const RUNTIME_PREREQUISITES = "runtime-prerequisites.json"
const GENERATED_DIRS = new Set([
  ".git",
  "node_modules",
  "dist",
  "dist-codeg",
  "build",
  ".cache",
])
const SENSITIVE_NAMES = new Set([
  ".env",
  ".env.local",
  ".env.production",
  "auth.json",
  "credentials.json",
])

function fail(message) {
  throw new Error(message)
}

function defaultRun(command, args, options) {
  return execFileSync(command, args, options)
}

function readJson(fsApi, file, label) {
  if (!fsApi.existsSync(file)) fail(`${label} is missing: ${file}`)
  try {
    return JSON.parse(fsApi.readFileSync(file, "utf8"))
  } catch (error) {
    fail(`${label} is not valid JSON: ${file} (${error.message})`)
  }
}

function requireFile(fsApi, file, label) {
  if (!fsApi.existsSync(file)) fail(`${label} is missing: ${file}`)
  if (!fsApi.statSync(file).isFile()) fail(`${label} is not a file: ${file}`)
}

function runGit(run, args, errorMessage) {
  try {
    return String(run("git", args, { encoding: "utf8" }))
  } catch (error) {
    fail(`${errorMessage} (${error.message})`)
  }
}

function parentGitlink(run, repoRoot) {
  const output = runGit(
    run,
    ["-C", repoRoot, "ls-tree", "HEAD", "--", SUBMODULE_RELATIVE],
    `cannot read the parent gitlink for ${SUBMODULE_RELATIVE}`
  )
  const line = output.trim().split(/\r?\n/).find(Boolean)
  const match = line?.match(/^160000 commit ([0-9a-f]{40})\t(.+)$/)
  if (!match || match[2] !== SUBMODULE_RELATIVE) {
    fail(
      `parent gitlink is missing or not a 160000 entry for ${SUBMODULE_RELATIVE}`
    )
  }
  return match[1]
}

export function sourceDigest(fsApi, sourceDir) {
  const hash = createHash("sha256")

  function walk(directory, relativeDirectory) {
    const entries = fsApi
      .readdirSync(directory, { withFileTypes: true })
      .sort((left, right) =>
        left.name < right.name ? -1 : left.name > right.name ? 1 : 0
      )
    for (const entry of entries) {
      if (GENERATED_DIRS.has(entry.name) || SENSITIVE_NAMES.has(entry.name))
        continue
      const relativePath = relativeDirectory
        ? `${relativeDirectory}/${entry.name}`
        : entry.name
      const source = join(directory, entry.name)
      if (entry.isDirectory()) {
        hash.update(`directory\0${relativePath}\0`)
        walk(source, relativePath)
      } else if (entry.isSymbolicLink()) {
        hash.update(`symlink\0${relativePath}\0${fsApi.readlinkSync(source)}\0`)
      } else if (entry.isFile()) {
        const contents = fsApi.readFileSync(source)
        hash.update(`file\0${relativePath}\0${contents.length}\0`)
        hash.update(contents)
      } else {
        fail(`unsupported source entry: ${source}`)
      }
    }
  }

  walk(sourceDir, "")
  return hash.digest("hex")
}

function submoduleHead(run, fsApi, sourceDir, gitlink) {
  if (
    !fsApi.existsSync(sourceDir) ||
    !fsApi.existsSync(join(sourceDir, ".git"))
  ) {
    fail(`pinned submodule is missing or uninitialized: ${sourceDir}`)
  }
  const head = runGit(
    run,
    ["-C", sourceDir, "rev-parse", "--verify", "HEAD^{commit}"],
    `cannot read the pinned submodule HEAD: ${sourceDir}`
  ).trim()
  if (head !== gitlink)
    fail(`submodule HEAD ${head || "<missing>"} does not match ${gitlink}`)
  const status = runGit(
    run,
    ["-C", sourceDir, "status", "--porcelain=v1", "--untracked-files=all"],
    `cannot inspect the pinned submodule worktree: ${sourceDir}`
  )
  if (status.trim())
    fail(`pinned submodule worktree is not clean: ${sourceDir}`)
}

function verifiedGitlink(
  run,
  fsApi,
  repoRoot,
  sourceDir,
  sourceGitlink,
  expectedSourceDigest
) {
  if (sourceGitlink) {
    if (sourceGitlink !== PINNED_GITLINK) {
      fail(
        `Docker source gitlink ${sourceGitlink} does not match pinned gitlink ${PINNED_GITLINK}`
      )
    }
    if (!fsApi.existsSync(sourceDir)) {
      fail(`pinned source is missing: ${sourceDir}`)
    }
    const actualDigest = sourceDigest(fsApi, sourceDir)
    if (actualDigest !== expectedSourceDigest) {
      fail(
        `Docker source digest ${actualDigest} does not match pinned source digest ${expectedSourceDigest}`
      )
    }
    return sourceGitlink
  }

  const gitlink = parentGitlink(run, repoRoot)
  if (gitlink !== PINNED_GITLINK) {
    fail(
      `parent gitlink ${gitlink} does not match pinned gitlink ${PINNED_GITLINK}`
    )
  }
  submoduleHead(run, fsApi, sourceDir, gitlink)
  return gitlink
}

function buildTarget(platform, arch) {
  const os = platform === "win32" ? "windows" : platform
  return {
    directory: `cli-${os}-${arch}`,
    executable: platform === "win32" ? "opencode.exe" : "opencode",
  }
}

export function stage({
  fsApi = fs,
  run = defaultRun,
  repoRoot = REPO_ROOT,
  sourceGitlink = parseSourceGitlink(process.argv.slice(2)),
  expectedSourceDigest = PINNED_SOURCE_DIGEST,
  platform = process.platform,
  arch = process.arch,
  expectedAdapterVersion = null,
} = {}) {
  const adapterVersion =
    expectedAdapterVersion ||
    acpVersion("opencode-adapter", { fsApi, repoRoot })
  const sourceDir = join(repoRoot, SUBMODULE_RELATIVE)
  const stageDir = join(repoRoot, STAGING_RELATIVE)
  const buildDir = join(sourceDir, BUILD_RELATIVE)
  const gitlink = verifiedGitlink(
    run,
    fsApi,
    repoRoot,
    sourceDir,
    sourceGitlink,
    expectedSourceDigest
  )

  const rootPackage = readJson(
    fsApi,
    join(sourceDir, "package.json"),
    "OpenCode root package.json"
  )
  const packageFile = join(sourceDir, "packages", "cli", "package.json")
  const packageJson = readJson(fsApi, packageFile, "OpenCode CLI package.json")
  requireFile(fsApi, join(sourceDir, "bun.lock"), "OpenCode bun.lock")
  requireFile(fsApi, join(sourceDir, "LICENSE"), "OpenCode LICENSE")
  requireFile(fsApi, join(sourceDir, "README.md"), "OpenCode README")
  if (
    rootPackage.name !== "opencode" ||
    rootPackage.version !== adapterVersion
  ) {
    fail(
      `unexpected OpenCode root identity: ${rootPackage.name}@${rootPackage.version}; expected opencode@${adapterVersion}`
    )
  }
  if (rootPackage.packageManager !== "bun@1.4.4")
    fail(`unsupported OpenCode package manager: ${rootPackage.packageManager}`)
  if (
    packageJson.name !== "@opencode/cli" ||
    packageJson.version !== adapterVersion
  ) {
    fail(
      `unexpected OpenCode CLI identity: ${packageJson.name}@${packageJson.version}; expected @opencode/cli@${adapterVersion}`
    )
  }

  run("bun", ["install", "--frozen-lockfile"], {
    cwd: sourceDir,
    stdio: "inherit",
  })
  run(
    "bun",
    [
      "run",
      "--cwd",
      "packages/cli",
      "script/build.ts",
      "--single",
      "--skip-install",
      "--skip-web-ui",
      `--outdir=${buildDir}`,
    ],
    {
      cwd: sourceDir,
      stdio: "inherit",
      env: {
        ...process.env,
        OPENCODE_CHANNEL: "latest",
        OPENCODE_VERSION: packageJson.version,
      },
    }
  )

  const target = buildTarget(platform, arch)
  const built = join(buildDir, target.directory, "bin", target.executable)
  requireFile(fsApi, built, "built OpenCode executable")
  fsApi.rmSync(stageDir, { recursive: true, force: true })
  fsApi.mkdirSync(join(stageDir, "bin"), { recursive: true })
  const entry = `bin/${target.executable}`
  fsApi.copyFileSync(built, join(stageDir, entry))
  if (platform !== "win32") fsApi.chmodSync(join(stageDir, entry), 0o755)
  fsApi.copyFileSync(packageFile, join(stageDir, "package.json"))
  fsApi.copyFileSync(join(sourceDir, "LICENSE"), join(stageDir, "LICENSE"))
  fsApi.copyFileSync(join(sourceDir, "README.md"), join(stageDir, "README.md"))
  fsApi.writeFileSync(
    join(stageDir, RUNTIME_PREREQUISITES),
    `${JSON.stringify(
      {
        schemaVersion: 1,
        adapter: {
          name: "OpenCode",
          version: packageJson.version,
          gitlink,
          sourceDigest: expectedSourceDigest,
          entry,
        },
        runtime: { bundled: true },
      },
      null,
      2
    )}\n`
  )
  return {
    stageDir,
    entry: join(stageDir, entry),
    packageName: packageJson.name,
    packageVersion: packageJson.version,
  }
}

function parseSourceGitlink(argv) {
  const prefix = "--source-gitlink="
  const argument = argv.find((value) => value.startsWith(prefix))
  if (!argument) return null
  const gitlink = argument.slice(prefix.length)
  if (!/^[0-9a-f]{40}$/.test(gitlink)) {
    fail(`invalid --source-gitlink value: ${gitlink || "<empty>"}`)
  }
  return gitlink
}

if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  try {
    const result = stage()
    console.log(
      `staged ${result.packageName}@${result.packageVersion} at ${result.stageDir}`
    )
  } catch (error) {
    console.error(error instanceof Error ? error.message : error)
    process.exitCode = 1
  }
}
