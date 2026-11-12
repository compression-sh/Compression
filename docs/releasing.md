# Releasing

Compression is a fork of [Ski](https://github.com/ilyamiro/Ski) by ilyamiro, developed further independently. The project lives at <https://github.com/compression-sh/Compression>; releases are cut here.

Pushing a tag `vMAJOR.MINOR.PATCH` runs `.github/workflows/release.yml`, which builds Compression and publishes a GitHub release with the `.tar.gz`, `.deb`, `.rpm` and `SHA256SUMS`. The same tag also makes `docker.yml` push the container image. Nothing is built or uploaded by hand. A patch release takes about 15 minutes in total, most of it waiting for builds.

## Before you start

- **Authentication.** `origin` is `https://github.com/compression-sh/Compression.git`, and pushes authenticate through the GitHub CLI. Check with `gh auth status`. If git asks for a password, run `gh auth setup-git`.
- **Pushing `master` publishes the `edge` Docker image, never `latest`.** `docker.yml` runs on every push to `master` and on every `v*` tag. A push to `master` moves only `ghcr.io/compression-sh/compression:edge`; only a release tag publishes `:<version>`, and only the highest release tag moves `:latest` (a pre-release tag such as `v1.2.3-rc.1`, or a tag for an older line such as `v0.5.10` after `v0.6.0`, publishes its version and leaves `latest` alone). So `latest` is always the highest release, and `edge` is whatever `master` is.
- **Build tools.** On NixOS run cargo inside the Nix shell (`nix-shell --run "cargo …"`); outside it the binary has no system libraries and fails with `NoWaylandLib`. On other distributions plain `cargo` is enough, with the graphics libraries installed that the [Installation page of the wiki](https://github.com/compression-sh/Compression/wiki/Installation) lists.
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
   cargo test --locked   # with an isolated home and no display, as in step 4
   ```
   Clippy must report no warnings, and the tests must pass. CI (`.github/workflows/ci.yml`) runs Clippy (`-D warnings`) and `cargo test --locked` (whatever tests the repository holds, in folders of their own and without a display) on pull requests and on pushes to branches; it builds nothing for release and publishes nothing.
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
4. Push `master` (this also runs `docker.yml` and updates `edge`, not `latest`), then tag and push the tag:
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

   The run builds the binary, makes the packages (`packaging/stage.sh`, `cargo deb`, `cargo generate-rpm`) and then runs **"Smoke-test the packages"** (`packaging/smoke.sh`), before anything is uploaded: it installs the `.deb` in a clean Ubuntu 22.04 container and the `.rpm` in a clean Fedora 43 container, each with Nautilus installed first, and checks that `compression --version` prints the version, that the package has its own D-Bus service file (`org.compression.Compression.FileManager1.service`, not Nautilus's) and that the desktop file is there. If this step fails, no release is published. The usual causes are two packages claiming one file, a dependency that cannot be met, and a version that differs from `Cargo.toml`; fix it, then move the tag as described under "Things that go wrong".
6. Confirm the release has four assets:
   ```sh
   gh release view v0.5.0 --repo compression-sh/Compression --json assets -q '.assets[].name'
   ```
   They are `compression-0.5.0-x86_64-linux.tar.gz`, `compression_0.5.0_amd64.deb`, `compression-0.5.0-1.x86_64.rpm` and `SHA256SUMS`. The generated notes list the commits since the last tag; replace them with the prepared notes once the release exists:
   ```sh
   gh release edit v0.5.0 --repo compression-sh/Compression --notes-file release-notes-v0.5.0.md
   ```
   Keep the notes file outside the repository.
7. The run's last step commits `nix/release.json` (the new version and the tarball's hash) to `master` as `github-actions[bot]`. Bring it in with `git pull --ff-only` before the next commit. The step tries up to five times, waiting 15, 30, 45 and 60 seconds, when GitHub answers with a temporary error. If it still fails, the release itself is published and fine, but `nix/release.json` names the previous version: set it by hand as described below (`default.nix` compiles the checkout until then).

   **Do not start the run of a release again to fix that.** A new run builds and uploads the files again, so the tarball is replaced, and the hash in `nix/release.json` has to be the one of the new tarball; and the run of an **older** tag would write that old version into `nix/release.json` on `master`. Fix `release.json` by hand instead.

Never edit or re-upload a release's tarball after that commit: its hash would no longer match and the Nix install would fail. To fix it by hand, `nix-prefetch-url <tarball url>` and put `sha256-` plus the base64 of that hash (`nix-hash --type sha256 --to-sri <hash>`) into `nix/release.json`. The hash in `SHA256SUMS` is the same digest in hex.

## 3. Docker image

- The tag runs `docker.yml` in addition to `release.yml`. It builds the `Dockerfile`, pushes `ghcr.io/compression-sh/compression:0.5.0` (the semver tag) and moves `latest`, but only when the tag is the highest `vX.Y.Z` of the repository (step `newest` of `docker.yml` compares it with all tags and then gives the metadata action `latest=auto` or `latest=false`; a pre-release tag never moves it). A push to `master`, or a manual run of the workflow on `master`, moves only `edge`. A manual run from any other branch builds the image and publishes nothing. `docker.yml` has no rule that sets `latest` for a branch: a change that adds one lets every push to `master` replace the release image with unreleased code.
- The package must be **public**, so that users can pull it without logging in. The repository owner is a user account, so check the package settings at <https://github.com/compression-sh?tab=packages>. To test from outside: ask `https://ghcr.io/v2/compression-sh/compression/tags/list` with an anonymous pull token; it must answer with the tags `latest` and `0.5.0` (and `edge`, once `master` has been pushed).
- If the push fails with `denied: permission_denied: read_package`, the workflow token has no write access to the package. Open the package's settings, choose "Manage Actions access" and add the repository Compression with the role Write. Then rerun the workflow.
- Users run the image with `docker/compression-docker`; see the [Installation page of the wiki](https://github.com/compression-sh/Compression/wiki/Installation).

## 4. After the release

- Wait until `release.yml` and `docker.yml` have both succeeded (`gh run list --repo compression-sh/Compression --limit 4`).
- Check the release: `gh release view v0.5.0 --repo compression-sh/Compression`, with the four assets above.
- Check the Docker tags `ghcr.io/compression-sh/compression:0.5.0` and `:latest`. The tag run's log (step "Docker tags" of the metadata action) lists the tags it computed; for a release tag these are the version and `latest`, and for a push to `master` only `edge`.
- Check the Nix follow-up commit on `master`: it changes only `nix/release.json`, to the new version and a hash that equals the tarball's entry in `SHA256SUMS`.
- When the commit has landed: `git pull --ff-only`.

## 5. NixOS

- `nix/bin.nix` installs the release binary `compression-<version>-x86_64-linux.tar.gz` from the GitHub release named in `nix/release.json`. The release workflow updates `nix/release.json` itself; do not edit it by hand for a normal release.
- Until the follow-up commit is on `master`, `release.json` still names the previous version. `default.nix` notices that it differs from the version in `Cargo.toml` and compiles the checkout instead of installing the old binary, so nothing has to be done by hand; `fromSource = true` still forces a compile at any time.
- The option names from before the rename keep working through an alias in `nix/module.nix`.
- Compression is not in nixpkgs, so there is no nixpkgs package or pull request to update.

## Things that go wrong

- The workflow fails at "Check the tag matches Cargo.toml": the tag and `version` differ. Fix the version, commit, then move the tag (`git tag -d`, `git push origin :refs/tags/vX.Y.Z`, tag again), as long as the release was not published yet.
- Never reuse a tag for a release that has already been published; cut a new patch version.
- "Smoke-test the packages" fails: see step 5 of "2. Release". Nothing was uploaded, so the tag can be moved once the cause is fixed.
- New files that the packages must contain go into `packaging/stage.sh` and then into both `[package.metadata.deb]` and `[package.metadata.generate-rpm]` in `Cargo.toml` (the `target/stage/...` sources must match what `stage.sh` writes). A new run-time library goes into the `depends` of the deb and the `requires` of the rpm, since neither is detected automatically. For the Nix packages (source build and release binary share `nix/runtime.nix`), a library Compression loads at run time goes into the `libs` list there, which ends up on `LD_LIBRARY_PATH`, and a program it runs goes into the `tools` list, which ends up on `PATH`.
- Translation files are built into the binary (`include_str!`), so a new language needs no packaging change. It needs its `src/i18n/<code>.json` and its arms in the `Language` matches in `src/i18n/mod.rs`.
- Builds are x86_64 only, on `ubuntu-22.04` so that the binary runs on older glibc. Do not move the runner to a newer image without a reason.
- `docker.yml` runs on `ubuntu-latest`, which GitHub is moving to a newer Ubuntu; that is harmless for the image.
- Workflows of a fork may not be registered until they have been pushed once: `gh workflow list` shows nothing and `gh workflow run` answers 404. Push a branch that contains them and check `gh workflow list` again.
- `git push` asks for a password: run `gh auth setup-git`.

## History of the tags and release pages

- The tags `v0.1.0` to `v0.4.5` are the releases of [Ski](https://github.com/ilyamiro/Ski) by ilyamiro (annotated tags by ilyamiro). Their commits are part of this repository's history, and the tags stay as credit. The first Compression release is `v0.5.0`, based on Ski 0.4.5.
- Ski's later tags `v0.4.6`, `v0.4.7` and `v0.4.8` point to commits that were never merged here, so they were removed from this repository's tags; they remain at <https://github.com/ilyamiro/Ski>.
- The release pages of `v0.5.0` to `v0.5.6` were added later, without files: `v0.5.0` and `v0.5.1` from the changelog, `v0.5.6` from the text of its tag annotation, and `v0.5.2` to `v0.5.5` as lists of commits, because their notes were not kept. `v0.5.7` has notes but no files. From `v0.5.8` on, a release page has the four files.
- Keep a copy of the notes of every release outside GitHub (the tag annotation, or the notes file kept beside the repository), so that a release page can be made again if it is ever lost. The annotation of `v0.5.6` is the only reason its notes survived.

## Checks with a real server and with Nix

The unit tests use stand-ins (a folder for a mount, a thread that sleeps for a server that hangs). Before a release that touches the server code, check once by hand with a real SSH server you may use, `sshfs` and the Connect to server dialog:

1. **Loss and Reconnect.** Connect `sftp://user@host`, open two tabs in different folders of the server (`/a`, `/b`), keep a third in your home folder. Cut the server off (stop `sshd`, or block it with `iptables -A OUTPUT -d <host> -j DROP`). Within about 20 seconds the dot in the sidebar must turn yellow or red. Restore the server, open the server's menu and choose **Reconnect**. Expected: the tab you were in returns to its folder and every tab that was in the server returns to its own folder; the tab in your home folder stays where it is. Repeat with `/b` renamed on the server meanwhile: that tab shows the top of the server, the others are unaffected.
2. **A server that hangs.** With the server cut off but the mount still there, click the server in the sidebar, open the folder menu with two tabs open (one in the server), and use Connect to server on the same address. The window must stay usable all the time; after 3 seconds (`CHECK_TIMEOUT` in `src/app/network_ui.rs`) a click on the server says "Not answering" and opens nothing. Restore the server: within about 15 seconds the dot turns green again, and a click opens the folder.
3. **A slow server.** Add latency (`tc qdisc add dev <if> root netem delay 800ms`) and click the server: it must open, not say "Not answering". If it does say it on a slow but working link, raise `CHECK_TIMEOUT`.
4. **Renaming on other file systems.** Rename a file and a folder on a GVfs mount (SFTP, SMB), an `sshfs`, a CIFS and an NFS mount, and move one into another folder of the same mount. Where the file system supports `RENAME_NOREPLACE`, the rename works and a taken name answers “already exists” without changing anything. Where it does not, Compression refuses with “This file system does not support moving or renaming without the risk of replacing something, so nothing was changed” and changes nothing: there is no fallback to a plain rename. Check each kind of mount with a test folder (a rename with `renameat2` and `RENAME_NOREPLACE` onto a taken name must answer `EEXIST`), and say in the release notes which kinds were checked. Only SFTP through GVfs has been checked so far.
5. **Nix.** On NixOS or with Nix installed: right after a release, before the follow-up commit, `nix-build -E 'with import <nixpkgs> {}; callPackage ./. {}'` in a checkout of the tag must compile the checkout (`release.json` names the previous version); after the follow-up commit the same call must install the release binary. `fromSource = true` must compile at any time. With `nix/release.json` removed or without a `version`, the checkout is compiled.

## Limits of the local checks

Docker, `cargo-deb`, `cargo-generate-rpm` and Nix are not always installed where you release from. Without them the Docker build, the `.deb` and `.rpm` and the Nix build are not tested locally. The release workflow is then the binding check for the packaging and the artifacts: it builds with `--locked`, installs `cargo-deb` and `cargo-generate-rpm`, runs `packaging/stage.sh` and creates the `.tar.gz`, `.deb`, `.rpm` and `SHA256SUMS`. Before the first install on a real machine, treat the packages as untested.

## Checklist

- [ ] Version in `Cargo.toml` updated
- [ ] `Cargo.lock` updated
- [ ] Build, clippy and tests green
- [ ] Smoke test in an isolated environment
- [ ] Tag created
- [ ] Tag pushed
- [ ] `release.yml` successful, including "Smoke-test the packages"
- [ ] `docker.yml` successful
- [ ] Release artifacts present (`.tar.gz`, `.deb`, `.rpm`, `SHA256SUMS`)
- [ ] Docker tags present (`<version>` and `latest`) and the package is public
- [ ] Release notes set (`gh release edit --notes-file`), and a copy kept outside GitHub
- [ ] `nix/release.json` updated by the workflow
- [ ] `git pull --ff-only` after the Nix commit
