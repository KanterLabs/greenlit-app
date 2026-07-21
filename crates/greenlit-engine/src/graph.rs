//! The `needs` dependency graph: job identity, cycle detection, and the
//! deterministic topological/"wave" order used for both rendering and (in
//! later phases) scheduling.
//!
//! Source: design memo §2 ("The `needs` graph: types, cycle detection,
//! topological order"), itself transcribing the docs' `needs` semantics —
//! "a string or array of strings" naming "jobs that must complete
//! successfully before this job will run"; "if a job fails or is skipped,
//! all jobs that need it are skipped unless the jobs use a conditional
//! expression that causes the job to continue" (see
//! [Using jobs in a workflow](https://docs.github.com/en/actions/writing-workflows/choosing-what-your-workflow-does/using-jobs-in-a-workflow)).
//! Jobs without `needs` run in parallel; for a matrix job, `needs` is
//! job-granular (every leg of the needed job must complete before any leg
//! of the dependent starts) — this graph stays at job granularity, matching
//! the design memo's explicit instruction: "Keep the plan graph at job
//! granularity; leg fan-out happens inside a job node."

use std::collections::HashMap;

use greenlit_workflow::model::job::Job;

/// A job's identifier — the `jobs:` mapping key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct JobId(pub String);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&str> for JobId {
    fn from(s: &str) -> Self {
        JobId(s.to_string())
    }
}

/// A dense index into [`JobGraph`]'s declaration-order job list — a cheap
/// `Copy` handle used everywhere internally instead of cloning [`JobId`]
/// strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobIdx(pub u32);

/// The `needs` graph, built once per plan.
///
/// `JobIdx` values are dense indices into declaration order (the order jobs
/// appear in the workflow file) — design memo §2.2: "this order is the
/// deterministic tie-break for everything downstream."
#[derive(Debug, Clone)]
pub struct JobGraph {
    ids: Vec<JobId>,
    by_id: HashMap<JobId, JobIdx>,
    /// `deps[j]` = the jobs `j` needs (edge `j -> dep`), deduplicated,
    /// preserving the `needs:` list's declaration order.
    deps: Vec<Vec<JobIdx>>,
    /// Reverse adjacency: `dependents[j]` = jobs that need `j`.
    dependents: Vec<Vec<JobIdx>>,
    /// The unique topological order that is lexicographically smallest by
    /// declaration index (design memo §2.3 step 3) — only populated when
    /// the graph is acyclic.
    topo_order: Vec<JobIdx>,
    /// `wave[j] = 0` if `deps[j]` is empty, else `1 + max(wave[d])` over
    /// `d` in `deps[j]` — longest-path depth (design memo §2.3 step 4).
    wave: Vec<u32>,
}

/// Everything that can go wrong building a [`JobGraph`] from a workflow's
/// `jobs:` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    /// A job's `needs:` entry names a job id that isn't defined anywhere in
    /// the workflow.
    UnknownNeed {
        /// The job whose `needs:` list has the bad reference.
        job: JobId,
        /// The unknown job id it referenced.
        needs: JobId,
    },
    /// One or more dependency cycles were found — every disjoint cycle is
    /// reported at once (design memo §2.3 step 1: "better UX than
    /// fix-one-rerun-find-next"). Each [`DependencyCycle`]'s own `Display`
    /// renders the full `a -> b -> c -> a` walk, and this error's own
    /// `Display` joins every disjoint named cycle into one diagnostic.
    Cycles(Vec<DependencyCycle>),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphError::UnknownNeed { job, needs } => {
                write!(f, "job '{job}' needs unknown job '{needs}'")
            }
            GraphError::Cycles(cycles) => {
                for (index, cycle) in cycles.iter().enumerate() {
                    if index > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{cycle}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for GraphError {}

/// One dependency cycle: a closed walk in the "needs" direction
/// (`members[i]` needs `members[(i + 1) % len]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyCycle {
    /// The cycle's members, in canonical walk order starting at the
    /// lexicographically smallest [`JobId`] — a self-dependency is a
    /// 1-cycle (`members = [j]`).
    pub members: Vec<JobId>,
}

impl std::fmt::Display for DependencyCycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dependency cycle: ")?;
        for (i, m) in self.members.iter().enumerate() {
            if i > 0 {
                write!(f, " -> ")?;
            }
            write!(f, "{m}")?;
        }
        if let Some(first) = self.members.first() {
            write!(f, " -> {first}")?;
        }
        Ok(())
    }
}

impl JobGraph {
    /// The jobs in declaration order.
    #[must_use]
    pub fn job_ids(&self) -> &[JobId] {
        &self.ids
    }

    /// Looks up the dense index for a job id.
    #[must_use]
    pub fn idx_of(&self, id: &JobId) -> Option<JobIdx> {
        self.by_id.get(id).copied()
    }

    /// The job id a dense index refers to.
    #[must_use]
    pub fn id_of(&self, idx: JobIdx) -> &JobId {
        &self.ids[idx.0 as usize]
    }

