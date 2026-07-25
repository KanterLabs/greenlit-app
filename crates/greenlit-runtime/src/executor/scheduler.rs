//! Concurrent DAG and matrix scheduling.
//!
//! Jobs in the same dependency wave are ready together and may run in
//! parallel. Each matrix retains declaration-order reports even though its
//! legs execute concurrently. A process-wide worker semaphore bounds resource
//! use; matrix `max-parallel` supplies the tighter per-group limit.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use greenlit_engine::execution::{Masker, NeedRecord};
use greenlit_engine::{Conclusion, Evaluation, JobId};
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};

use super::instance::JobGroup;
use super::report::{JobReport, RunReport};
use super::{CompletedJob, ExecError, Shared, aggregate, job};
use crate::progress::{ProgressEvent, ProgressSink};

struct GroupResult {
    id: String,
    completed: CompletedJob,
    reports: Vec<JobReport>,
}

#[derive(Clone, Default)]
struct ConcurrencyCoordinator {
    groups: Arc<AsyncMutex<HashMap<String, ConcurrencyGroup>>>,
}

struct ConcurrencyGroup {
    gate: Arc<Semaphore>,
    owner: Option<crate::Cancellation>,
}

#[derive(Clone)]
struct ScheduleLimits {
    workers: Arc<Semaphore>,
    machine_workers: Option<Arc<super::worker_pool::MachineWorkerPool>>,
    concurrency: ConcurrencyCoordinator,
}

impl ConcurrencyCoordinator {
    async fn enter(
        &self,
        key: &str,
        cancel_in_progress: bool,
        owner: &crate::Cancellation,
        run_cancellation: &crate::Cancellation,
    ) -> Result<OwnedSemaphorePermit, ExecError> {
        let (gate, immediate) = {
            let mut groups = self.groups.lock().await;
            let group = groups
                .entry(key.to_string())
                .or_insert_with(|| ConcurrencyGroup {
                    gate: Arc::new(Semaphore::new(1)),
                    owner: None,
                });
            if cancel_in_progress && let Some(current) = &group.owner {
                current.cancel();
            }
            let gate = Arc::clone(&group.gate);
            let immediate = Arc::clone(&gate).try_acquire_owned().ok();
            if immediate.is_some() {
                group.owner = Some(owner.clone());
            }
            (gate, immediate)
        };
        if let Some(permit) = immediate {
            return Ok(permit);
        }
        let permit = tokio::select! {
            permit = gate.acquire_owned() => permit.map_err(|_| ExecError::Infrastructure {
                message: format!("concurrency group '{key}' closed before the job started"),
                fix: "retry the run".to_string(),
            })?,
            () = run_cancellation.cancelled() => {
                return Err(ExecError::Infrastructure {
                    message: "run cancellation reached a queued concurrency group".to_string(),
                    fix: "retry the run when ready".to_string(),
                });
            }
        };
        let mut groups = self.groups.lock().await;
        if let Some(group) = groups.get_mut(key) {
            group.owner = Some(owner.clone());
        }
        Ok(permit)
    }
}

#[derive(Clone)]
struct SharedWriter<'a> {
    inner: Arc<Mutex<&'a mut (dyn Write + Send)>>,
}

impl Write for SharedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut writer = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("run log writer lock was poisoned"))?;
        writer.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut writer = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("run log writer lock was poisoned"))?;
        writer.flush()
    }
}

#[derive(Clone)]
struct SharedProgress<'a> {
    inner: Arc<Mutex<&'a mut (dyn ProgressSink + Send)>>,
}

impl ProgressSink for SharedProgress<'_> {
    fn on_progress(&mut self, event: ProgressEvent) {
        if let Ok(mut sink) = self.inner.lock() {
            sink.on_progress(event);
        }
    }
}

