//! The tags the Docker workflow publishes, as text. This does not run the workflow (only GitHub does that: the
//! step "Docker tags" of its log says what it computed); it keeps the rules that decide which tag moves when.
//! `latest` is the last release: only a release tag moves it, and a push to master moves `edge`.
use std::path::Path;

fn read(relative: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)).unwrap_or_else(|e| panic!("{relative}: {e}"))
}

/// The lines of the block that follows `key: |` in `text` (the first such key), without their indentation.
fn block(text: &str, key: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().position(|l| l.trim() == format!("{key}: |")).unwrap_or_else(|| panic!("no `{key}: |` block"));
    let indent = |l: &str| l.len() - l.trim_start().len();
    let base = indent(lines[start]);
    lines[start + 1..].iter().take_while(|l| l.trim().is_empty() || indent(l) > base).map(|l| l.trim().to_owned()).filter(|l| !l.is_empty() && !l.starts_with('#')).collect()
}

fn workflow() -> String {
    read(".github/workflows/docker.yml")
}

#[test]
fn no_rule_sets_latest_for_a_branch() {
    let text = workflow();
    let code: Vec<&str> = text.lines().filter(|l| !l.trim_start().starts_with('#')).collect();
    assert!(!code.iter().any(|l| l.contains("value=latest")), "a rule that sets `latest` by hand");
    assert!(!code.iter().any(|l| l.contains("type=raw")), "a raw tag rule");
    assert!(!code.iter().any(|l| l.contains("is_default_branch")), "a rule that follows the default branch");
}

#[test]
fn the_tags_are_the_version_of_a_release_tag_and_edge_for_master_and_nothing_else() {
    let rules = block(&workflow(), "tags");
    assert_eq!(rules, ["type=semver,pattern={{version}}", "type=edge,branch=master"], "no sha tag, no major.minor tag, nothing else");
}

#[test]
fn latest_comes_only_from_the_automatic_rule_for_release_tags() {
    assert_eq!(block(&workflow(), "flavor"), ["latest=auto"]);
}

#[test]
fn only_a_release_tag_or_master_publishes() {
    let text = workflow();
    let pushes: Vec<&str> = text.lines().map(str::trim).filter(|l| l.starts_with("push: ")).collect();
    assert_eq!(pushes, ["push: ${{ github.ref_type == 'tag' || github.ref == 'refs/heads/master' }}"], "a push that is not conditional, or a different condition");
    // And the triggers are the same as before: master, release tags, and a manual run
    assert!(text.contains("branches: [master]") && text.contains("tags: [\"v*\"]") && text.contains("workflow_dispatch:"));
}

#[test]
fn the_release_guide_says_that_master_moves_edge_and_only_a_release_tag_moves_latest() {
    let guide = read("docs/releasing.md");
    assert!(guide.contains("publishes the `edge` Docker image, never `latest`"), "the guide does not say it");
    assert!(!guide.contains("and updates `latest`") && !guide.contains("moves only `latest`"), "the guide says master moves latest");
    assert!(guide.contains("`flavor: latest=auto`"));
}
