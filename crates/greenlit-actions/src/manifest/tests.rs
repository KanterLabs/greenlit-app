//! Oracle-style coverage for `action.yml` parsing: one case per documented
//! schema rule this crate models (`PHASE-3-actions.md`'s field list), plus
//! this crate's own deliberate scope decisions (unknown-key/null/scope
//! documented in `manifest`'s module docs).

use indexmap::IndexMap;

use super::*;

fn parse(source: &str) -> Result<ActionManifest, ManifestError> {
    parse_manifest("action.yml", source)
}

#[test]
fn parses_a_minimal_composite_action() {
    let manifest = parse(
        r#"
name: My Action
description: does a thing
runs:
  using: composite
  steps:
    - run: echo hi
      shell: bash
"#,
    )
    .unwrap();
    assert_eq!(manifest.name.as_deref(), Some("My Action"));
    assert_eq!(manifest.description.as_deref(), Some("does a thing"));
    let Runs::Composite { steps } = manifest.runs else {
        panic!("expected composite runs");
    };
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].run.as_deref(), Some("echo hi"));
    assert_eq!(steps[0].shell.as_deref(), Some("bash"));
}

#[test]
fn name_and_description_are_optional() {
    let manifest = parse(
        r#"
runs:
  using: composite
  steps:
    - run: echo hi
      shell: bash
"#,
    )
    .unwrap();
    assert_eq!(manifest.name, None);
    assert_eq!(manifest.description, None);
}

#[test]
fn parses_a_node20_action_with_pre_and_post() {
    let manifest = parse(
        r#"
name: n
description: d
runs:
  using: node20
  main: index.js
  pre: setup.js
  pre-if: runner.os == 'Linux'
  post: cleanup.js
  post-if: always()
"#,
    )
    .unwrap();
    let Runs::Node(node) = manifest.runs else {
        panic!("expected node runs");
    };
    assert_eq!(node.using, NodeVersion::Node20);
    assert_eq!(node.main, "index.js");
    assert_eq!(node.pre.as_deref(), Some("setup.js"));
    assert_eq!(node.pre_if.as_deref(), Some("runner.os == 'Linux'"));
    assert_eq!(node.post.as_deref(), Some("cleanup.js"));
    assert_eq!(node.post_if.as_deref(), Some("always()"));
}

#[test]
fn parses_a_node24_action_without_pre_or_post() {
    let manifest = parse(
        r#"
runs:
  using: node24
  main: index.js
"#,
    )
    .unwrap();
    let Runs::Node(node) = manifest.runs else {
        panic!("expected node runs");
    };
    assert_eq!(node.using, NodeVersion::Node24);
    assert_eq!(node.pre, None);
    assert_eq!(node.post, None);
}

#[test]
fn parses_a_docker_action() {
    let manifest = parse(
        r#"
runs:
  using: docker
  image: docker://alpine:3.8
  entrypoint: /entrypoint.sh
  args:
    - one
    - ${{ inputs.who-to-greet }}
  env:
    GREETING: hello
"#,
    )
    .unwrap();
    let Runs::Docker(docker) = manifest.runs else {
        panic!("expected docker runs");
    };
    assert_eq!(docker.image, "docker://alpine:3.8");
    assert_eq!(docker.entrypoint.as_deref(), Some("/entrypoint.sh"));
    assert_eq!(docker.args, vec!["one", "${{ inputs.who-to-greet }}"]);
    assert_eq!(
        docker.env.get("GREETING").map(String::as_str),
        Some("hello")
    );
}

#[test]
fn docker_action_image_may_be_a_dockerfile_path() {
    let manifest = parse(
        r#"
runs:
  using: docker
  image: Dockerfile
"#,
    )
    .unwrap();
    let Runs::Docker(docker) = manifest.runs else {
        panic!("expected docker runs");
    };
    assert_eq!(docker.image, "Dockerfile");
    assert!(docker.args.is_empty());
    assert!(docker.env.is_empty());
}

#[test]
fn parses_inputs_with_every_documented_field() {
    let manifest = parse(
        r#"
runs:
  using: composite
  steps:
    - run: echo hi
      shell: bash
inputs:
  who-to-greet:
    description: who to greet
    required: true
    default: World
    deprecationMessage: use 'name' instead
  bare:
    description: no other fields
"#,
    )
    .unwrap();
    let who = &manifest.inputs["who-to-greet"];
    assert_eq!(who.description.as_deref(), Some("who to greet"));
    assert!(who.required);
    assert_eq!(who.default.as_deref(), Some("World"));
    assert_eq!(
        who.deprecation_message.as_deref(),
        Some("use 'name' instead")
    );

    let bare = &manifest.inputs["bare"];
    assert!(!bare.required, "required must default to false");
    assert_eq!(bare.default, None);
    assert_eq!(bare.deprecation_message, None);
}