pub(super) async fn run(
    shared: &Shared<'_>,
    groups: &[JobGroup<'_>],
    baseline_masker: &Masker,
    out: &mut (dyn Write + Send),
    progress: &mut (dyn ProgressSink + Send),
) -> Result<RunReport, ExecError> {
    let writer = SharedWriter {
        inner: Arc::new(Mutex::new(out)),
    };
    let progress = SharedProgress {
        inner: Arc::new(Mutex::new(progress)),
    };
    let workers = std::thread::available_parallelism()
        .map_or(2, |count| count.get())
        .max(2);
    let per_run_workers = if workers == 1 { 1 } else { workers - 1 };
    let worker_limit = Arc::new(Semaphore::new(per_run_workers));
    let machine_workers = match shared.config.store.as_ref() {
        Some(store) => Some(Arc::new(
            super::worker_pool::MachineWorkerPool::open(&store.toolcache_root_parent(), workers)
                .await?,
        )),
        None => None,
    };
    let concurrency = ConcurrencyCoordinator::default();
    let limits = ScheduleLimits {
        workers: worker_limit,
        machine_workers,
        concurrency,
    };
    let mut completed = HashMap::new();
    let mut reports_by_group: HashMap<String, Vec<JobReport>> = HashMap::new();
    let max_wave = groups.iter().map(|group| group.wave).max().unwrap_or(0);

    for wave in 0..=max_wave {
        let wave_dependencies = completed.clone();
        let mut running = FuturesUnordered::new();
        for group in groups.iter().filter(|group| group.wave == wave) {
            running.push(run_group(
                shared,
                group,
                &wave_dependencies,
                baseline_masker,
                writer.clone(),
                progress.clone(),
                limits.clone(),
            ));
        }

        let mut first_error = None;
        while let Some(result) = running.next().await {
            match result {
                Ok(group) => {
                    reports_by_group.insert(group.id.clone(), group.reports);
                    completed.insert(group.id, group.completed);
                }
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
    }

    let mut reports = Vec::new();
    for group in groups {
        if let Some(mut group_reports) = reports_by_group.remove(&group.id.0) {
            reports.append(&mut group_reports);
        }
    }
    let overall = RunReport::overall_of(&reports);
    Ok(RunReport {
        jobs: reports,
        overall,
    })
}

async fn run_group(
    shared: &Shared<'_>,
    group: &JobGroup<'_>,
    completed: &HashMap<String, CompletedJob>,
    baseline_masker: &Masker,
    writer: SharedWriter<'_>,
    progress: SharedProgress<'_>,
    limits: ScheduleLimits,
) -> Result<GroupResult, ExecError> {
    let needs = Arc::new(needs_records(group.needs, completed));
    let materialized = super::instance::materialize(group, shared.roots, needs.as_slice())?;
    let group_fail_fast = materialized.fail_fast;
    let group_max_parallel = materialized.max_parallel;
    let group_total = materialized.instances.len();
    let mut running = FuturesUnordered::new();
    let make_leg = |index, instance| {
        let mut masker = baseline_masker.clone();
        let mut instance_writer = writer.clone();
        let mut instance_progress = progress.clone();
        let workers = Arc::clone(&limits.workers);
        let machine_workers = limits.machine_workers.clone();
        let instance_needs = Arc::clone(&needs);
        let cancellation = shared.cancellation.clone();
        let coordinator = limits.concurrency.clone();
        let instance_key = format!("{}-{index:03}", group.id.0);
        async move {
            if cancellation.is_cancelled() {
                return Ok((index, cancelled_report(&group.id, instance, &masker)));
            }
            let job_cancellation = crate::Cancellation::new();
            let concurrency_policy = super::instance::materialize_concurrency(
                instance,
                shared.roots,
                instance_needs.as_slice(),
                group_fail_fast,
                std::num::NonZeroU32::new(u32::try_from(group_max_parallel).unwrap_or(u32::MAX)),
                index,
                group_total,
            )?;
            let concurrency_permit = match concurrency_policy {
                Some((key, cancel_in_progress)) => {
                    match coordinator
                        .enter(&key, cancel_in_progress, &job_cancellation, &cancellation)
                        .await
                    {
                        Ok(permit) => Some(permit),
                        Err(_) if cancellation.is_cancelled() => {
                            return Ok((index, cancelled_report(&group.id, instance, &masker)));
                        }
                        Err(error) => return Err(error),
                    }
                }
                None => None,
            };
            let worker_permit = tokio::select! {
                permit = workers.acquire_owned() => permit.map_err(|_| ExecError::Infrastructure {
                        message: "the run worker pool closed before a ready job started"
                            .to_string(),
                        fix: "retry the run".to_string(),
                    })?,
                () = cancellation.cancelled() => {
                    return Ok((
                        index,
                        cancelled_report(&group.id, instance, &masker),
                    ));
                }
            };
            let machine_worker_permit = match machine_workers {
                Some(pool) => match pool.acquire(&cancellation).await {
                    Ok(permit) => Some(permit),
                    Err(_) if cancellation.is_cancelled() => {
                        return Ok((index, cancelled_report(&group.id, instance, &masker)));
                    }
                    Err(error) => return Err(error),
                },
                None => None,
            };
            let job_shared = Shared {
                engine: shared.engine,
                config: shared.config,
                roots: shared.roots,
                workflow_env: shared.workflow_env,
                cancellation: &job_cancellation,
                namespace: shared.namespace,
            };
            let result = job::run_instance(
                &job_shared,
                &mut masker,
                instance,
                job::JobIdentity {
                    id: &group.id,
                    instance_key: &instance_key,
                    matrix_index: index,
                },
                instance_needs.as_slice(),
                &mut instance_writer,
                &mut instance_progress,
            );
            tokio::pin!(result);
            let result = tokio::select! {
                result = &mut result => result,
                () = cancellation.cancelled() => {
                    job_cancellation.cancel();
                    result.await
                }
            };
            drop(worker_permit);
            drop(machine_worker_permit);
            drop(concurrency_permit);
            result.map(|report| (index, report))
        }
    };
    let mut pending = materialized.instances.iter().enumerate();
    for _ in 0..materialized.max_parallel.max(1) {
        let Some((index, instance)) = pending.next() else {
            break;
        };
        running.push(make_leg(index, instance));
    }

    let mut indexed = Vec::with_capacity(materialized.instances.len());
    let mut first_error = None;
    let mut fail_fast_triggered = false;
    while let Some(result) = running.next().await {
        match result {
            Ok(report) => {
                fail_fast_triggered |= materialized.fail_fast
                    && matches!(report.1.result, Conclusion::Failure | Conclusion::Cancelled);
                indexed.push(report);
            }
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
        if !fail_fast_triggered
            && first_error.is_none()
            && let Some((index, instance)) = pending.next()
        {
            running.push(make_leg(index, instance));
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    if fail_fast_triggered {
        indexed.extend(pending.map(|(index, instance)| {
            (
                index,
                cancelled_report(&group.id, instance, baseline_masker),
            )
        }));
    }
    let instance_results = indexed
        .iter()
        .map(|(_, report)| (report.result, report.outputs.clone()))
        .collect::<Vec<_>>();
    indexed.sort_by_key(|(index, _)| *index);
    let reports: Vec<JobReport> = indexed.into_iter().map(|(_, report)| report).collect();
    let aggregated = aggregate(&instance_results);
    let ancestors_failed = group
        .needs
        .iter()
        .any(|need| completed.get(&need.0).is_some_and(|done| done.chain_failed));
    let ancestors_cancelled = group.needs.iter().any(|need| {
        completed
            .get(&need.0)
            .is_some_and(|done| done.chain_cancelled)
    });
    let chain_failed = ancestors_failed || matches!(aggregated.0, Conclusion::Failure);
    let chain_cancelled = ancestors_cancelled || matches!(aggregated.0, Conclusion::Cancelled);

    Ok(GroupResult {
        id: group.id.0.clone(),
        completed: CompletedJob {
            result: aggregated.0,
            outputs: aggregated.1,
            chain_failed,
            chain_cancelled,
        },
        reports,
    })
}

fn cancelled_report(
    job_id: &JobId,
    instance: &super::instance::JobInstance<'_>,
    masker: &Masker,
) -> JobReport {
    let display = match &instance.display.evaluation {
        Evaluation::Static(value) => value,
        Evaluation::Deferred(_) => &instance.display.source,
    };
    JobReport {
        id: job_id.0.clone(),
        display: masker.apply(display),
        result: Conclusion::Cancelled,
        steps: Vec::new(),
        outputs: indexmap::IndexMap::new(),
        duration: Duration::ZERO,
        container_id: None,
    }
}

fn needs_records(needs: &[JobId], completed: &HashMap<String, CompletedJob>) -> Vec<NeedRecord> {
    needs
        .iter()
        .filter_map(|need| {
            completed.get(&need.0).map(|done| NeedRecord {
                job: need.clone(),
                result: done.result,
                outputs: done.outputs.clone(),
                chain_failed: done.chain_failed,
                chain_cancelled: done.chain_cancelled,
            })
        })
        .collect()
}
