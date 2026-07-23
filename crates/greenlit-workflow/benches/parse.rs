//! Criterion micro-benchmark for the parser's hot path: parsing a
//! realistic multi-job workflow (`fixtures/multi_job.yml` — matrix with
//! `include`/`exclude`, `needs` chain, services, a job container, and a
//! mix of `run`/`uses` steps) plus running static extraction over the
//! result, end to end.
//!
//! Per `PHASE-1-engine-core.md` ("Criterion micro-benchmarks … recording
//! baselines (no budgets yet — history first)"), this only needs to record
//! a baseline, not enforce one; Phase 5 extends this harness with budgets.

use criterion::{Criterion, criterion_group, criterion_main};
use greenlit_workflow::{extract_static, parse_workflow};
use std::hint::black_box;

const MULTI_JOB_WORKFLOW: &str = include_str!("fixtures/multi_job.yml");

fn bench_parse_and_extract(c: &mut Criterion) {
    c.bench_function("parse_workflow(multi_job.yml)", |b| {
        b.iter(|| black_box(parse_workflow("ci.yml", black_box(MULTI_JOB_WORKFLOW))));
    });

    let Ok(workflow) = parse_workflow("ci.yml", MULTI_JOB_WORKFLOW) else {
        return;
    };
    c.bench_function("extract_static(multi_job.yml)", |b| {
        b.iter(|| black_box(extract_static(black_box(&workflow))));
    });
}

criterion_group!(benches, bench_parse_and_extract);
criterion_main!(benches);
