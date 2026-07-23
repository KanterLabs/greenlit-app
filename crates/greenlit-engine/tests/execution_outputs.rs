//! Table/fake-driven tests for job-output finalization, the live
//! `steps`/`needs` contexts, matrix-output merging, secret redaction, and the
//! output size cap. No container required.

use std::io;
use std::path::Path;
use std::sync::Arc;

use indexmap::IndexMap;

use greenlit_engine::condition::DeferredExpr;
use greenlit_engine::execution::contexts::{
    NeedRecord, StepRecord, build_needs_context, build_steps_context, merge_matrix_outputs,
};
use greenlit_engine::execution::job_outputs::{
    JobOutputError, MAX_JOB_OUTPUTS_BYTES, finalize_outputs, job_result,
};
use greenlit_engine::execution::log_commands::Masker;
use greenlit_engine::execution::outcome::Conclusion;
use greenlit_engine::graph::JobId;
use greenlit_engine::outputs::{JobOutputsPlan, PlannedOutput, PlannedValue};
use greenlit_expr::{
    Context, EntryKind, HashFilesFs, OpenedDir, RunStatus, Value, evaluate, parse,
};
use greenlit_workflow::{Location, Span};

#[derive(Debug)]
struct NoFs;

impl HashFilesFs for NoFs {
    fn workspace_root(&self) -> &Path {
        Path::new("/workspace")
    }
    fn open_dir(&self, _path: &Path) -> io::Result<OpenedDir<'_>> {
        Err(io::Error::other("no fs"))
    }
    fn entry_kind(&self, _path: &Path) -> io::Result<EntryKind> {
        Err(io::Error::other("no fs"))
    }
    fn hash_file_sha256(
        &self,
        _path: &Path,
        _check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        Err(io::Error::other("no fs"))
    }
}

fn span() -> Span {
    Span::new(
        Arc::from("test.yml"),
        Location::new(1, 1),
        Location::new(1, 1),
    )
}

fn map(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn outputs_plan(entries: Vec<(&str, PlannedValue)>) -> JobOutputsPlan {
    let mut map = IndexMap::new();
    for (name, value) in entries {
        map.insert(
            name.to_string(),
            PlannedOutput {
                span: span(),
                source: name.to_string(),
                value,
            },
        );
    }
    JobOutputsPlan { entries: map }
}

fn deferred(expr: &str) -> PlannedValue {
    PlannedValue::Deferred(DeferredExpr {
        residual: parse(expr).expect("valid expression"),
        residual_text: expr.to_string(),
        defers_on: Vec::new(),
    })
}

#[test]
fn steps_context_exposes_outcome_conclusion_and_outputs() {
    let steps = build_steps_context(&[StepRecord {
        id: "build".to_string(),
        outcome: Conclusion::Success,
        conclusion: Conclusion::Success,
        outputs: map(&[("artifact", "app.tar")]),
    }]);
    let ctx = Context::new(Arc::new(NoFs)).with_steps(steps);
    assert_eq!(
        evaluate(&parse("steps.build.outputs.artifact").unwrap(), &ctx).unwrap(),
        Value::String("app.tar".to_string())
    );
    assert_eq!(
        evaluate(&parse("steps.build.conclusion").unwrap(), &ctx).unwrap(),
        Value::String("success".to_string())
    );
}

#[test]
fn needs_context_carries_result_and_outputs_for_direct_deps() {
    let needs = build_needs_context(&[NeedRecord {
        job: JobId("build".to_string()),
        result: Conclusion::Success,
        outputs: map(&[("image", "sha256:abc")]),
    }]);
    let ctx = Context::new(Arc::new(NoFs)).with_needs(needs);
    assert_eq!(
        evaluate(&parse("needs.build.result").unwrap(), &ctx).unwrap(),
        Value::String("success".to_string())
    );
    assert_eq!(
        evaluate(&parse("needs.build.outputs.image").unwrap(), &ctx).unwrap(),
        Value::String("sha256:abc".to_string())
    );
}

#[test]
fn matrix_outputs_last_leg_wins() {
    let leg_a = map(&[("os", "linux"), ("shared", "a")]);
    let leg_b = map(&[("arch", "x64"), ("shared", "b")]);
    let merged = merge_matrix_outputs([&leg_a, &leg_b]);
    assert_eq!(merged.get("os").unwrap(), "linux");
    assert_eq!(merged.get("arch").unwrap(), "x64");
    // A shared key is clobbered by the later leg.
    assert_eq!(merged.get("shared").unwrap(), "b");
}

#[test]
fn finalizes_static_and_deferred_outputs() {
    let steps = build_steps_context(&[StepRecord {
        id: "gen".to_string(),
        outcome: Conclusion::Success,
        conclusion: Conclusion::Success,
        outputs: map(&[("version", "1.2.3")]),
    }]);
    let ctx = Context::new(Arc::new(NoFs)).with_steps(steps);
    let plan = outputs_plan(vec![
        ("literal", PlannedValue::Static("fixed".to_string())),
        ("ver", deferred("steps.gen.outputs.version")),
    ]);
    let out = finalize_outputs(&plan, &ctx, &Masker::new()).unwrap();
    assert_eq!(out.get("literal").unwrap(), "fixed");
    assert_eq!(out.get("ver").unwrap(), "1.2.3");
}

#[test]
fn secret_values_are_redacted_from_outputs() {
    let ctx = Context::new(Arc::new(NoFs));
    let mut masker = Masker::new();
    masker.add("s3cr3t");
    let plan = outputs_plan(vec![(
        "leak",
        PlannedValue::Static("prefix-s3cr3t-suffix".to_string()),
    )]);
    let out = finalize_outputs(&plan, &ctx, &masker).unwrap();
    assert_eq!(out.get("leak").unwrap(), "prefix-***-suffix");
}

#[test]
fn oversized_outputs_are_rejected() {
    let ctx = Context::new(Arc::new(NoFs));
    let big = "x".repeat(MAX_JOB_OUTPUTS_BYTES + 1);
    let plan = outputs_plan(vec![("big", PlannedValue::Static(big))]);
    assert!(matches!(
        finalize_outputs(&plan, &ctx, &Masker::new()),
        Err(JobOutputError::TooLarge { .. })
    ));
}

#[test]
fn job_result_maps_status_and_skip() {
    assert_eq!(job_result(true, RunStatus::Success), Conclusion::Success);
    assert_eq!(job_result(true, RunStatus::Failure), Conclusion::Failure);
    assert_eq!(
        job_result(true, RunStatus::Cancelled),
        Conclusion::Cancelled
    );
    // A job that never ran (its own `if:` was false) is skipped.
    assert_eq!(job_result(false, RunStatus::Success), Conclusion::Skipped);
}