    /// The jobs `idx` needs (edges `idx -> dep`), in `needs:` declaration
    /// order, deduplicated.
    #[must_use]
    pub fn deps_of(&self, idx: JobIdx) -> &[JobIdx] {
        &self.deps[idx.0 as usize]
    }

    /// The jobs that need `idx`.
    #[must_use]
    pub fn dependents_of(&self, idx: JobIdx) -> &[JobIdx] {
        &self.dependents[idx.0 as usize]
    }

    /// The deterministic topological order (design memo §2.3 step 3):
    /// Kahn's algorithm with a min-heap keyed by declaration index, giving
    /// the unique topo order that is lexicographically smallest by
    /// declaration order.
    #[must_use]
    pub fn topo_order(&self) -> &[JobIdx] {
        &self.topo_order
    }

    /// `wave(j)`: `0` if `j` has no dependencies, else `1 + max` of its
    /// dependencies' waves — how the human tree shows parallelism.
    #[must_use]
    pub fn wave(&self, idx: JobIdx) -> u32 {
        self.wave[idx.0 as usize]
    }

    /// How many jobs are in the graph.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether the graph has no jobs at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

/// Builds the `needs` graph from a workflow's jobs, validating every
/// `needs:` reference and rejecting any dependency cycle (design memo §2.1
/// "Validation errors").
pub fn build_graph(jobs: &[Job]) -> Result<JobGraph, GraphError> {
    let ids: Vec<JobId> = jobs.iter().map(|j| JobId(j.id.value.clone())).collect();
    let by_id: HashMap<JobId, JobIdx> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.clone(), JobIdx(i as u32)))
        .collect();

    let mut deps: Vec<Vec<JobIdx>> = Vec::with_capacity(jobs.len());
    for job in jobs {
        let mut seen = std::collections::HashSet::new();
        let mut this_deps = Vec::new();
        for need in &job.needs {
            let need_id = JobId(need.value.clone());
            let Some(&idx) = by_id.get(&need_id) else {
                return Err(GraphError::UnknownNeed {
                    job: JobId(job.id.value.clone()),
                    needs: need_id,
                });
            };
            // Duplicate entries in one `needs:` list are a lint concern
            // (design memo §2.1), deduplicated here at the graph-edge
            // level; the lint itself is emitted by `crate::plan` while it
            // still has the raw job model in hand (it can see the actual
            // duplicated span there, which this graph representation no
            // longer carries).
            if seen.insert(idx) {
                this_deps.push(idx);
            }
        }
        deps.push(this_deps);
    }

    let mut dependents: Vec<Vec<JobIdx>> = vec![Vec::new(); jobs.len()];
    for (j, ds) in deps.iter().enumerate() {
        for &d in ds {
            dependents[d.0 as usize].push(JobIdx(j as u32));
        }
    }

    let cycles = find_cycles(&ids, &deps);
    if !cycles.is_empty() {
        return Err(GraphError::Cycles(cycles));
    }

    let topo_order = kahn_topo_order(&deps, &dependents);
    let wave = compute_waves(&topo_order, &deps);

    Ok(JobGraph {
        ids,
        by_id,
        deps,
        dependents,
        topo_order,
        wave,
    })
}

/// Iterative Tarjan's SCC (design memo §2.3 step 1), returning one
/// [`DependencyCycle`] per strongly-connected component of size >= 2, plus
/// every single node with a self-edge — "avoid recursion so a pathological
/// 10k-job file cannot overflow the stack."
///
/// The explicit `call_stack` holds `(node, next_child_cursor)` pairs,
/// mirroring the recursive algorithm's call frames one-for-one: pushing a
/// frame is a recursive call, advancing its cursor is the loop over a
/// node's adjacency list, and popping a fully-scanned frame is a return —
/// at which point its final `lowlink` is folded into the (now top-of-stack)
/// caller's, exactly as a `return`-then-`min()` would.
fn find_cycles(ids: &[JobId], deps: &[Vec<JobIdx>]) -> Vec<DependencyCycle> {
    let n = ids.len();
    let mut visited: Vec<bool> = vec![false; n];
    let mut index: Vec<u32> = vec![0; n];
    let mut lowlink: Vec<u32> = vec![0; n];
    let mut on_stack: Vec<bool> = vec![false; n];
    let mut tarjan_stack: Vec<usize> = Vec::new();
    let mut next_index: u32 = 0;
    let mut sccs: Vec<Vec<usize>> = Vec::new();

    for start in 0..n {
        if visited[start] {
            continue;
        }

        let mut call_stack: Vec<(usize, usize)> = vec![(start, 0)];
        visited[start] = true;
        index[start] = next_index;
        lowlink[start] = next_index;
        next_index += 1;
        tarjan_stack.push(start);
        on_stack[start] = true;

        while let Some(frame) = call_stack.last_mut() {
            let v = frame.0;
            if frame.1 < deps[v].len() {
                let w = deps[v][frame.1].0 as usize;
                frame.1 += 1;
                if !visited[w] {
                    visited[w] = true;
                    index[w] = next_index;
                    lowlink[w] = next_index;
                    next_index += 1;
                    tarjan_stack.push(w);
                    on_stack[w] = true;
                    call_stack.push((w, 0));
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(index[w]);
                }
            } else {
                call_stack.pop();
                if let Some(&(parent, _)) = call_stack.last() {
                    lowlink[parent] = lowlink[parent].min(lowlink[v]);
                }
                if lowlink[v] == index[v] {
                    let mut scc = Vec::new();
                    while let Some(w) = tarjan_stack.pop() {
                        on_stack[w] = false;
                        let is_root = w == v;
                        scc.push(w);
                        if is_root {
                            break;
                        }
                    }
                    sccs.push(scc);
                }
            }
        }
    }

    let mut cycles: Vec<DependencyCycle> = Vec::new();
    for scc in &sccs {
        let is_cycle = scc.len() >= 2
            || (scc.len() == 1 && deps[scc[0]].iter().any(|d| d.0 as usize == scc[0]));
        if !is_cycle {
            continue;
        }
        if let Some(cycle) = build_cycle_display(ids, deps, scc) {
            cycles.push(cycle);
        }
    }
    // Deterministic output order: by each cycle's canonical (smallest)
    // member id.
    cycles.sort_by(|a, b| a.members.first().cmp(&b.members.first()));
    cycles
}

