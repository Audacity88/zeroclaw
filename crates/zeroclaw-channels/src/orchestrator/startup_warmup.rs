//! Optional connection probes owned by one channel startup generation.

use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

const CONCURRENCY: usize = 2;
const TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct StartupWarmups {
    jobs: Vec<AbortOnDropHandle<()>>,
    permits: Arc<Semaphore>,
    cancel: CancellationToken,
}

impl StartupWarmups {
    pub(super) fn new(cancel: CancellationToken) -> Self {
        Self {
            jobs: Vec::new(),
            permits: Arc::new(Semaphore::new(CONCURRENCY)),
            cancel,
        }
    }

    pub(super) fn schedule(
        &mut self,
        agent: String,
        warmup: impl Future<Output = ()> + Send + 'static,
    ) {
        let permits = Arc::clone(&self.permits);
        let cancel = self.cancel.clone();
        // Retain abort-on-drop ownership: startup errors and supervisor aborts
        // must not leave probes running against a retired configuration.
        let job = zeroclaw_spawn::spawn!(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {}
                () = async {
                    let Ok(_permit) = permits.acquire_owned().await else {
                        return;
                    };
                    if tokio::time::timeout(TIMEOUT, warmup).await.is_err() {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                                .with_attrs(::serde_json::json!({"agent": agent})),
                            "ModelProvider startup warmup timed out (non-fatal)"
                        );
                    }
                } => {}
            }
        });
        self.jobs.push(AbortOnDropHandle::new(job));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Active(Arc<AtomicUsize>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn stalled_warmups_do_not_block_scheduling_and_cancel_releases_them() {
        let cancel = CancellationToken::new();
        let mut warmups = StartupWarmups::new(cancel.clone());
        let active = Arc::new(AtomicUsize::new(0));
        for _ in 0..4 {
            let active = Arc::clone(&active);
            warmups.schedule("test-agent".into(), async move {
                active.fetch_add(1, Ordering::SeqCst);
                let _active = Active(active);
                std::future::pending::<()>().await;
            });
        }
        tokio::task::yield_now().await;
        assert_eq!(active.load(Ordering::SeqCst), CONCURRENCY);
        cancel.cancel();
        for job in warmups.jobs.drain(..) {
            job.await.expect("warmup exits on cancellation");
        }
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn dropping_startup_owner_aborts_outstanding_probes() {
        let mut warmups = StartupWarmups::new(CancellationToken::new());
        let active = Arc::new(AtomicUsize::new(0));
        let probe_active = Arc::clone(&active);
        warmups.schedule("test-agent".into(), async move {
            probe_active.fetch_add(1, Ordering::SeqCst);
            let _active = Active(probe_active);
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(active.load(Ordering::SeqCst), 1);
        drop(warmups);
        tokio::task::yield_now().await;
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_probes_release_capacity_for_queued_work() {
        let mut warmups = StartupWarmups::new(CancellationToken::new());
        let completed = Arc::new(AtomicUsize::new(0));
        for _ in 0..CONCURRENCY {
            warmups.schedule("test-agent".into(), std::future::pending());
        }
        let count = Arc::clone(&completed);
        warmups.schedule("queued-agent".into(), async move {
            count.fetch_add(1, Ordering::SeqCst);
        });
        tokio::task::yield_now().await;
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        tokio::time::advance(TIMEOUT).await;
        for job in warmups.jobs.drain(..) {
            job.await.expect("warmup finishes after timeout");
        }
        assert_eq!(completed.load(Ordering::SeqCst), 1);
    }
}
