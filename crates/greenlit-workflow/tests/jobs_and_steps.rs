//! Oracle tables for documented `jobs.<id>:` and `jobs.<id>.steps[]:` fields.

#[path = "jobs_and_steps/extraction.rs"]
mod extraction;
#[path = "jobs_and_steps/job_fields.rs"]
mod job_fields;
#[path = "jobs_and_steps/steps.rs"]
mod steps;

const HEADER: &str = "on: push\n";
