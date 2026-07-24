# Releasing cloo

This is the maintainer runbook for publishing the official npm distribution. It is deliberately
manual: a registry publish is public and irreversible, and the CI workflow builds and archives
artifacts but never receives an npm credential.

## Current release line

The checked-in release version is **0.0.3**. It is shared by the Rust workspace, the
`clooterminal` launcher, and all four native optional-dependency packages. The current commit has
no release tag.

Release this exact line as `v0.0.3`; do not call it `v1.0.0`. A 1.0.0 release requires a deliberate
product decision and a coordinated version update before the tag is made. The workboard is named
for the v1 implementation plan, which is separate from the published semantic version.

The official npm package is [`clooterminal`](https://www.npmjs.com/package/clooterminal), because
npm rejects the package name `cloo` under its similarity policy. Installing it still provides the
`cloo` command.

## Preconditions

- You are the npm package owner and have an npm token with permission to publish all five public
  packages.
- Two-factor authentication, if required by the npm account or package policy, is available.
- The checkout is clean and at the commit intended for release.
- The full fast verification suite passes:

  ```bash
  cargo fmt --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  ```

Never print, commit, or persist `NPM_TOKEN`. The ignored repository-root `.env` may contain only
that token; see [ENV_VARS.md](ENV_VARS.md).

## Build the immutable artifacts

1. Confirm every npm manifest has the same version and that it is not already published:

   ```bash
   git status --short
   git rev-parse --verify HEAD
   node -p "require('./npm/package.json').version"
   for manifest in npm/packages/*/package.json; do
     node -p "require('./' + process.argv[1]).name + '@' + require('./' + process.argv[1]).version" "$manifest"
   done
   npm view clooterminal@0.0.3 version
   ```

   The final command should report a not-found error before this new version is released. Replace
   `0.0.3` only after making the coordinated version update described above.

2. Create and push the matching annotated tag. The tag triggers the artifact workflow for all four
   supported native targets:

   ```bash
   git tag -a v0.0.3 -m "cloo 0.0.3"
   git push origin v0.0.3
   ```

3. Wait for the `Release artifacts` workflow for that tag to pass. Download its five artifacts into
   an empty, ignored `dist/npm/` directory. It must contain exactly one tarball for each package:

   ```text
   clooterminal-darwin-arm64-0.0.3.tgz
   clooterminal-darwin-x64-0.0.3.tgz
   clooterminal-linux-arm64-0.0.3.tgz
   clooterminal-linux-x64-0.0.3.tgz
   clooterminal-0.0.3.tgz
   ```

   The root launcher artifact comes from the `npm package` job; each native artifact comes from
   its matching native job. Do not substitute a binary built on a different target.

4. Inspect every tarball before publishing it:

   ```bash
   for package in dist/npm/*.tgz; do
     npm publish --dry-run "$package"
   done
   ```

   The native packages contain only their target's executable, product-mark asset, README, and
   manifest. The launcher contains its JavaScript resolver, README, product-mark asset, and
   manifest—never a fallback downloader or install hook.

## Publish in dependency order

Native packages must be publicly available before the launcher is published, because the launcher
declares exact-version optional dependencies on them.

Load the owner-provided token only into this shell and place it in a temporary npm configuration:

```bash
set -a
. ./.env
set +a
test -n "${NPM_TOKEN:-}"

release_npmrc="$(mktemp)"
trap 'rm -f "$release_npmrc"' EXIT
umask 077
printf '//registry.npmjs.org/:_authToken=%s\n' "$NPM_TOKEN" > "$release_npmrc"
npm --userconfig="$release_npmrc" whoami
```

Publish the four native packages first, then the launcher:

```bash
for package in \
  dist/npm/clooterminal-darwin-arm64-0.0.3.tgz \
  dist/npm/clooterminal-darwin-x64-0.0.3.tgz \
  dist/npm/clooterminal-linux-arm64-0.0.3.tgz \
  dist/npm/clooterminal-linux-x64-0.0.3.tgz
do
  npm --userconfig="$release_npmrc" publish "$package" --access public
done

npm --userconfig="$release_npmrc" publish dist/npm/clooterminal-0.0.3.tgz --access public
```

Do not rerun a successful `npm publish`: versions cannot be overwritten. If publishing stops after
some native packages succeed, inspect each package's registry version, publish only the missing
ones, and publish the launcher last.

## Verify the public install

Confirm every package and the `latest` tag refer to the same release, then perform a fresh install
on at least one supported platform:

```bash
npm view clooterminal version dist-tags --json
npm view clooterminal-darwin-arm64@0.0.3 version
npm view clooterminal-darwin-x64@0.0.3 version
npm view clooterminal-linux-arm64@0.0.3 version
npm view clooterminal-linux-x64@0.0.3 version

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir" "$release_npmrc"' EXIT
npm install --prefix "$work_dir" --ignore-scripts clooterminal@0.0.3
"$work_dir/node_modules/.bin/cloo" --version
```

The final command must report `cloo 0.0.3`. The installed launcher must select the native package
for the host without download, post-install, or build steps.

Record the tag, commit hash, npm package versions, workflow URL, and install result in the release
record. A public npm release does not automatically make the product semantically 1.0.0.
