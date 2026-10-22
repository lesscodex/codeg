#!/usr/bin/env node

// Stage the maintained pi-acp source for desktop, server and Docker bundles.
// The adapter is a local Node ESM build; Pi itself remains a user-supplied
// runtime and is recorded separately in runtime-prerequisites.json.

import { execFileSync } from "node:child_process"
import { createHash } from "node:crypto"
import * as fs from "node:fs"
import { dirname, isAbsolute, join, relative, resolve } from "node:path"
import { fileURLToPath } from "node:url"
import process from "node:process"

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url))
const REPO_ROOT = resolve(SCRIPT_DIR, "../..")
const SUBMODULE_RELATIVE = "src-tauri/vendor/pi-acp"
const STAGING_RELATIVE = "src-tauri/build/staging/pi-acp"
const PINNED_GITLINK = "e943617d0790ad3f9d170b6680253f8f449a092e"
const PINNED_SOURCE_DIGEST =
  "fc4b2d625df2eb4343d9f45000effa3fd33f1c53797f6866db72beeaea6d41e7"
const PI_ACP_PACKAGE = "pi-acp"
const PI_ACP_BIN = "pi-acp"
const PI_MIN_RUNTIME_VERSION = "1.4.0"
const RUNTIME_PREREQUISITES = "runtime-prerequisites.json"
const SOURCE_INPUTS = [
  "package-lock.json",
  "package.json",
  "src",
  "tsconfig.json",
  "tsup.config.ts",
]

function fail(message) {
  throw new Error(message)
}

function defaultRun(command, args, options) {
  if (process.platform === "win32" && command === "npm") {
    return execFileSync(
      process.env.ComSpec || "cmd.exe",
      ["/d", "/s", "/c", "npm.cmd", ...args],
      options
    )
  }
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
  let stat
  try {
    stat = fsApi.statSync(file)
  } catch (error) {
    fail(`${label} cannot be inspected: ${file} (${error.message})`)
  }
  if (!stat.isFile()) fail(`${label} is not a file: ${file}`)
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

export function computeSourceDigest(fsApi, submoduleDir) {
  const hash = createHash("sha256")

  function walk(directory, relativeDirectory) {
    const entries = fsApi
      .readdirSync(directory, { withFileTypes: true })
      .sort((left, right) =>
        left.name < right.name ? -1 : left.name > right.name ? 1 : 0
      )
    for (const entry of entries) {
      const relativePath = `${relativeDirectory}/${entry.name}`
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
        fail(`unsupported source entry in pinned submodule: ${source}`)
      }
    }
  }

  for (const relativePath of SOURCE_INPUTS) {
    const source = join(submoduleDir, relativePath)
    if (!fsApi.existsSync(source)) {
      fail(`pinned source input is missing: ${source}`)
    }
    const stat = fsApi.lstatSync(source)
    if (stat.isDirectory()) {
      hash.update(`directory\0${relativePath}\0`)
      walk(source, relativePath)
    } else if (stat.isFile()) {
      const contents = fsApi.readFileSync(source)
      hash.update(`file\0${relativePath}\0${contents.length}\0`)
      hash.update(contents)
    } else {
      fail(`unsupported pinned source input: ${source}`)
    }
  }
  return hash.digest("hex")
}

function submoduleHead(run, fsApi, submoduleDir, gitlink) {
  if (
    !fsApi.existsSync(submoduleDir) ||
    !fsApi.existsSync(join(submoduleDir, ".git"))
  ) {
    fail(`pinned submodule is missing or uninitialized: ${submoduleDir}`)
  }

  const head = runGit(
    run,
    ["-C", submoduleDir, "rev-parse", "--verify", "HEAD^{commit}"],
    `pinned submodule is missing or uninitialized: ${submoduleDir}`
  ).trim()
  if (head !== gitlink) {
    fail(
      `submodule HEAD ${head || "<missing>"} does not match parent gitlink ${gitlink}`
    )
  }

  const status = runGit(
    run,
    ["-C", submoduleDir, "status", "--porcelain=v1", "--untracked-files=all"],
    `cannot inspect the pinned submodule worktree: ${submoduleDir}`
  )
  if (status.trim())
    fail(`pinned submodule worktree is not clean: ${submoduleDir}`)
}

