//! Synthetic push and pull-request branch, activity, and path filters.

use super::common::*;
use super::support;
use super::support::Sandbox;

#[test]
fn event_selection_and_invalid_filter_patterns_fail_actionably() {
    let rows = [
        (
            "event not declared",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
            vec!["plan", "-W", "wf.yml", "-e", "pull_request"],
            "wf.yml:1:1",
            "workflow does not declare the `pull_request` event",
            "fix: select an event declared under `on:`, or add this event to the workflow",
        ),
        (
            "invalid branch filter",
            "on:\n  push:\n    branches: [+main]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
            vec!["plan", "-W", "wf.yml"],
            "wf.yml:3:16",
            "invalid `branches` filter pattern '+main'",
            "fix: fix the trigger filter pattern at the location above",
        ),
    ];

    for (name, source, args, location, message, fix) in rows {
        let sandbox = Sandbox::new();
        sandbox.write("wf.yml", source);
        sandbox.init_git();

        let output = sandbox.run(&args);
        assert!(
            !output.status.success(),
            "row '{name}' unexpectedly planned"
        );
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains(location), "row '{name}': {stderr}");
        assert!(stderr.contains(message), "row '{name}': {stderr}");
        assert!(stderr.contains(fix), "row '{name}': {stderr}");
    }
}

