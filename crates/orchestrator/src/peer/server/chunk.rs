use bytes::{Bytes, BytesMut};
use futures::stream::{self, BoxStream};
use futures::{Stream, StreamExt};

use super::source::{Chunks, SourceError};

pub const SEND_CHUNK_SIZE: usize = 1 << 20;

pub fn chunked(data: Bytes) -> Chunks {
    stream::unfold(Some(data), |remaining| async move {
        let mut data = remaining?;
        let take = data.len().min(SEND_CHUNK_SIZE);
        let chunk = data.split_to(take);
        Some((Ok(chunk), (!data.is_empty()).then_some(data)))
    })
    .boxed()
}

pub fn rechunked<S>(pieces: S) -> Chunks
where
    S: Stream<Item = Result<Bytes, SourceError>> + Send + 'static,
{
    let rechunk = Rechunk {
        pieces: pieces.boxed(),
        buffer: BytesMut::new(),
        finished: false,
    };
    stream::unfold(rechunk, |mut rechunk| async move {
        rechunk.next_chunk().await.map(|chunk| (chunk, rechunk))
    })
    .boxed()
}

struct Rechunk {
    pieces: BoxStream<'static, Result<Bytes, SourceError>>,
    buffer: BytesMut,
    finished: bool,
}

impl Rechunk {
    async fn next_chunk(&mut self) -> Option<Result<Bytes, SourceError>> {
        while !self.finished && self.buffer.len() < SEND_CHUNK_SIZE {
            match self.pieces.next().await {
                Some(Ok(piece)) => self.buffer.extend_from_slice(&piece),
                Some(Err(e)) => {
                    self.finished = true;
                    return Some(Err(e));
                }
                None => self.finished = true,
            }
        }
        let take = self.buffer.len().min(SEND_CHUNK_SIZE);
        (take > 0).then(|| Ok(self.buffer.split_to(take).freeze()))
    }
}

#[cfg(test)]
mod tests {
    use futures::TryStreamExt;
    use prost::Message;

    use super::*;
    use crate::proto::GetBuildBlobResponse;

    const GRPC_DEFAULT_MAX_RECV_MSG_SIZE: usize = 4 * 1024 * 1024;
    const STORAGE_MEMORY_CHUNK_SIZE: usize = 4 * 1024 * 1024;

    async fn sizes(chunks: Chunks) -> Vec<usize> {
        chunks.map_ok(|chunk| chunk.len()).try_collect().await.unwrap()
    }

    async fn joined(chunks: Chunks) -> Vec<u8> {
        chunks
            .try_fold(Vec::new(), |mut all, chunk| async move {
                all.extend_from_slice(&chunk);
                Ok(all)
            })
            .await
            .unwrap()
    }

    #[test]
    fn a_chunk_encodes_under_the_grpc_default_receive_limit() {
        let encoded = GetBuildBlobResponse {
            data: vec![0; SEND_CHUNK_SIZE],
            availability: None,
        }
        .encoded_len();

        assert!(encoded > SEND_CHUNK_SIZE);
        assert!(encoded <= GRPC_DEFAULT_MAX_RECV_MSG_SIZE);
    }

    #[test]
    fn a_storage_chunk_overflows_the_grpc_default_receive_limit_once_encoded() {
        let encoded = GetBuildBlobResponse {
            data: vec![0; STORAGE_MEMORY_CHUNK_SIZE],
            availability: None,
        }
        .encoded_len();

        assert!(encoded > GRPC_DEFAULT_MAX_RECV_MSG_SIZE);
    }

    #[tokio::test]
    async fn chunked_bounds_every_send() {
        let tail = SEND_CHUNK_SIZE / 2;
        let payload = Bytes::from(vec![b'x'; 2 * SEND_CHUNK_SIZE + tail]);

        let chunks = chunked(payload.clone());

        assert_eq!(sizes(chunks).await, vec![SEND_CHUNK_SIZE, SEND_CHUNK_SIZE, tail]);
        assert_eq!(joined(chunked(payload.clone())).await, payload.to_vec());
    }

    #[tokio::test]
    async fn chunked_exact_multiple_does_not_send_an_empty_tail() {
        let payload = Bytes::from(vec![b'x'; 2 * SEND_CHUNK_SIZE]);

        assert_eq!(sizes(chunked(payload)).await, vec![SEND_CHUNK_SIZE, SEND_CHUNK_SIZE]);
    }

    #[tokio::test]
    async fn chunked_small_payload_is_one_send() {
        assert_eq!(joined(chunked(Bytes::from_static(b"small"))).await, b"small");
        assert_eq!(sizes(chunked(Bytes::from_static(b"small"))).await, vec![5]);
    }

    #[tokio::test]
    async fn chunked_empty_payload_still_sends() {
        assert_eq!(sizes(chunked(Bytes::new())).await, vec![0]);
    }

    #[tokio::test]
    async fn chunked_stops_at_the_first_chunk_the_consumer_does_not_pull() {
        let payload = Bytes::from(vec![b'x'; 3 * SEND_CHUNK_SIZE]);

        let pulled = chunked(payload).take(2).try_collect::<Vec<_>>().await.unwrap();

        assert_eq!(pulled.len(), 2);
    }

    #[tokio::test]
    async fn rechunked_bounds_every_send_whatever_the_piece_sizes() {
        let payload = vec![b'y'; 2 * SEND_CHUNK_SIZE + 7];
        let pieces = stream::iter(
            payload
                .chunks(7 * 1024)
                .map(|piece| Ok(Bytes::copy_from_slice(piece)))
                .collect::<Vec<_>>(),
        );

        let chunks = rechunked(pieces);

        assert_eq!(sizes(chunks).await, vec![SEND_CHUNK_SIZE, SEND_CHUNK_SIZE, 7]);
    }

    #[tokio::test]
    async fn rechunked_keeps_every_byte() {
        let payload = vec![b'z'; SEND_CHUNK_SIZE + 5];
        let pieces = stream::iter(
            payload
                .chunks(1000)
                .map(|piece| Ok(Bytes::copy_from_slice(piece)))
                .collect::<Vec<_>>(),
        );

        assert_eq!(joined(rechunked(pieces)).await, payload);
    }

    #[tokio::test]
    async fn rechunked_nothing_sends_nothing() {
        assert!(sizes(rechunked(stream::empty())).await.is_empty());
    }

    #[tokio::test]
    async fn rechunked_passes_a_piece_error_through() {
        let pieces = stream::iter(vec![Err(SourceError::Failed("read failed".to_owned()))]);

        let error = rechunked(pieces).try_collect::<Vec<_>>().await.unwrap_err();

        assert_eq!(error, SourceError::Failed("read failed".to_owned()));
    }
}
