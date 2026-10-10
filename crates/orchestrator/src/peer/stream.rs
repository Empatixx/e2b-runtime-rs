use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use tonic::{Code, Response, Status, Streaming};

use super::availability::served_by_peer;
use crate::proto::{GetBuildBlobResponse, PeerAvailability, ReadAtBuildSeekableResponse};

pub trait ChunkMessage {
    fn into_parts(self) -> (Vec<u8>, Option<PeerAvailability>);
}

impl ChunkMessage for GetBuildBlobResponse {
    fn into_parts(self) -> (Vec<u8>, Option<PeerAvailability>) {
        (self.data, self.availability)
    }
}

impl ChunkMessage for ReadAtBuildSeekableResponse {
    fn into_parts(self) -> (Vec<u8>, Option<PeerAvailability>) {
        (self.data, self.availability)
    }
}

pub fn status_text(status: &Status) -> String {
    let code = match status.code() {
        Code::Cancelled => "Canceled".to_owned(),
        code => format!("{code:?}"),
    };
    format!("rpc error: code = {code} desc = {}", status.message())
}

pub struct PeerChunks<M> {
    stream: Streaming<M>,
    first: Vec<u8>,
    uploaded: Arc<AtomicBool>,
}

impl<M: ChunkMessage + Send + 'static> PeerChunks<M> {
    pub async fn open(
        opened: Result<Response<Streaming<M>>, Status>,
        kind: &str,
        uploaded: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let mut stream = opened
            .map_err(|status| format!("open {kind} stream: {}", status_text(&status)))?
            .into_inner();
        let first = stream
            .message()
            .await
            .map_err(|status| format!("recv first {kind} message: {}", status_text(&status)))?
            .ok_or_else(|| format!("recv first {kind} message: EOF"))?;
        let (data, availability) = first.into_parts();
        match served_by_peer(availability.as_ref(), &uploaded) {
            true => Ok(Self {
                stream,
                first: data,
                uploaded,
            }),
            false => Err(format!("peer not available for {kind} stream")),
        }
    }

    pub fn into_stream(self) -> BoxStream<'static, Result<Bytes, String>> {
        stream::try_unfold(self, |mut chunks| async move {
            chunks.next().await.map(|data| data.map(|data| (data, chunks)))
        })
        .boxed()
    }

    async fn next(&mut self) -> Result<Option<Bytes>, String> {
        loop {
            if !self.first.is_empty() {
                return Ok(Some(Bytes::from(std::mem::take(&mut self.first))));
            }
            let message = self
                .stream
                .message()
                .await
                .map_err(|status| format!("failed to receive chunk from peer: {}", status_text(&status)))?;
            let Some(message) = message else { return Ok(None) };
            let (data, availability) = message.into_parts();
            served_by_peer(availability.as_ref(), &self.uploaded);
            if !data.is_empty() {
                return Ok(Some(Bytes::from(data)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_reads_like_the_go_grpc_error() {
        assert_eq!(
            status_text(&Status::unavailable("connection refused")),
            "rpc error: code = Unavailable desc = connection refused"
        );
        assert_eq!(
            status_text(&Status::cancelled("gone")),
            "rpc error: code = Canceled desc = gone"
        );
    }
}