function verifiedGitlink(
  run,
  fsApi,
  repoRoot,
  submoduleDir,
  sourceGitlink,
  expectedSourceDigest
) {
  if (sourceGitlink) {
    if (sourceGitlink !== PINNED_GITLINK) {
      fail(
        `Docker source gitlink ${sourceGitlink} does not match pinned gitlink ${PINNED_GITLINK}`
      )
    }
    if (!fsApi.existsSync(submoduleDir)) {
      fail(`pinned submodule source is missing: ${submoduleDir}`)
    }
    const sourceDigest = computeSourceDigest(fsApi, submoduleDir)
    if (sourceDigest !== expectedSourceDigest) {
      fail(
        `Docker source digest ${sourceDigest} does not match pinned source digest ${expectedSourceDigest}`
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
  submoduleHead(run, fsApi, submoduleDir, gitlink)
  return gitlink
}

function runtimeEntry(packageJson, submoduleDir) {
  const bin = packageJson.bin
  const entry =
    typeof bin === "string" ? bin : bin?.[PI_ACP_BIN] || packageJson.main
  if (typeof entry !== "string" || !entry) {
    fail("pi-acp package metadata has no runtime entry")
  }
  const entryFile = resolve(submoduleDir, entry)
  const entryRelative = relative(submoduleDir, entryFile)
  if (
    !entryRelative ||
    entryRelative.startsWith("..") ||
    isAbsolute(entryRelative)
  ) {
    fail(`pi-acp runtime entry escapes the pinned submodule: ${entry}`)
  }
  return { entryFile, entryRelative }
}

function lockDependency(packageLock, name) {
  const dependency = packageLock.packages?.[`node_modules/${name}`]
  if (!dependency || typeof dependency.version !== "string") {
    fail(`pi-acp lockfile is missing the ${name} dependency`)
  }
  return dependency
}

export function stagePiAcp({
  repoRoot = REPO_ROOT,
  stagingDir = null,
  sourceGitlink = null,
  expectedSourceDigest = PINNED_SOURCE_DIGEST,
  run = defaultRun,
  fsApi = fs,
  log = (message) => console.log(`[stage-pi-acp] ${message}`),
} = {}) {
  const root = resolve(repoRoot)
  const submoduleDir = join(root, SUBMODULE_RELATIVE)
  const stageDir = resolve(stagingDir || join(root, STAGING_RELATIVE))
  const packageFile = join(submoduleDir, "package.json")
  const lockFile = join(submoduleDir, "package-lock.json")

  log(`repository root: ${root}`)
  log(`pinned submodule: ${submoduleDir}`)
  const gitlink = verifiedGitlink(
    run,
    fsApi,
    root,
    submoduleDir,
    sourceGitlink,
    expectedSourceDigest
  )

  const packageJson = readJson(fsApi, packageFile, "pi-acp package.json")
  const packageLock = readJson(fsApi, lockFile, "pi-acp package-lock.json")
  const lockRoot = packageLock.packages?.[""]
  if (!lockRoot || typeof lockRoot !== "object") {
    fail("pi-acp package-lock.json has no root package metadata")
  }
  const metadata = [
    ["package.json", packageJson],
    ["package-lock.json", packageLock],
    ["package-lock.json root", lockRoot],
  ]
  for (const [label, value] of metadata) {
    if (
      !value ||
      typeof value !== "object" ||
      typeof value.name !== "string" ||
      typeof value.version !== "string"
    ) {
      fail(`${label} is missing root name/version metadata`)
    }
  }
  const names = new Set(metadata.map(([, value]) => value.name))
  const versions = new Set(metadata.map(([, value]) => value.version))
  if (names.size !== 1 || versions.size !== 1) {
    fail(
      `pi-acp package/lock metadata mismatch: ${[...names].join(", ")} ${[
        ...versions,
      ].join(", ")}`
    )
  }
  if (packageJson.name !== PI_ACP_PACKAGE) {
    fail(`unexpected pi-acp package name: ${packageJson.name}`)
  }
  if (typeof packageJson.bin?.[PI_ACP_BIN] !== "string") {
    fail("pi-acp package metadata has no pi-acp bin")
  }
  for (const dependency of Object.keys(packageJson.dependencies || {})) {
    lockDependency(packageLock, dependency)
  }
  const nodeRequired = packageJson.engines?.node
  if (typeof nodeRequired !== "string" || !nodeRequired) {
    fail("pi-acp package metadata has no Node.js engine requirement")
  }

  const { entryFile, entryRelative } = runtimeEntry(packageJson, submoduleDir)
  const extensionRelative = "dist/pi-mcp-extension.js"
  const extensionFile = join(submoduleDir, extensionRelative)
  log(
    `installing ${packageJson.name}@${packageJson.version} from package-lock.json`
  )
  run("npm", ["ci"], { cwd: submoduleDir, stdio: "inherit" })
  log("building through the maintained npm build script")
  run("npm", ["run", "build"], { cwd: submoduleDir, stdio: "inherit" })
  requireFile(fsApi, entryFile, "pi-acp build output")
  requireFile(fsApi, extensionFile, "pi-acp MCP extension build output")
  run("npm", ["prune", "--omit=dev"], { cwd: submoduleDir, stdio: "inherit" })

  const nodeModulesDir = join(submoduleDir, "node_modules")
  for (const dependency of Object.keys(packageJson.dependencies || {})) {
    if (!fsApi.existsSync(join(nodeModulesDir, dependency))) {
      fail(
        `bundled pi-acp dependency is missing: ${join(nodeModulesDir, dependency)}`
      )
    }
  }

  fsApi.rmSync(stageDir, { recursive: true, force: true })
  fsApi.mkdirSync(dirname(join(stageDir, entryRelative)), { recursive: true })
  fsApi.cpSync(join(submoduleDir, "dist"), join(stageDir, "dist"), {
    recursive: true,
  })
  fsApi.copyFileSync(packageFile, join(stageDir, "package.json"))
  fsApi.cpSync(nodeModulesDir, join(stageDir, "node_modules"), {
    recursive: true,
  })
  const prerequisites = {
    schemaVersion: 1,
    adapter: {
      name: packageJson.name,
      version: packageJson.version,
      gitlink,
      entry: entryRelative,
      mcpExtension: extensionRelative,
    },
    runtime: {
      node: {
        required: true,
        engine: nodeRequired,
        executable: "node",
      },
      pi: {
        required: true,
        minimumVersion: PI_MIN_RUNTIME_VERSION,
        executable: "pi",
        environmentVariable: "PI_ACP_PI_COMMAND",
        package: "@earendil-works/pi-coding-agent",
      },
    },
  }
  fsApi.writeFileSync(
    join(stageDir, RUNTIME_PREREQUISITES),
    `${JSON.stringify(prerequisites, null, 2)}\n`
  )

  requireFile(fsApi, join(stageDir, entryRelative), "staged pi-acp entry")
  requireFile(
    fsApi,
    join(stageDir, extensionRelative),
    "staged pi-acp MCP extension"
  )
  requireFile(
    fsApi,
    join(stageDir, "package.json"),
    "staged pi-acp package.json"
  )
  requireFile(
    fsApi,
    join(stageDir, RUNTIME_PREREQUISITES),
    "staged pi-acp runtime prerequisites"
  )
  log(`staged runtime artifact at ${stageDir}`)
  return {
    stageDir,
    entry: join(stageDir, entryRelative),
    extension: join(stageDir, extensionRelative),
    packageName: packageJson.name,
    packageVersion: packageJson.version,
    gitlink,
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

function isMainModule() {
  return (
    process.argv[1] &&
    resolve(process.argv[1]) === fileURLToPath(import.meta.url)
  )
}

if (isMainModule()) {
  try {
    stagePiAcp({ sourceGitlink: parseSourceGitlink(process.argv.slice(2)) })
  } catch (error) {
    console.error(`[stage-pi-acp][ERROR] ${error.message}`)
    process.exitCode = 1
  }
}
