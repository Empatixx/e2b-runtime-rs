use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use futures::stream::BoxStream;
use sha2::{Digest, Sha256};

pub struct Verified {
    inner: BoxStream<'static, io::Result<Bytes>>,
    sha: Sha256,
    read: u64,
    expected_size: u64,
    expected_digest: String,
    done: bool,
}

impl Verified {
    pub fn new(inner: BoxStream<'static, io::Result<Bytes>>, expected_digest: &str, expected_size: u64) -> Self {
        Self {
            inner,
            sha: Sha256::new(),
            read: 0,
            expected_size,
            expected_digest: expected_digest.to_owned(),
            done: false,
        }
    }

    fn finish(&mut self) -> Option<io::Error> {
        self.done = true;
        let got = format!("sha256:{:x}", std::mem::take(&mut self.sha).finalize());
        match (self.read == self.expected_size, got == self.expected_digest) {
            (false, _) => Some(io::Error::other(format!(
                "error verifying size; got {}, want {}",
                self.read, self.expected_size
            ))),
            (true, false) => Some(io::Error::other(format!(
                "error verifying sha256 checksum after reading {} bytes; got {}, want {}",
                self.read,
                crate::go_strconv::quote(&got),
                crate::go_strconv::quote(&self.expected_digest)
            ))),
            (true, true) => None,
        }
    }
}

impl Stream for Verified {
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.sha.update(&chunk);
                self.read += chunk.len() as u64;
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(None) => Poll::Ready(self.finish().map(Err)),
            other => other,
        }
    }
}

pub fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use futures::{StreamExt, stream};

    use super::*;

    async fn collect(chunks: Vec<&'static [u8]>, digest: &str, size: u64) -> Vec<io::Result<Bytes>> {
        let inner = stream::iter(chunks.into_iter().map(|chunk| Ok(Bytes::from_static(chunk)))).boxed();
        Verified::new(inner, digest, size).collect().await
    }

    #[tokio::test]
    async fn good_blobs_pass_and_bad_ones_fail_at_the_end() {
        let digest = digest_of(b"hello");

        let good = collect(vec![b"he", b"llo"], &digest, 5).await;
        let short = collect(vec![b"he"], &digest, 5).await;
        let wrong = collect(vec![b"jello"], &digest, 5).await;

        assert!(good.iter().all(Result::is_ok));
        assert_eq!(
            short.last().unwrap().as_ref().unwrap_err().to_string(),
            "error verifying size; got 2, want 5"
        );
        assert!(
            wrong
                .last()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .to_string()
                .starts_with("error verifying sha256 checksum after reading 5 bytes")
        );
    }
}
