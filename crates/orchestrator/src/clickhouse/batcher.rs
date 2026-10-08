use std::future::Future;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub const DEFAULT_QUEUE_SIZE: usize = 1000;
pub const DEFAULT_MAX_BATCH_SIZE: usize = 1000;
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatcherOptions {
    pub queue_size: usize,
    pub max_batch_size: usize,
    pub max_delay: Duration,
}

impl Default for BatcherOptions {
    fn default() -> Self {
        Self {
            queue_size: DEFAULT_QUEUE_SIZE,
            max_batch_size: DEFAULT_MAX_BATCH_SIZE,
            max_delay: DEFAULT_MAX_DELAY,
        }
    }
}

pub struct Batcher<T> {
    sender: mpsc::Sender<T>,
    flusher: JoinHandle<()>,
}

impl<T: Send + 'static> Batcher<T> {
    pub fn start<F, Fut>(name: &str, options: BatcherOptions, flush: F) -> Self
    where
        F: Fn(Vec<T>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel(options.queue_size.max(1));
        let name = name.to_owned();
        let flusher = tokio::spawn(run(name, options, receiver, flush));
        Self { sender, flusher }
    }

    pub fn push(&self, item: T) -> Result<(), String> {
        self.sender.try_send(item).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => "batcher queue is full".to_owned(),
            mpsc::error::TrySendError::Closed(_) => "batcher is not started".to_owned(),
        })
    }

    pub async fn stop(self) {
        drop(self.sender);
        let _ = self.flusher.await;
    }
}

async fn run<T, F, Fut>(name: String, options: BatcherOptions, mut receiver: mpsc::Receiver<T>, flush: F)
where
    F: Fn(Vec<T>) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let mut batch = Vec::new();
    let mut ticks = tokio::time::interval(options.max_delay);
    ticks.tick().await;
    loop {
        tokio::select! {
            item = receiver.recv() => match item {
                Some(item) => {
                    batch.push(item);
                    if batch.len() >= options.max_batch_size {
                        send(&name, &flush, &mut batch).await;
                        ticks.reset();
                    }
                }
                None => {
                    send(&name, &flush, &mut batch).await;
                    return;
                }
            },
            _ = ticks.tick() => send(&name, &flush, &mut batch).await,
        }
    }
}

async fn send<T, F, Fut>(name: &str, flush: &F, batch: &mut Vec<T>)
where
    F: Fn(Vec<T>) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    if batch.is_empty() {
        return;
    }
    if let Err(e) = flush(std::mem::take(batch)).await {
        tracing::error!(batcher = name, error = %e, "error batching items");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;

    type Flushed = Arc<Mutex<Vec<Vec<u32>>>>;

    fn flush_into(
        flushed: Flushed,
    ) -> impl Fn(Vec<u32>) -> std::future::Ready<Result<(), String>> + Send + Sync + 'static {
        move |batch| {
            flushed.lock().push(batch);
            std::future::ready(Ok(()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn batches_flush_at_the_size_limit_and_after_the_delay() {
        let flushed = Flushed::default();
        let flush = flush_into(flushed.clone());
        let options = BatcherOptions {
            queue_size: 10,
            max_batch_size: 3,
            max_delay: Duration::from_millis(1000),
        };
        let batcher = Batcher::start("test", options, flush);

        (1..=4).for_each(|item| batcher.push(item).unwrap());
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(flushed.lock().clone(), vec![vec![1, 2, 3]]);

        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert_eq!(flushed.lock().clone(), vec![vec![1, 2, 3], vec![4]]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_queue_drops_and_stop_flushes_the_rest() {
        let flushed = Flushed::default();
        let flush = flush_into(flushed.clone());
        let options = BatcherOptions {
            queue_size: 2,
            max_batch_size: 100,
            max_delay: Duration::from_secs(3600),
        };
        let batcher = Batcher::start("test", options, flush);

        batcher.push(1).unwrap();
        batcher.push(2).unwrap();
        assert_eq!(batcher.push(3), Err("batcher queue is full".to_owned()));
        batcher.stop().await;

        assert_eq!(flushed.lock().concat(), vec![1, 2]);
    }
}
