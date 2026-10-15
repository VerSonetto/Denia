import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const semver = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?(?:\+[0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*)?$/

export function validateReleaseVersions({ workspace, desktop, frontend, lockfile, tag = '' }) {
  const section = workspace.match(/^\[workspace\.package\]\s*\r?\n([\s\S]*?)(?=^\[|$(?![\s\S]))/m)?.[1]
  const version = section?.match(/^version\s*=\s*"([^"]+)"\s*$/m)?.[1]
  if (!version || !semver.test(version)) throw new Error('Cargo workspace must have a valid semantic version.')
  const releaseTag = tag.trim() || `v${version}`
  if (releaseTag !== `v${version}`) throw new Error(`Release tag ${releaseTag} must match the local version v${version}. Update local versions before publishing.`)
  for (const [name, actual] of [['desktop', desktop.version], ['frontend', frontend.version]]) {
    if (actual !== version) throw new Error(`${name} version ${actual} does not match workspace version ${version}.`)
  }
  const packages = lockfile.split(/^\[\[package\]\]\s*$/m).slice(1)
  const local = packages.filter(block => /^name\s*=\s*"denia(?:-[^"]+)?"\s*$/m.test(block) && !/^source\s*=/m.test(block))
  if (!local.length) throw new Error('Cargo.lock contains no local Denia packages.')
  for (const block of local) {
    const name = block.match(/^name\s*=\s*"([^"]+)"/m)?.[1]
    const actual = block.match(/^version\s*=\s*"([^"]+)"/m)?.[1]
    if (actual !== version) throw new Error(`${name} in Cargo.lock is ${actual}; run cargo update --workspace to synchronize ${version}.`)
  }
  return releaseTag
}

export function readReleaseVersion(root, tag) {
  const read = path => readFileSync(resolve(root, path), 'utf8')
  return validateReleaseVersions({
    workspace: read('Cargo.toml'),
    desktop: JSON.parse(read('crates/desktop/tauri.conf.json')),
    frontend: JSON.parse(read('web/package.json')),
    lockfile: read('Cargo.lock'),
    tag,
  })
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    console.log(readReleaseVersion(resolve(fileURLToPath(new URL('../..', import.meta.url))), process.env.RELEASE_TAG_INPUT ?? ''))
  } catch (error) {
    console.error(error.message)
    process.exitCode = 1
  }
}
