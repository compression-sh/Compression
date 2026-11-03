# Releasing

Compression is a fork of [Ski](https://github.com/ilyamiro/Ski) by ilyamiro, developed further independently. The project lives at <https://github.com/compression-sh/Compression>; releases are cut here.

Pushing a tag `vMAJOR.MINOR.PATCH` runs `.github/workflows/release.yml`, which builds Compression and publishes a GitHub release with the `.tar.gz`, `.deb`, `.rpm` and `SHA256SUMS`. The same tag also makes `docker.yml` push the container image. Nothing is built or uploaded by hand. A patch release takes about 15 minutes in total, most of it waiting for builds.

## Before you start

- **Authentication.** `origin` is `https://github.com/compression-sh/Compression.git`, and pushes authenticate through the GitHub CLI. Check with `gh auth status`. If git asks for a password, run `gh auth setup-git`.
- **Pushing `master` publishes the Docker image.** `docker.yml` runs on every push to `master` and on every `v*` tag, and it pushes `ghcr.io/compression-sh/compression:latest`. Only push `master` when `latest` is meant to move.
- **Build tools.** On NixOS run cargo inside the Nix shell (`nix-shell --run "cargo …"`); outside it the binary has no system libraries and fails with `NoWaylandLib`. On other distributions plain `cargo` is enough, with the graphics libraries from the README installed.
- **Use timeouts.** Give long commands a `timeout`, so a stuck build fails instead of hanging. A clean release build takes about 5 minutes and an incremental one 1–2 minutes. Never wait for a build with `pgrep -f "cargo build"`: the waiting shell's own command line matches the pattern, so the loop waits forever.
- **Merge pull requests first**, for example translations, with `gh pr merge <n> --squash`. Then `git pull`.
- **No AI attribution.** Do not add a `Co-Authored-By` line to release or other commits.

## 1. Prepare and check

1. Start from a clean, up-to-date `master`. `git pull`, then `git status -sb` must show `## master...origin/master` with no `[ahead]` or `[behind]` unless you are about to push the release commit. Release only what is committed.
2. Check the version in `Cargo.toml` and bring `Cargo.lock` in line:
   ```sh
   cargo update -p compression --offline
   git diff Cargo.lock   # only the version line of the `compression` entry may change
   ```
   Both files must agree: the Nix source build reads the version from `Cargo.toml` and looks for the same version in `Cargo.lock`.
3. Build and lint exactly as CI does:
   ```sh
   cargo build --release --locked
   cargo clippy --locked --all-targets --all-features -- -D warnings
   ```
   Clippy must report no warnings. CI (`.github/workflows/ci.yml`) runs clippy (`-D warnings`) on pull requests and on pushes to branches; it builds nothing for release and publishes nothing.
4. Smoke-test the release binary for a few seconds with an isolated environment. **Never use real user data**: a home of its own, `XDG_*` folders of their own and a separate `XDG_RUNTIME_DIR`.
   ```sh
   S=$(mktemp -d); mkdir -p $S/home $S/run; chmod 700 $S/run
   env -i PATH="$PATH" HOME=$S/home XDG_CONFIG_HOME=$S/home/.config \
     XDG_DATA_HOME=$S/home/.local/share XDG_STATE_HOME=$S/home/.local/state \
     XDG_CACHE_HOME=$S/home/.cache XDG_RUNTIME_DIR=$S/run \
     WAYLAND_DISPLAY=wayland-1 LANG=vi_VN.UTF-8 timeout 8 ./target/release/compression $S/home
   ```
   A window needs a display. Put a symlink to the Wayland socket into the separate runtime folder (`ln -s /run/user/$UID/wayland-1 $S/run/`), so the run does not talk to an instance of your own. Exit code 124 (stopped by `timeout`) with no output is a pass. A message like `the vi translation is broken` means that JSON file doesn't parse. Without a display, `winit` fails at the display connection; that is expected. Afterwards stop anything the run started and delete `$S`.

## 2. Release

1. Pick the version (semver; while below 1.0, a new feature bumps the minor, a fix the patch).
2. Set it in `Cargo.toml` and update `Cargo.lock` as in step 1.2, build once more with `cargo build --release --locked`, and commit with a Conventional Commit:
   ```text
   chore(release): bump version to X.Y.Z
   ```
3. Optional dry run. It builds and packages but publishes nothing (the tag check, the release upload and the Nix commit only run for tags). Run it from a branch, not from `master`, so `docker.yml` is not triggered by the push:
   ```sh
   git switch -c release-dry-run-vX.Y.Z
   git push -u origin release-dry-run-vX.Y.Z
   gh workflow run release.yml --repo compression-sh/Compression --ref release-dry-run-vX.Y.Z
   gh run watch --repo compression-sh/Compression <id> --exit-status
   ```
   The dry run uploads no artifacts, so it only proves that the build and the packaging commands succeed. Delete the branch afterwards (`git push origin --delete release-dry-run-vX.Y.Z`).
4. Push `master` (this also runs `docker.yml` and updates `latest`), then tag and push the tag:
   ```sh
   git push origin master
   git tag -a v0.5.0 -m "Compression 0.5.0"
   git push origin v0.5.0
   ```
   Use your own version. The tag message has the form `Compression X.Y.Z`.
5. Wait for the run (about 7 minutes):
   ```sh
   gh run list --repo compression-sh/Compression --workflow release.yml --limit 1
   gh run watch --repo compression-sh/Compression <id> --exit-status
   ```
   The warning that Node.js 20 is deprecated is harmless.
6. Confirm the release has four assets:
   ```sh
   gh release view v0.5.0 --repo compression-sh/Compression --json assets -q '.assets[].name'
   ```
   They are `compression-0.5.0-x86_64-linux.tar.gz`, `compression_0.5.0_amd64.deb`, `compression-0.5.0-1.x86_64.rpm` and `SHA256SUMS`. The generated notes list the commits since the last tag; replace them with the prepared notes once the release exists:
   ```sh
   gh release edit v0.5.0 --repo compression-sh/Compression --notes-file release-notes-v0.5.0.md
   ```
   Keep the notes file outside the repository.
7. The run's last step commits `nix/release.json` (the new version and the tarball's hash) to `master` as `github-actions[bot]`. Bring it in with `git pull --ff-only` before the next commit.

