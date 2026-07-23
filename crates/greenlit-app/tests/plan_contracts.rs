//! Public `litci plan` contracts not owned by the `matrix-needs` fixture.

pub mod support;

#[path = "plan_contracts/common.rs"]
mod common;
#[path = "plan_contracts/controls.rs"]
mod controls;
#[path = "plan_contracts/dispatch.rs"]
mod dispatch;
#[path = "plan_contracts/dispatch_failures.rs"]
mod dispatch_failures;
#[path = "plan_contracts/pull_request.rs"]
mod pull_request;
#[path = "plan_contracts/rejections.rs"]
mod rejections;
#[path = "plan_contracts/resource_limits.rs"]
mod resource_limits;
