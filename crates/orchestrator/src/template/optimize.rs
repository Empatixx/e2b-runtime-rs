use std::future::Future;

use super::metadata::MemoryPrefetch;
use crate::memory::trace::Trace;

pub const PREFETCH_ITERATIONS: usize = 2;

pub async fn collect_prefetch<F, Fut>(mut resume: F) -> Result<Option<MemoryPrefetch>, String>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<Trace, String>>,
{
    let mut traces = Vec::with_capacity(PREFETCH_ITERATIONS);
    for run in 1..=PREFETCH_ITERATIONS {
        tracing::debug!("starting sandbox run {run}/{PREFETCH_ITERATIONS} for prefetch collection");
        let trace = resume(run)
            .await
            .map_err(|e| format!("failed to collect prefetch data from run {run}: {e}"))?;
        traces.push(trace);
    }
    let common = Trace::intersect(&traces).map(|common| common.to_mapping());
    if common.is_none() {
        tracing::debug!("no common blocks found for prefetch mapping");
    }
    Ok(common)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::memory::trace::{Access, Touch};

    fn trace(indices: &[u64]) -> Trace {
        Trace {
            block_size: 4096,
            touches: indices
                .iter()
                .enumerate()
                .map(|(order, index)| {
                    (
                        *index,
                        Touch {
                            order: order as u64 + 1,
                            access: Access::Read,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[tokio::test]
    async fn two_runs_are_intersected_into_a_mapping() {
        let runs = [trace(&[5, 1, 9]), trace(&[5, 7, 1])];

        let mapping = collect_prefetch(|run| std::future::ready(Ok(runs[run - 1].clone())))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(mapping.indices, vec![5, 1]);
        assert_eq!(mapping.block_size, 4096);
    }

    #[tokio::test]
    async fn a_failed_run_fails_the_collection_and_disjoint_runs_give_none() {
        let failed = collect_prefetch(|run| {
            std::future::ready(if run == 1 {
                Ok(trace(&[1]))
            } else {
                Err("boom".to_owned())
            })
        })
        .await;
        assert_eq!(
            failed,
            Err("failed to collect prefetch data from run 2: boom".to_owned())
        );

        let disjoint = [trace(&[1]), trace(&[2])];
        assert_eq!(
            collect_prefetch(|run| std::future::ready(Ok(disjoint[run - 1].clone()))).await,
            Ok(None)
        );
    }
}