/// Reconstructs one concrete witness loop for an SCC's member set (design
/// memo §2.3 step 2). An iterative depth-first search starts at the
/// lexicographically smallest member and finds a simple path back to that
/// start. Every adjacent pair therefore comes directly from `deps`; a
/// tempting greedy walk is incorrect because it can enter a smaller
/// sub-cycle that does not contain the canonical start.
fn build_cycle_display(
    ids: &[JobId],
    deps: &[Vec<JobIdx>],
    scc: &[usize],
) -> Option<DependencyCycle> {
    let first = *scc.first()?;
    let member_set: std::collections::HashSet<usize> = scc.iter().copied().collect();
    let start = *scc.iter().min_by_key(|&&node| &ids[node]).unwrap_or(&first);

    let mut path = vec![start];
    let mut cursors = vec![0usize];
    let mut on_path = std::collections::HashSet::from([start]);

    while let Some(&current) = path.last() {
        let mut neighbors: Vec<usize> = deps[current]
            .iter()
            .map(|dependency| dependency.0 as usize)
            .filter(|dependency| member_set.contains(dependency))
            .collect();
        neighbors.sort_by(|left, right| ids[*left].cmp(&ids[*right]));

        let cursor = cursors.last_mut()?;
        if *cursor >= neighbors.len() {
            let removed = path.pop()?;
            cursors.pop();
            on_path.remove(&removed);
            continue;
        }

        let next = neighbors[*cursor];
        *cursor += 1;
        if next == start {
            return Some(DependencyCycle {
                members: path.into_iter().map(|index| ids[index].clone()).collect(),
            });
        }
        if on_path.insert(next) {
            path.push(next);
            cursors.push(0);
        }
    }
    None
}

/// Kahn's algorithm with a min-heap keyed by declaration index (design memo
/// §2.3 step 3), producing the unique topo order that is lexicographically
/// smallest by declaration order. Only called once [`find_cycles`] has
/// confirmed the graph is acyclic, so it always fully drains.
fn kahn_topo_order(deps: &[Vec<JobIdx>], dependents: &[Vec<JobIdx>]) -> Vec<JobIdx> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let n = deps.len();
    let mut indegree: Vec<u32> = deps.iter().map(|d| d.len() as u32).collect();
    let mut heap: BinaryHeap<Reverse<u32>> = BinaryHeap::new();
    for (i, &deg) in indegree.iter().enumerate() {
        if deg == 0 {
            heap.push(Reverse(i as u32));
        }
    }

    let mut order = Vec::with_capacity(n);
    while let Some(Reverse(i)) = heap.pop() {
        order.push(JobIdx(i));
        for &dependent in &dependents[i as usize] {
            let d = dependent.0 as usize;
            indegree[d] -= 1;
            if indegree[d] == 0 {
                heap.push(Reverse(dependent.0));
            }
        }
    }
    order
}

/// `wave(j) = 0` if `deps[j]` is empty, else `1 + max(wave(d))` — one pass
/// over the topo order (design memo §2.3 step 4).
fn compute_waves(topo_order: &[JobIdx], deps: &[Vec<JobIdx>]) -> Vec<u32> {
    let mut wave = vec![0u32; deps.len()];
    for &idx in topo_order {
        let i = idx.0 as usize;
        wave[i] = deps[i]
            .iter()
            .map(|d| wave[d.0 as usize] + 1)
            .max()
            .unwrap_or(0);
    }
    wave
}
