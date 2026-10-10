use std::sync::OnceLock;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::Mutex;

use crate::storage::build::Build;
use crate::storage::header::Header;

pub type DurableFuture = Shared<BoxFuture<'static, Result<Header, String>>>;

#[derive(Default)]
pub struct MemfileHeaders {
    swapped: Mutex<Option<Header>>,
    durable: OnceLock<DurableFuture>,
}

impl MemfileHeaders {
    pub fn with_durable(future: BoxFuture<'static, Result<Header, String>>) -> Self {
        let headers = Self::default();
        headers.set_durable(future);
        headers
    }

    pub fn set_durable(&self, future: BoxFuture<'static, Result<Header, String>>) {
        let _ = self.durable.set(future.shared());
    }

    pub fn swap(&self, next: Header) {
        *self.swapped.lock() = Some(next);
    }

    pub fn swap_if_current(&self, live: &Header, expected: &Header, next: Header) -> bool {
        let mut swapped = self.swapped.lock();
        let current = swapped.as_ref().unwrap_or(live);
        let matches = current == expected;
        if matches {
            *swapped = Some(next);
        }
        matches
    }

    pub fn apply(&self, mut build: Build) -> Build {
        if let Some(swapped) = self.swapped.lock().clone() {
            build.memfile = swapped;
        }
        build
    }

    pub fn current(&self, live: &Header) -> Header {
        self.swapped.lock().clone().unwrap_or_else(|| live.clone())
    }

    pub async fn durable(&self, live: &Header) -> Result<Header, String> {
        let current = self.current(live);
        if !current.incomplete_pending_upload {
            return Ok(current);
        }
        match self.durable.get() {
            Some(durable) => durable.clone().await,
            None => Ok(current),
        }
    }

    pub fn durable_now(&self, live: &Header) -> Option<Header> {
        let current = self.current(live);
        if !current.incomplete_pending_upload {
            return Some(current);
        }
        match self.durable.get() {
            None => Some(current),
            Some(durable) => durable.peek().cloned().and_then(Result::ok),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::oneshot;
    use uuid::Uuid;

    use super::*;
    use crate::storage::header::Metadata;

    fn header(size: u64, incomplete: bool) -> Header {
        Header {
            metadata: Metadata::template(Uuid::nil(), 4096, size),
            mappings: Vec::new(),
            builds: None,
            incomplete_pending_upload: incomplete,
        }
    }

    fn pending() -> (oneshot::Sender<Header>, MemfileHeaders) {
        let (sender, receiver) = oneshot::channel();
        let headers = MemfileHeaders::with_durable(async move { receiver.await.map_err(|e| e.to_string()) }.boxed());
        (sender, headers)
    }

    #[tokio::test]
    async fn the_live_header_is_returned_when_no_durable_future_is_set() {
        let live = header(1, true);

        let durable = MemfileHeaders::default().durable(&live).await.unwrap();

        assert_eq!(durable, live);
    }

    #[tokio::test]
    async fn an_incomplete_live_header_waits_for_the_durable_future() {
        let live = header(1, true);
        let (resolve, headers) = pending();
        let waiting = tokio::spawn({
            let live = live.clone();
            async move { headers.durable(&live).await }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "durable returned before the future resolved");
        let deduped = header(2, true);
        resolve.send(deduped.clone()).unwrap();

        let got = waiting.await.unwrap().unwrap();
        assert_eq!(got, deduped);
        assert_ne!(got, live);
    }

    #[tokio::test]
    async fn a_finalized_live_header_beats_the_stale_durable_future() {
        let deduped = header(2, true);
        let headers = MemfileHeaders::with_durable(futures::future::ready(Ok(deduped)).boxed());
        let finalized = header(3, false);

        let got = headers.durable(&finalized).await.unwrap();

        assert_eq!(got, finalized);
        assert_eq!(headers.durable_now(&finalized), Some(finalized));
    }

    #[test]
    fn swap_if_current_replaces_only_a_header_that_still_matches() {
        let headers = MemfileHeaders::default();
        let provisional = header(1, true);
        let deduped = header(2, true);
        let finalized = header(3, false);

        assert!(headers.swap_if_current(&provisional, &provisional, deduped.clone()));
        assert_eq!(headers.current(&provisional), deduped);

        headers.swap(finalized.clone());
        assert!(!headers.swap_if_current(&provisional, &provisional, deduped));
        assert_eq!(headers.current(&provisional), finalized, "the newer header is kept");
    }

    #[tokio::test]
    async fn durable_now_is_known_without_blocking_only_once_the_swap_settled() {
        let live = header(1, true);
        let headers = MemfileHeaders::default();
        assert_eq!(headers.durable_now(&live), Some(live.clone()));

        let (resolve, headers) = pending();
        assert_eq!(headers.durable_now(&live), None);

        let deduped = header(2, true);
        resolve.send(deduped.clone()).unwrap();
        let _ = headers.durable(&live).await;
        assert_eq!(headers.durable_now(&live), Some(deduped));
    }

    #[test]
    fn applying_swaps_the_memfile_header_of_a_build() {
        let headers = MemfileHeaders::default();
        let build = Build {
            build_id: Uuid::nil(),
            memfile: header(1, true),
            rootfs: header(9, false),
        };
        let deduped = header(2, true);

        assert_eq!(headers.apply(build.clone()), build);
        headers.swap(deduped.clone());

        let applied = headers.apply(build.clone());
        assert_eq!(applied.memfile, deduped);
        assert_eq!(applied.rootfs, build.rootfs);
    }
}
