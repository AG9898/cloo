# Releasing cloo

This is the maintainer runbook for directly publishing the official npm distribution from a
Linux x64 terminal. Registry publishing is public and irreversible; it never runs in GitHub
Actions.

## Current release line

The checked-in release version is **0.0.4**. It is shared by the Rust workspace, the
`clooterminal` launcher, and the `clooterminal-linux-x64` native package. The current npm
distribution supports Linux x64 only.

Release this exact line as `v0.0.4`; do not call it `v1.0.0`. The workboard's v1 implementation
plan is separate from the published semantic version.

The official npm package is [`clooterminal`](https://www.npmjs.com/package/clooterminal), because
npm rejects `cloo` under its similarity policy. Installing it provides the `cloo` command.

## Verify and build

1. Confirm the checkout is clean, the package versions agree, and this version is unpublished:

   ```bash
   git status --short
   node -p "require('./npm/package.json').version"
   node -p "require('./npm/packages/linux-x64/package.json').name + '@' + require('./npm/packages/linux-x64/package.json').version"
   npm view clooterminal@0.0.4 version
   ```

   The final command must report a not-found error before release.

2. Run the fast verification suite:

   ```bash
   cargo fmt --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```

3. Build and pack both tarballs from the clean checkout:

   ```bash
   cargo build --locked --release --package cloo --target x86_64-unknown-linux-gnu
   scripts/package-npm.sh x86_64-unknown-linux-gnu
   mkdir -p dist/npm
   (cd npm && npm pack --ignore-scripts --pack-destination ../dist/npm)
   npm publish --dry-run dist/npm/clooterminal-linux-x64-0.0.4.tgz
   npm publish --dry-run dist/npm/clooterminal-0.0.4.tgz
   ```

   Inspect the tarballs before publishing. The native package contains only the executable,
   manifest, README, and product-mark asset; the launcher contains its resolver, manifest, README,
   and product-mark asset. Neither package may include an install hook or downloader.

## Publish from the maintainer terminal

The ignored repository-root `.env` may contain only `NPM_TOKEN`. Load it only into the current
shell and configure npm with a temporary file; never print, commit, or persist the token.

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

npm --userconfig="$release_npmrc" publish dist/npm/clooterminal-linux-x64-0.0.4.tgz --access public
npm --userconfig="$release_npmrc" publish dist/npm/clooterminal-0.0.4.tgz --access public
```

Publish the native package first. Do not rerun a successful `npm publish`: a published version
cannot be overwritten. If the native package succeeds and the launcher fails, resolve the launcher
issue and publish only that missing tarball.

## Verify the public install and tag the source

```bash
npm view clooterminal version dist-tags --json
npm view clooterminal-linux-x64@0.0.4 version

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir" "$release_npmrc"' EXIT
npm install --prefix "$work_dir" --ignore-scripts clooterminal@0.0.4
"$work_dir/node_modules/.bin/cloo" --version

git tag -a v0.0.4 -m "cloo 0.0.4"
git push origin main v0.0.4
```

The installed command must report `cloo 0.0.4` and require no build, download, or install hook.
Record the tag, commit hash, npm versions, and install result in the release record. A public npm
release does not make the product semantically 1.0.0.
