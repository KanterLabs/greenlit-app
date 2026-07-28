//! Public-boundary coverage for the declared stable metrics-record schema.

use greenlit_metrics::{
    HitMissCounter, InvocationRecord, SCHEMA_VERSION, StageDuration, StepDuration,
};

// This is the one declared stable snapshot surface for this crate
// (`TESTING.md`: metrics records are an allowed stable schema). It pins the
// exact serialized field set, order, and types through the crate's public
// API. A deliberate schema change updates both this expected string and
// `SCHEMA_VERSION`, never one without the other.
#[test]
fn invocation_record_json_shape_is_pinned() {
    let record = InvocationRecord {
        schema_version: SCHEMA_VERSION,
        command: "plan".to_string(),
        started_at_unix_ms: 1_700_000_000_000,
        total_duration_ms: 12.5,
        stages: vec![
            StageDuration {
                name: "parse".to_string(),
                duration_ms: 1.0,
            },
            StageDuration {
                name: "eval".to_string(),
                duration_ms: 2.5,
            },
            StageDuration {
                name: "plan".to_string(),
                duration_ms: 9.0,
            },
        ],
        steps: vec![StepDuration {
            job: "build".to_string(),
            step: "compile".to_string(),
            duration_ms: 7.5,
        }],
        hit_miss: vec![HitMissCounter {
            name: "cache".to_string(),
            hits: 3,
            misses: 1,
            bytes: 4096,
        }],
    };

    let json = serde_json::to_string(&record).expect("record must serialize");
    assert_eq!(
        json,
        "{\"schema_version\":2,\"command\":\"plan\",\"started_at_unix_ms\":1700000000000,\
         \"total_duration_ms\":12.5,\"stages\":[{\"name\":\"parse\",\"duration_ms\":1.0},\
         {\"name\":\"eval\",\"duration_ms\":2.5},{\"name\":\"plan\",\"duration_ms\":9.0}],\
         \"steps\":[{\"job\":\"build\",\"step\":\"compile\",\"duration_ms\":7.5}],\
         \"hit_miss\":[{\"name\":\"cache\",\"hits\":3,\"misses\":1,\"bytes\":4096}]}"
    );

    let parsed: InvocationRecord = serde_json::from_str(&json).expect("record must parse");
    assert_eq!(parsed, record);
}