#[test]
fn synthetic_events_honor_branch_activity_and_path_filters() {
    let branch_filtered = workflow_with_trigger("  push:\n    branches: [main]\n");
    for (source, branch, event, expected_detail) in [
        (branch_filtered.as_str(), "dev", "push", "branch 'dev'"),
        (
            PR_TYPE_FILTER_WORKFLOW,
            "main",
            "pull_request",
            "activity 'opened'",
        ),
        (PATH_FILTER_WORKFLOW, "main", "push", "1 compared path(s)"),
    ] {
        let sandbox = Sandbox::new();
        sandbox.write("wf.yml", source);
        sandbox.init_git_on(branch);

        let output = sandbox.run(&["plan", "-W", "wf.yml", "-e", event]);
        assert!(!output.status.success());
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains("does not satisfy its trigger filters"));
        assert!(stderr.contains(expected_detail), "{stderr}");
        assert!(stderr.contains("fix:"));
    }

    for (trigger, branch, event) in [
        ("  push:\n    tags: ['v*']\n", "main", "push"),
        ("  push:\n    branches-ignore: [main]\n", "main", "push"),
        ("  push:\n    paths-ignore: ['wf.yml']\n", "main", "push"),
        (
            "  push:\n    branches: ['release/*']\n",
            "release/deep/10",
            "push",
        ),
    ] {
        let sandbox = Sandbox::new();
        sandbox.write("wf.yml", &workflow_with_trigger(trigger));
        sandbox.init_git_on(branch);

        let output = sandbox.run(&["plan", "-W", "wf.yml", "-e", event]);
        assert!(!output.status.success());
        assert!(support::stderr_text(&output).contains("does not satisfy its trigger filters"));
    }

    for (trigger, branch, event) in [
        (
            "  push:\n    branches: ['releases+/[1-2]0']\n",
            "releases/10",
            "push",
        ),
        (
            "  push:\n    branches: ['**', '!dev', 'dev']\n",
            "dev",
            "push",
        ),
        ("  push:\n    paths: ['wf.yml']\n", "main", "push"),
        ("  push:\n    paths-ignore: ['docs/**']\n", "main", "push"),
        (
            "  pull_request:\n    types: [opened]\n    branches: [main]\n",
            "main",
            "pull_request",
        ),
        (
            "  pull_request:\n    branches: [trunk]\n",
            "trunk",
            "pull_request",
        ),
    ] {
        let sandbox = Sandbox::new();
        sandbox.write("wf.yml", &workflow_with_trigger(trigger));
        sandbox.init_git_on(branch);

        let output = sandbox.run(&["plan", "-W", "wf.yml", "-e", event]);
        assert!(output.status.success(), "{}", support::stderr_text(&output));
    }

    // `**/` spans zero or more directories in GitHub's filter grammar.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#filter-pattern-cheat-sheet
    for (trigger, extra_path) in [
        ("  push:\n    paths: ['**/wf.yml']\n", None),
        (
            "  push:\n    paths: ['docs/**/*.md']\n",
            Some("docs/README.md"),
        ),
    ] {
        let sandbox = Sandbox::new();
        sandbox.write("wf.yml", &workflow_with_trigger(trigger));
        if let Some(path) = extra_path {
            sandbox.write(path, "documentation\n");
        }
        sandbox.init_git();

        let output = sandbox.run(&["plan", "-W", "wf.yml", "-e", "push"]);
        assert!(output.status.success(), "{}", support::stderr_text(&output));
    }

    // GitHub evaluates only the first 3,000 files in a generated diff. A
    // match at that boundary starts the workflow; the same match shifted to
    // position 3,001 does not.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#git-diff-comparisons
    let sandbox = Sandbox::new();
    sandbox.write(
        "wf.yml",
        &workflow_with_trigger("  pull_request:\n    paths: ['zz-match.txt']\n"),
    );
    sandbox.init_git();
    sandbox.git(&["checkout", "-q", "-b", "feature"]);
    for index in 0..2_999 {
        sandbox.write(&format!("a/{index:04}.txt"), "changed\n");
    }
    sandbox.write("zz-match.txt", "matched\n");
    sandbox.git(&["add", "."]);
    sandbox.git(&["commit", "-q", "-m", "three thousand paths"]);

    let at_limit = sandbox.run(&["plan", "-W", "wf.yml", "-e", "pull_request"]);
    assert!(
        at_limit.status.success(),
        "{}",
        support::stderr_text(&at_limit)
    );

    sandbox.write("a/2999.txt", "changed\n");
    sandbox.git(&["add", "."]);
    sandbox.git(&["commit", "-q", "-m", "three thousand and one paths"]);
    let over_limit = sandbox.run(&["plan", "-W", "wf.yml", "-e", "pull_request"]);
    assert!(!over_limit.status.success());
    let stderr = support::stderr_text(&over_limit);
    assert!(
        stderr.contains("does not satisfy its trigger filters"),
        "{stderr}"
    );
    assert!(stderr.contains("3000 compared path(s)"), "{stderr}");
    assert!(
        stderr.contains("comparison truncated at GitHub's 3,000-path limit"),
        "{stderr}"
    );

    // A configured non-self upstream is the synthetic PR base even when a
    // different remote default exists. The PR's three-dot comparison keeps
    // an earlier topic-branch change visible, while the synthetic push still
    // evaluates only the latest commit.
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", PR_COMPARISON_WORKFLOW);
    sandbox.init_git();
    sandbox.git(&["branch", "release"]);
    sandbox.git(&["branch", "trunk"]);
    sandbox.git(&[
        "update-ref",
        "refs/remotes/origin/trunk",
        "refs/heads/trunk",
    ]);
    sandbox.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/trunk",
    ]);
    sandbox.git(&["checkout", "-q", "-b", "feature", "release"]);
    sandbox.git(&["config", "branch.feature.remote", "."]);
    sandbox.git(&["config", "branch.feature.merge", "refs/heads/release"]);
    sandbox.write("src/earlier.rs", "fn earlier() {}\n");
    sandbox.git(&["add", "."]);
    sandbox.git(&["commit", "-q", "-m", "add source"]);
    sandbox.write("docs/latest.md", "latest\n");
    sandbox.git(&["add", "."]);
    sandbox.git(&["commit", "-q", "-m", "add docs"]);

    let pull_request = sandbox.run(&["plan", "-W", "wf.yml", "-e", "pull_request"]);
    assert!(
        pull_request.status.success(),
        "{}",
        support::stderr_text(&pull_request)
    );
    let push = sandbox.run(&["plan", "-W", "wf.yml", "-e", "push"]);
    assert!(!push.status.success());
    assert!(support::stderr_text(&push).contains("1 compared path(s)"));

    // Without an upstream, the remote's symbolic default branch is the
    // strongest available base-branch signal, ahead of conventional local
    // branch-name fallbacks.
    let sandbox = Sandbox::new();
    sandbox.write(
        "wf.yml",
        &workflow_with_trigger("  pull_request:\n    branches: [trunk]\n"),
    );
    sandbox.init_git();
    sandbox.git(&["branch", "trunk"]);
    sandbox.git(&[
        "update-ref",
        "refs/remotes/origin/trunk",
        "refs/heads/trunk",
    ]);
    sandbox.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/trunk",
    ]);
    sandbox.git(&["checkout", "-q", "-b", "feature", "trunk"]);
    sandbox.write("feature.txt", "feature\n");
    sandbox.git(&["add", "."]);
    sandbox.git(&["commit", "-q", "-m", "feature"]);

    let output = sandbox.run(&["plan", "-W", "wf.yml", "-e", "pull_request"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));

    // Ordered negative/re-include semantics remain exact across the
    // implementation's internal pattern batches. This is exercised through
    // the real CLI boundary so the test pins authored workflow behavior,
    // independent of the matcher implementation.
    let mut patterns = vec!["'**'".to_owned()];
    patterns.extend((1..128).map(|index| format!("'never-{index}'")));
    patterns.push("'!**'".to_owned());
    let excluded_trigger = format!("  push:\n    branches: [{}]\n", patterns.join(", "));
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", &workflow_with_trigger(&excluded_trigger));
    sandbox.init_git_on("dev");
    let excluded = sandbox.run(&["plan", "-W", "wf.yml"]);
    assert!(!excluded.status.success());

    patterns.extend((129..256).map(|index| format!("'never-{index}'")));
    patterns.push("'**'".to_owned());
    let reincluded_trigger = format!("  push:\n    branches: [{}]\n", patterns.join(", "));
    sandbox.write("wf.yml", &workflow_with_trigger(&reincluded_trigger));
    let reincluded = sandbox.run(&["plan", "-W", "wf.yml"]);
    assert!(
        reincluded.status.success(),
        "{}",
        support::stderr_text(&reincluded)
    );
}
