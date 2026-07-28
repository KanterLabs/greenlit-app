//! Shared workflow sources and output selectors.

pub(super) const LITERAL_VAR_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    if: vars.MODE == 'ci'
    steps:
      - run: echo hi
";

pub(super) const PR_TYPE_FILTER_WORKFLOW: &str = "\
on:
  pull_request:
    types: [closed]
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
";

pub(super) const PATH_FILTER_WORKFLOW: &str = "\
on:
  push:
    paths: [src/**]
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
";

pub(super) const PR_COMPARISON_WORKFLOW: &str = "\
on:
  push:
    paths: [src/**]
  pull_request:
    branches: [release]
    paths: [src/**]
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
";

pub(super) fn workflow_with_trigger(trigger: &str) -> String {
    format!(
        "on:\n{trigger}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    )
}
