//! Public CLI coverage for bounded recent-history and append-tail work.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

use super::{Sandbox, WORKFLOW, metrics_record, support, write_metrics};

const LARGE_RECORD_PADDING_BYTES: usize = 6 * 1024 * 1024;

#[test]
fn stats_reads_only_the_retained_tail_and_caps_aggregate_record_bytes() {
    let sparse = Sandbox::new();
    let path = sparse.metrics_file();
    std::fs::create_dir_all(path.parent().expect("metrics parent"))
        .expect("create metrics directory");
    let mut file = File::create(&path).expect("create sparse metrics history");
    file.write_all(b"old-corruption\n")
        .expect("write old corrupt record");
    file.seek(SeekFrom::Start(4 * 1024 * 1024 * 1024))
        .expect("seek across sparse old history");
    file.write_all(b"\n")
        .expect("terminate sparse old-history record");
    for index in 0..25 {
        writeln!(
            file,
            "{}",
            metrics_record(greenlit_metrics::SCHEMA_VERSION, index)
        )
        .expect("write recent record");
    }
    drop(file);

    let output = sparse.run(&["stats"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(stdout.contains("up to 20, 20 shown"), "{stdout}");
    assert!(stdout.contains("t=24 "), "{stdout}");
    assert!(!stdout.contains("t=4 "), "{stdout}");

    // Three individually valid records fit the per-record cap, but all three
    // exceed read_recent's aggregate serialized-byte budget. Unknown JSON
    // fields let the fixture stay large without making renderer output large.
    let aggregate = Sandbox::new();
    let path = aggregate.metrics_file();
    std::fs::create_dir_all(path.parent().expect("metrics parent"))
        .expect("create metrics directory");
    let mut file = File::create(path).expect("create large metrics history");
    for started_at in 1..=3 {
        write_padded_record(&mut file, started_at, LARGE_RECORD_PADDING_BYTES);
    }
    drop(file);

    let output = aggregate.run(&["stats"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(stdout.contains("up to 20, 2 shown"), "{stdout}");
    assert!(!stdout.contains("t=1 "), "{stdout}");
    assert!(stdout.contains("t=2 "), "{stdout}");
    assert!(stdout.contains("t=3 "), "{stdout}");
}

#[test]
fn append_never_truncates_without_a_bounded_known_newline_boundary() {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", WORKFLOW);
    sandbox.init_git();
    write_metrics(
        &sandbox,
        format!("{}\n", metrics_record(greenlit_metrics::SCHEMA_VERSION, 8)),
    );

    let path = sandbox.metrics_file();
    let file = OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open metrics history for sparse extension");
    let committed_len = file.metadata().expect("metrics metadata").len();
    file.set_len(committed_len + 32 * 1024 * 1024)
        .expect("create sparse unknown tail");
    let unknown_len = file.metadata().expect("extended metrics metadata").len();
    drop(file);

    let output = sandbox.run(&["plan", "-W", "wf.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("per-record safety limit"), "{stderr}");
    assert!(stderr.contains("move the listed runs.ndjson"), "{stderr}");
    assert_eq!(
        std::fs::metadata(path)
            .expect("metrics history remains")
            .len(),
        unknown_len,
        "an unknown tail boundary must never be truncated"
    );
}

fn write_padded_record(file: &mut File, started_at: u128, padding_bytes: usize) {
    let schema = greenlit_metrics::SCHEMA_VERSION;
    write!(
        file,
        "{{\"schema_version\":{schema},\"command\":\"plan\",\"started_at_unix_ms\":{started_at},\"total_duration_ms\":1.0,\"stages\":[],\"steps\":[],\"hit_miss\":[],\"padding\":\""
    )
    .expect("write large record prefix");
    let mut padding = std::io::repeat(b'x').take(padding_bytes as u64);
    std::io::copy(&mut padding, file).expect("stream large record padding");
    writeln!(file, "\"}}").expect("write large record suffix");
}