#[test]
fn parses_outputs_including_composite_value() {
    let manifest = parse(
        r#"
runs:
  using: composite
  steps:
    - id: step1
      run: echo "greeting=hi" >> "$GITHUB_OUTPUT"
      shell: bash
outputs:
  greeting:
    description: the greeting
    value: ${{ steps.step1.outputs.greeting }}
"#,
    )
    .unwrap();
    let output = &manifest.outputs["greeting"];
    assert_eq!(output.description.as_deref(), Some("the greeting"));
    assert_eq!(
        output.value.as_deref(),
        Some("${{ steps.step1.outputs.greeting }}")
    );
}

#[test]
fn preserves_input_and_output_authored_order() {
    let manifest = parse(
        r#"
runs:
  using: composite
  steps:
    - run: echo hi
      shell: bash
inputs:
  zeta:
    description: z
  alpha:
    description: a
"#,
    )
    .unwrap();
    let keys: Vec<&str> = manifest.inputs.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["zeta", "alpha"]);
}

#[test]
fn author_and_branding_are_recognized_and_ignored() {
    // Near-universal in real marketplace `action.yml` files; see
    // `manifest::parse::TOP_LEVEL_KEYS`'s doc comment for why these are
    // accepted rather than rejected as unknown, unlike
    // `greenlit-workflow`'s stricter policy for authored workflow YAML.
    let manifest = parse(
        r#"
name: n
author: Someone
branding:
  icon: activity
  color: green
runs:
  using: composite
  steps:
    - run: echo hi
      shell: bash
"#,
    )
    .unwrap();
    assert_eq!(manifest.name.as_deref(), Some("n"));
}

#[test]
fn unquoted_default_value_is_used_verbatim_as_a_string() {
    let manifest = parse(
        r#"
runs:
  using: composite
  steps:
    - run: echo hi
      shell: bash
inputs:
  count:
    description: c
    default: 3
"#,
    )
    .unwrap();
    assert_eq!(manifest.inputs["count"].default.as_deref(), Some("3"));
}

#[test]
fn explicit_null_default_resolves_to_absent() {
    let manifest = parse(
        r#"
runs:
  using: composite
  steps:
    - run: echo hi
      shell: bash
inputs:
  count:
    description: c
    default: null
"#,
    )
    .unwrap();
    assert_eq!(manifest.inputs["count"].default, None);
}

#[test]
fn quoted_null_default_is_the_literal_string() {
    let manifest = parse(
        r#"
runs:
  using: composite
  steps:
    - run: echo hi
      shell: bash
inputs:
  count:
    description: c
    default: "null"
"#,
    )
    .unwrap();
    assert_eq!(manifest.inputs["count"].default.as_deref(), Some("null"));
}

#[test]
fn required_rejects_non_boolean_values() {
    let error = parse(
        r#"
runs:
  using: composite
  steps:
    - run: echo hi
      shell: bash
inputs:
  count:
    required: yes
"#,
    )
    .unwrap_err();
    // "yes" is deliberately not GitHub's Boolean grammar (only
    // true/True/TRUE/false/False/FALSE) — see `parse::util` module docs.
    assert!(matches!(error, ManifestError::Schema { .. }));
}

#[test]
fn missing_runs_is_a_missing_key_error() {
    let error = parse("name: n\n").unwrap_err();
    assert!(matches!(
        error,
        ManifestError::MissingKey { key: "runs", .. }
    ));
}

#[test]
fn unknown_top_level_key_is_rejected() {
    let error = parse(
        r#"
name: n
nonsense: true
runs:
  using: composite
  steps:
    - run: echo hi
      shell: bash
"#,
    )
    .unwrap_err();
    match error {
        ManifestError::UnknownKey { key, .. } => assert_eq!(key, "nonsense"),
        other => panic!("expected UnknownKey, got {other:?}"),
    }
}

#[test]
fn unsupported_using_names_the_supported_set() {
    let error = parse(
        r#"
runs:
  using: node16
  main: index.js
"#,
    )
    .unwrap_err();
    let message = error.to_string();
    match error {
        ManifestError::UnsupportedUsing { value, .. } => assert_eq!(value, "node16"),
        other => panic!("expected UnsupportedUsing, got {other:?}"),
    }
    assert!(message.contains("node20, node24, composite, docker"));
}

#[test]
fn duplicate_key_is_rejected_case_insensitively() {
    let error = parse(
        r#"
name: one
NAME: two
runs:
  using: composite
  steps:
    - run: echo hi
      shell: bash
"#,
    )
    .unwrap_err();
    assert!(matches!(error, ManifestError::DuplicateKey { .. }));
}

#[test]
fn composite_step_missing_run_and_uses_is_rejected() {
    let error = parse(
        r#"
runs:
  using: composite
  steps:
    - name: does nothing
"#,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        ManifestError::CompositeStepMissingRunOrUses { .. }
    ));
}

