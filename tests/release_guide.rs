//! The release guide (`docs/releasing.md`) as text, against the workflows it describes. Like `docker_workflow.rs` this
//! keeps what the guide says from drifting away from what `ci.yml` and `release.yml` do; it does not run anything.
use std::path::Path;

fn read(relative: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)).unwrap_or_else(|e| panic!("{relative}: {e}"))
}

fn guide() -> String {
    read("docs/releasing.md")
}

#[test]
fn the_guide_says_that_ci_runs_the_tests_and_ci_does() {
    let (guide, ci) = (guide(), read(".github/workflows/ci.yml"));
    assert!(ci.contains("cargo test --locked"), "ci.yml no longer runs the tests: change the guide with it");
    assert!(guide.contains("runs Clippy (`-D warnings`) and `cargo test --locked`"), "the guide does not say what CI runs");
    assert!(guide.contains("cargo test --locked   # with an isolated home"), "the test command is missing from step 1.3");
}

#[test]
fn the_guide_describes_the_smoke_step_that_runs_before_the_upload() {
    let (guide, release) = (guide(), read(".github/workflows/release.yml"));
    let smoke = release.find("Smoke-test the packages").expect("release.yml has no smoke step");
    let upload = release.find("softprops/action-gh-release").expect("release.yml has no upload step");
    assert!(smoke < upload, "the smoke step must come before the upload, as the guide says");
    assert!(release.contains("packaging/smoke.sh"));
    for part in ["**\"Smoke-test the packages\"**", "`packaging/smoke.sh`", "Ubuntu 22.04", "Fedora 43", "with Nautilus installed first", "If this step fails, no release is published", "\"Smoke-test the packages\" fails"] {
        assert!(guide.contains(part), "the guide does not say: {part}");
    }
    let script = read("packaging/smoke.sh");
    assert!(script.contains("ubuntu:22.04") && script.contains("fedora:43"), "smoke.sh uses other images than the guide names");
}

#[test]
fn the_guide_gives_the_retries_of_the_nix_step_as_the_workflow_has_them() {
    let (guide, release) = (guide(), read(".github/workflows/release.yml"));
    assert!(release.contains("for attempt in 1 2 3 4 5"), "the number of tries changed: change the guide");
    assert!(release.contains("sleep $(((attempt - 1) * 15))"), "the waits changed: change the guide");
    assert!(guide.contains("tries up to five times, waiting 15, 30, 45 and 60 seconds"));
    assert!(guide.contains("**Do not start the run of a release again to fix that.**"));
    assert!(guide.contains("the run of an **older** tag would write that old version into `nix/release.json` on `master`"));
}

#[test]
fn the_guide_explains_the_old_tags_and_the_release_pages_that_were_added_later() {
    let guide = guide();
    for part in [
        "## History of the tags and release pages",
        "The tags `v0.1.0` to `v0.4.5` are the releases of [Ski](https://github.com/ilyamiro/Ski) by ilyamiro",
        "`v0.4.6`, `v0.4.7` and `v0.4.8` point to commits that were never merged here",
        "The release pages of `v0.5.0` to `v0.5.6` were added later, without files",
        "Keep a copy of the notes of every release outside GitHub",
    ] {
        assert!(guide.contains(part), "the guide does not say: {part}");
    }
}

#[test]
fn the_checklist_has_the_steps_that_were_missing() {
    let guide = guide();
    for item in [
        "- [ ] Build, clippy and tests green",
        "- [ ] `release.yml` successful, including \"Smoke-test the packages\"",
        "- [ ] Release notes set (`gh release edit --notes-file`), and a copy kept outside GitHub",
    ] {
        assert!(guide.contains(item), "the checklist lacks: {item}");
    }
}

#[test]
fn nix_compiles_the_checkout_unless_release_json_names_its_very_version() {
    // Nix is not run here: `default.nix` is read for the one decision the Nix follow-up of `release.yml` relies on
    let default = read("default.nix");
    // No release.json, or one without a version, is no release
    assert!(default.contains("builtins.pathExists releaseFile"), "a missing release.json must not break the build");
    assert!(default.contains(".version or null"), "a release.json without a version must count as none");
    // The binary only for the version of this checkout, on x86_64 Linux and when not asked to compile
    assert!(default.contains("if fromSource || pkgs.stdenv.hostPlatform.system != \"x86_64-linux\" || releaseVersion != checkoutVersion"));
    assert!(default.contains("then import ./nix/package.nix"), "otherwise the checkout is compiled");
    assert!(default.contains("else import ./nix/bin.nix"));
    // And the release workflow is what writes release.json after a tag
    assert!(read(".github/workflows/release.yml").contains("nix/release.json"));
}
