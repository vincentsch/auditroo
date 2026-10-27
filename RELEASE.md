# Releasing auditroo

A release is a Git tag such as `v0.1.0`. Pushing the tag starts
`.github/workflows/release.yml`. The workflow runs CI and builds the
binaries. Then it publishes a GitHub release with the binaries and
`checksums.txt`, plus build provenance attestations.

## Binaries

| Asset | Built on | Notes |
| --- | --- | --- |
| `auditroo-linux-amd64` | ubuntu-latest | static musl build with `cargo zigbuild` |
| `auditroo-linux-arm64` | ubuntu-latest | static musl build with `cargo zigbuild` |
| `auditroo-darwin-amd64` | macos-14 | cross-compiled on Apple silicon |
| `auditroo-darwin-arm64` | macos-14 | |
| `auditroo-windows-amd64.exe` | windows-2022 | |
| `auditroo-windows-arm64.exe` | windows-11-arm | |

The workflow runs `--version` and `demo` with each binary that can run on
its build machine. The Linux arm64 and macOS Intel binaries are only built
there, so test them by hand when you can.

`install.sh` and the GitHub Action download these exact file names. Do not
rename them without changing both.

## Steps

1. Pick the version. auditroo follows semantic versioning. Before 1.0, a
   minor version may break commands or the report format.
2. Set `version` in `Cargo.toml` and run `cargo build` so that `Cargo.lock`
   follows. Set the default of the `version` input in `action.yml` to the
   same tag, for example `default: v0.2.0`, so that pinning the action also
   pins the binary. A test fails while the two differ.
3. In `CHANGELOG.md`, rename `## Unreleased` to `## 0.2.0 (2026-11-02)` with
   the real version and date. Add a new empty `## Unreleased` above it.
   Check the notes with `scripts/release-notes.sh 0.2.0`.
4. Update the version in the examples that pin one. Search for the old tag:

   ```sh
   git grep -n 'v0\.1\.0' -- README.md AGENTS.md docs
   ```

5. Run the checks:

   ```sh
   make ci
   make browser
   make docs
   ```

6. Commit with a message such as "Release 0.2.0" and push to `main`. Wait
   for CI to pass.
7. Do a build without publishing. In the Actions tab, run the Release
   workflow by hand on `main`. It builds all six binaries and uploads them
   as workflow artifacts. Nothing is published. Download one or two and run
   `auditroo demo`.
8. Test the installer against those files. Put them in a local folder with
   the same layout as a release. Then serve the folder and point the
   installer at it:

   ```sh
   mkdir -p fake/releases/download/v0.2.0
   cp auditroo-linux-amd64 fake/releases/download/v0.2.0/
   (cd fake/releases/download/v0.2.0 && sha256sum auditroo-* > checksums.txt)
   (cd fake && python3 -m http.server 8765 --bind 127.0.0.1) &
   AUDITROO_DOWNLOAD_BASE=http://127.0.0.1:8765 AUDITROO_VERSION=v0.2.0 \
     INSTALL_DIR=/tmp/auditroo-test sh install.sh
   /tmp/auditroo-test/auditroo --version
   ```

9. Tag and push:

   ```sh
   git tag -a v0.2.0 -m "auditroo 0.2.0"
   git push origin v0.2.0
   ```

   The workflow stops when the tag does not match `version` in
   `Cargo.toml`, or when `CHANGELOG.md` has no section for the version.

## Before the first public release

Some things the README and the docs describe only exist once the repository
is public and the first release is out. Check each of them then:

- The repository is public, so the installer URL
  `https://raw.githubusercontent.com/vincentsch/auditroo/main/install.sh`
  and `cargo install --git https://github.com/vincentsch/auditroo` work
  without access rights.
- `releases/latest` has all six binaries and `checksums.txt`, and the
  installer picks the right binary on every system it supports.
- The tag `v0.1.0` exists, so `uses: vincentsch/auditroo@v0.1.0` in the
  docs resolves, and the action installs auditroo 0.1.0 without a
  `version` input.
- The CI matrix has passed on Windows. GitHub Actions was off while the
  repository was private, so the Windows job and the action self-test have
  not run there yet.
- The Linux arm64 and macOS Intel binaries ran `auditroo demo` on real
  machines. CI builds them but cannot run them.
- The Vercel `deployment_status` recipe in `docs/ci.md` ran against a real
  Vercel project.

## After the release

- Install it with the one-line installer from the README and run
  `auditroo --version`.
- Verify the provenance of a binary:

  ```sh
  gh attestation verify auditroo-linux-amd64 --repo vincentsch/auditroo
  ```

- Run the GitHub Action with the new tag in a test repository, for example
  with `args: demo`.

## When something goes wrong

If the workflow fails before the publish job, fix the problem. Then delete
the tag locally and on GitHub, and tag again. Once a release is published, do
not replace its files. Publish a new patch version instead, so that
checksums people already saw stay valid.