Never edit or re-upload a release's tarball after that commit: its hash would no longer match and the Nix install would fail. To fix it by hand, `nix-prefetch-url <tarball url>` and put `sha256-` plus the base64 of that hash (`nix-hash --type sha256 --to-sri <hash>`) into `nix/release.json`. The hash in `SHA256SUMS` is the same digest in hex.

## 3. Docker image

- The tag runs `docker.yml` in addition to `release.yml`. It builds the `Dockerfile` and pushes `ghcr.io/compression-sh/compression:0.5.0` (the semver tag) and moves `latest`. A push to `master` or a manual run of the workflow on `master` moves only `latest`.
- The package must be **public**, so that users can pull it without logging in. The repository owner is a user account, so check the package settings at <https://github.com/compression-sh?tab=packages>. To test from outside: ask `https://ghcr.io/v2/compression-sh/compression/tags/list` with an anonymous pull token; it must answer with the tags `latest` and `0.5.0`.
- If the push fails with `denied: permission_denied: read_package`, the workflow token has no write access to the package. Open the package's settings, choose "Manage Actions access" and add the repository Compression with the role Write. Then rerun the workflow.
- Users run the image with `docker/compression-docker`; see the README.

## 4. After the release

- Wait until `release.yml` and `docker.yml` have both succeeded (`gh run list --repo compression-sh/Compression --limit 4`).
- Check the release: `gh release view v0.5.0 --repo compression-sh/Compression`, with the four assets above.
- Check the Docker tags `ghcr.io/compression-sh/compression:0.5.0` and `:latest`.
- Check the Nix follow-up commit on `master`: it changes only `nix/release.json`, to the new version and a hash that equals the tarball's entry in `SHA256SUMS`.
- When the commit has landed: `git pull --ff-only`.

## 5. NixOS

- `nix/bin.nix` installs the release binary `compression-<version>-x86_64-linux.tar.gz` from the GitHub release named in `nix/release.json`. The release workflow updates `nix/release.json` itself; do not edit it by hand for a normal release.
- Until the follow-up commit is on `master`, `release.json` still names the previous version, which may not exist as a release. Until then, install from the source build: `pkgs.callPackage /path/to/Compression { fromSource = true; }`, or `programs.compression.package` set to it.
- The option names from before the rename keep working through an alias in `nix/module.nix`.
- Compression is not in nixpkgs, so there is no nixpkgs package or pull request to update.

## Things that go wrong

- The workflow fails at "Check the tag matches Cargo.toml": the tag and `version` differ. Fix the version, commit, then move the tag (`git tag -d`, `git push origin :refs/tags/vX.Y.Z`, tag again), as long as the release was not published yet.
- Never reuse a tag for a release that has already been published; cut a new patch version.
- New files that the packages must contain go into `packaging/stage.sh` and then into both `[package.metadata.deb]` and `[package.metadata.generate-rpm]` in `Cargo.toml` (the `target/stage/...` sources must match what `stage.sh` writes). A new run-time library goes into the `depends` of the deb and the `requires` of the rpm, since neither is detected automatically. For the Nix packages (source build and release binary share `nix/runtime.nix`), a library Compression loads at run time goes into the `libs` list there, which ends up on `LD_LIBRARY_PATH`, and a program it runs goes into the `tools` list, which ends up on `PATH`.
- Translation files are built into the binary (`include_str!`), so a new language needs no packaging change. It needs its `src/i18n/<code>.json` and its arms in the `Language` matches in `src/i18n/mod.rs`.
- Builds are x86_64 only, on `ubuntu-22.04` so that the binary runs on older glibc. Do not move the runner to a newer image without a reason.
- `docker.yml` runs on `ubuntu-latest`, which GitHub is moving to a newer Ubuntu; that is harmless for the image.
- Workflows of a fork may not be registered until they have been pushed once: `gh workflow list` shows nothing and `gh workflow run` answers 404. Push a branch that contains them and check `gh workflow list` again.
- `git push` asks for a password: run `gh auth setup-git`.

## Limits of the local checks

Docker, `cargo-deb`, `cargo-generate-rpm` and Nix are not always installed where you release from. Without them the Docker build, the `.deb` and `.rpm` and the Nix build are not tested locally. The release workflow is then the binding check for the packaging and the artifacts: it builds with `--locked`, installs `cargo-deb` and `cargo-generate-rpm`, runs `packaging/stage.sh` and creates the `.tar.gz`, `.deb`, `.rpm` and `SHA256SUMS`. Before the first install on a real machine, treat the packages as untested.

## Checklist

- [ ] Version in `Cargo.toml` updated
- [ ] `Cargo.lock` updated
- [ ] Build and clippy green
- [ ] Smoke test in an isolated environment
- [ ] Tag created
- [ ] Tag pushed
- [ ] `release.yml` successful
- [ ] `docker.yml` successful
- [ ] Release artifacts present (`.tar.gz`, `.deb`, `.rpm`, `SHA256SUMS`)
- [ ] Docker tags present (`<version>` and `latest`) and the package is public
- [ ] `nix/release.json` updated by the workflow
- [ ] `git pull --ff-only` after the Nix commit