#[test]
fn composite_step_with_both_run_and_uses_is_rejected() {
    let error = parse(
        r#"
runs:
  using: composite
  steps:
    - run: echo hi
      shell: bash
      uses: actions/checkout@v4
"#,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        ManifestError::CompositeStepHasBothRunAndUses { .. }
    ));
}

#[test]
fn composite_step_run_without_shell_is_rejected() {
    let error = parse(
        r#"
runs:
  using: composite
  steps:
    - run: echo hi
"#,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        ManifestError::CompositeStepRunMissingShell { .. }
    ));
}

#[test]
fn composite_step_uses_does_not_require_shell() {
    let manifest = parse(
        r#"
runs:
  using: composite
  steps:
    - uses: actions/checkout@v4
      with:
        ref: main
"#,
    )
    .unwrap();
    let Runs::Composite { steps } = manifest.runs else {
        panic!("expected composite runs");
    };
    assert_eq!(steps[0].uses.as_deref(), Some("actions/checkout@v4"));
    let mut expected = IndexMap::new();
    expected.insert("ref".to_owned(), "main".to_owned());
    assert_eq!(steps[0].with, expected);
}

#[test]
fn composite_step_full_field_set() {
    let manifest = parse(
        r#"
runs:
  using: composite
  steps:
    - id: greet
      name: Say hi
      if: ${{ success() }}
      run: echo "hi $NAME"
      shell: bash
      working-directory: sub
      continue-on-error: true
      env:
        NAME: World
"#,
    )
    .unwrap();
    let Runs::Composite { steps } = manifest.runs else {
        panic!("expected composite runs");
    };
    let step = &steps[0];
    assert_eq!(step.id.as_deref(), Some("greet"));
    assert_eq!(step.name.as_deref(), Some("Say hi"));
    assert_eq!(step.if_condition.as_deref(), Some("${{ success() }}"));
    assert_eq!(step.working_directory.as_deref(), Some("sub"));
    assert_eq!(step.continue_on_error.as_deref(), Some("true"));
    assert_eq!(step.env.get("NAME").map(String::as_str), Some("World"));
}

#[test]
fn malformed_yaml_is_a_yaml_error() {
    let error = parse("runs: [this is not\n  closed").unwrap_err();
    assert!(matches!(error, ManifestError::Yaml { .. }));
}

#[test]
fn empty_source_is_an_empty_document_error() {
    let error = parse("").unwrap_err();
    assert!(matches!(error, ManifestError::EmptyDocument { .. }));
}

#[test]
fn multiple_documents_are_rejected() {
    let error = parse("runs:\n  using: composite\n  steps: []\n---\nname: two\n").unwrap_err();
    assert!(matches!(error, ManifestError::MultipleDocuments { .. }));
}

#[test]
fn anchors_are_rejected() {
    let error = parse(
        r#"
runs: &r
  using: composite
  steps: []
other: *r
"#,
    )
    .unwrap_err();
    assert!(matches!(error, ManifestError::AnchorsNotSupported { .. }));
}

#[test]
fn explicit_tags_are_rejected() {
    let error = parse(
        r#"
name: !!str n
runs:
  using: composite
  steps: []
"#,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        ManifestError::ExplicitTagsNotSupported { .. }
    ));
}

#[test]
fn oversized_source_is_rejected_before_parsing() {
    let huge = "a".repeat(MAX_MANIFEST_SOURCE_BYTES + 1);
    let error = parse(&huge).unwrap_err();
    assert!(matches!(error, ManifestError::SourceLimit { .. }));
}

#[test]
fn composite_action_with_no_steps_is_valid_but_empty() {
    let manifest = parse("runs:\n  using: composite\n  steps: []\n").unwrap();
    let Runs::Composite { steps } = manifest.runs else {
        panic!("expected composite runs");
    };
    assert!(steps.is_empty());
}

#[test]
fn load_manifest_tries_yml_then_yaml() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("action.yaml"),
        "name: yaml-form\nruns:\n  using: composite\n  steps:\n    - run: echo hi\n      shell: bash\n",
    )
    .unwrap();
    let manifest = load_manifest(dir.path()).unwrap();
    assert_eq!(manifest.name.as_deref(), Some("yaml-form"));
}

#[test]
fn load_manifest_prefers_yml_over_yaml() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("action.yml"),
        "name: yml-form\nruns:\n  using: composite\n  steps:\n    - run: echo hi\n      shell: bash\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("action.yaml"),
        "name: yaml-form\nruns:\n  using: composite\n  steps:\n    - run: echo hi\n      shell: bash\n",
    )
    .unwrap();
    let manifest = load_manifest(dir.path()).unwrap();
    assert_eq!(manifest.name.as_deref(), Some("yml-form"));
}

#[test]
fn load_manifest_errors_when_neither_file_exists() {
    let dir = tempfile::tempdir().unwrap();
    let error = load_manifest(dir.path()).unwrap_err();
    assert!(matches!(error, ManifestError::NotFound { .. }));
}
