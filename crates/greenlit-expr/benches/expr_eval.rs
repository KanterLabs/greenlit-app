//! Criterion micro-benchmarks for the parse-and-evaluate hot path.
//!
//! Per `AGENTS.md` ("Metrics"): "Micro-benchmarks (criterion) for the parser
//! and expression evaluator live in CI from Phase 1 recording baselines;
//! Phase 5's budget enforcement extends this harness rather than starting
//! fresh." No performance budget is enforced yet — this only needs to
//! compile and run cleanly under `cargo bench -p greenlit-expr`, recording a
//! baseline for later phases to build on.

use criterion::{Criterion, criterion_group, criterion_main};
use greenlit_expr::{Context, RealFs, Value, evaluate, parse};
use std::hint::black_box;
use std::sync::Arc;

/// A representative, moderately complex expression: property/index access
/// through two contexts, an object filter, a boolean short-circuit, and a
/// built-in function call — exercising the lexer, the full precedence
/// chain, `Index`/`Wildcard` evaluation, and function dispatch together
/// rather than any single narrow path.
const EXPR: &str = "contains(github.event.labels.*.name, 'bug') && (matrix.os == 'ubuntu-latest' || env.FORCE == 'true')";

fn sample_context() -> Context {
    let labels = Value::array(vec![
        Value::object(vec![("name".into(), Value::String("bug".into()))]),
        Value::object(vec![("name".into(), Value::String("enhancement".into()))]),
    ]);
    let event = Value::object(vec![("labels".into(), labels)]);
    Context::new(Arc::new(RealFs::new(std::env::temp_dir())))
        .with_github(Value::object(vec![("event".into(), event)]))
        .with_matrix(Value::object(vec![(
            "os".into(),
            Value::String("ubuntu-latest".into()),
        )]))
        .with_env(Value::object(vec![(
            "FORCE".into(),
            Value::String("false".into()),
        )]))
}

fn bench_parse(c: &mut Criterion) {
    c.bench_function("parse", |b| {
        b.iter(|| parse(black_box(EXPR)));
    });
}

fn bench_parse_and_evaluate(c: &mut Criterion) {
    let ctx = sample_context();
    c.bench_function("parse_and_evaluate", |b| {
        b.iter(|| parse(black_box(EXPR)).map(|expr| evaluate(&expr, black_box(&ctx))));
    });
}

fn bench_evaluate_only(c: &mut Criterion) {
    let ctx = sample_context();
    let Ok(expr) = parse(EXPR) else {
        return;
    };
    c.bench_function("evaluate_only", |b| {
        b.iter(|| evaluate(black_box(&expr), black_box(&ctx)));
    });
}

criterion_group!(
    benches,
    bench_parse,
    bench_parse_and_evaluate,
    bench_evaluate_only
);
criterion_main!(benches);
