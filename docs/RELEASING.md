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

4. Rehearse the install from the local tarballs, which exercises the launcher's optional-dependency
   resolution without touching the registry:

   ```bash
   work_dir="$(mktemp -d)"
   npm install --prefix "$work_dir" --ignore-scripts \
     dist/npm/clooterminal-linux-x64-0.0.4.tgz dist/npm/clooterminal-0.0.4.tgz
   "$work_dir/node_modules/.bin/cloo" --version
   rm -rf "$work_dir"
   ```

### Compatibility floor

The release binary links glibc dynamically, so the build host sets the floor for every user. Check
it before publishing:

```bash
objdump -T target/x86_64-unknown-linux-gnu/release/cloo | grep -oE 'GLIBC_[0-9.]+' | sort -uV | tail -1
```

Weak symbols such as `pidfd_spawnp` resolve to null on older systems and do not raise the floor;
the highest *strong* requirement does. The 0.0.4 build requires glibc 2.34, which covers Ubuntu
22.04, Debian 12, RHEL 9, and later. Building on a newer distribution silently raises that floor
and breaks installs that previously worked, so any change here belongs in both READMEs.

## Publish from the maintainer terminal

Authenticate the maintainer's terminal with npm. Do not print, commit, or persist a token in this
repository.

```bash
npm login
npm whoami

npm publish dist/npm/clooterminal-linux-x64-0.0.4.tgz --access public
npm publish dist/npm/clooterminal-0.0.4.tgz --access public
```

Publish the native package first. Do not rerun a successful `npm publish`: a published version
cannot be overwritten. If the native package succeeds and the launcher fails, resolve the launcher
issue and publish only that missing tarball. A valid npm PAT may be used instead of interactive
login for authenticated reads and publishing, but it needs permission to publish both packages and
does not bypass an account's publish-OTP policy.

If the npm account requires two-factor authentication for writes, run the same two commands from
the maintainer terminal with a current authenticator code, for example
`npm publish <tarball> --access public --otp=<code>`. Do not send a one-time code to an agent or
commit it anywhere.

## Verify the public install and tag the source

```bash
npm view clooterminal version dist-tags --json
npm view clooterminal-linux-x64@0.0.4 version

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
npm install --prefix "$work_dir" --ignore-scripts clooterminal@0.0.4
"$work_dir/node_modules/.bin/cloo" --version

git tag -a v0.0.4 -m "cloo 0.0.4"
git push origin main v0.0.4
```

The installed command must report `cloo 0.0.4` and require no build, download, or install hook.
Record the tag, commit hash, npm versions, and install result in the release record. A public npm
release does not make the product semantically 1.0.0.
