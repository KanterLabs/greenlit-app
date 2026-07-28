//! Criterion micro-benchmark for the parser's hot path: parsing a
//! realistic multi-job workflow (`fixtures/multi_job.yml` — matrix with
//! `include`/`exclude`, `needs` chain, services, a job container, and a
//! mix of `run`/`uses` steps) plus running static extraction over the
//! result, end to end.
//!
//! Per `PHASE-1-engine-core.md` ("Criterion micro-benchmarks … recording
//! baselines (no budgets yet — history first)"), this target records the
//! measurements; the manifest-authoritative
//! `tools/tests/check-criterion-manifest` boundary now enforces the fixed-host
//! budgets for each declared benchmark identity.

use criterion::{Criterion, criterion_group, criterion_main};
use greenlit_workflow::{extract_static, parse_workflow};
use std::hint::black_box;

const MULTI_JOB_WORKFLOW: &str = include_str!("fixtures/multi_job.yml");

fn setup_failed(stage: &str, error: impl std::fmt::Display) -> ! {
    eprintln!(
        "Criterion benchmark setup failed while {stage}: {error}\n\
         fix: repair benches/fixtures/multi_job.yml before recording a baseline"
    );
    std::process::exit(2);
}

fn bench_parse_and_extract(c: &mut Criterion) {
    let workflow = match parse_workflow("ci.yml", MULTI_JOB_WORKFLOW) {
        Ok(workflow) => workflow,
        Err(error) => setup_failed("parsing the multi-job fixture", error),
    };
    if let Err(error) = extract_static(&workflow) {
        setup_failed("extracting static workflow references", error);
    }

    c.bench_function("parse_workflow(multi_job.yml)", |b| {
        b.iter(|| black_box(parse_workflow("ci.yml", black_box(MULTI_JOB_WORKFLOW))));
    });

    c.bench_function("extract_static(multi_job.yml)", |b| {
        b.iter(|| black_box(extract_static(black_box(&workflow))));
    });
}

criterion_group!(benches, bench_parse_and_extract);
criterion_main!(benches);
