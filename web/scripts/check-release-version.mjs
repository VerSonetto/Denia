import assert from 'node:assert/strict'
import { readReleaseVersion, validateReleaseVersions } from '../../.github/scripts/release-version.mjs'
import { fileURLToPath } from 'node:url'

const versions = {
  workspace: '[workspace]\nresolver = "2"\n[workspace.package]\nversion = "0.4.2-beta"\nedition = "2024"\n[workspace.dependencies]\n',
  desktop: { version: '0.4.2-beta' },
  frontend: { version: '0.4.2-beta' },
  lockfile: 'version = 4\n[[package]]\nname = "denia-server"\nversion = "0.4.2-beta"\n[[package]]\nname = "denia-desktop"\nversion = "0.4.2-beta"\n[[package]]\nname = "other"\nversion = "1.0.0"\nsource = "registry+https://example.test"\n',
}
assert.equal(validateReleaseVersions(versions), 'v0.4.2-beta')
assert.equal(validateReleaseVersions({ ...versions, tag: ' v0.4.2-beta ' }), 'v0.4.2-beta')
assert.throws(() => validateReleaseVersions({ ...versions, tag: 'v0.4.3-beta' }), /must match the local version/)
assert.throws(() => validateReleaseVersions({ ...versions, desktop: { version: '0.4.1-beta' } }), /desktop version/)
assert.throws(() => validateReleaseVersions({ ...versions, frontend: { version: '0.4.1-beta' } }), /frontend version/)
assert.throws(() => validateReleaseVersions({ ...versions, lockfile: versions.lockfile.replace('version = "0.4.2-beta"', 'version = "0.4.1-beta"') }), /Cargo.lock/)
assert.throws(() => validateReleaseVersions({ ...versions, lockfile: '' }), /no local Denia packages/)
assert.throws(() => validateReleaseVersions({ ...versions, workspace: versions.workspace.replace('0.4.2-beta', '0.4.2beta') }), /valid semantic version/)
assert.throws(() => validateReleaseVersions({ ...versions, workspace: versions.workspace.replace('0.4.2-beta', '0.4.2-01') }), /valid semantic version/)
const root = fileURLToPath(new URL('../..', import.meta.url))
assert.match(readReleaseVersion(root, ''), /^v\d+\.\d+\.\d+/)
console.log('Release version guard: local package versions, lockfile and tag consistency checked.')
