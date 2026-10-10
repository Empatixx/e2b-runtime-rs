use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;

use super::source::{Chunks, SourceError};
use crate::block::chunker::Chunker;

pub struct SeekableSource {
    pub chunker: Arc<Chunker>,
    pub block_size: usize,
}

fn blocks(data: Bytes, block_size: usize) -> Chunks {
    stream::unfold(data, move |mut data| async move {
        (!data.is_empty()).then(|| {
            let block = data.split_to(data.len().min(block_size));
            (Ok(block), data)
        })
    })
    .boxed()
}

impl SeekableSource {
    pub fn size(&self) -> u64 {
        self.chunker.size()
    }

    pub async fn stream(&self, offset: u64, length: u64) -> Result<Chunks, SourceError> {
        let mut data = vec![0; length as usize];
        self.chunker
            .read_at(offset, &mut data)
            .await
            .map_err(|e| SourceError::Failed(format!("slice diff at offset {offset}: {e}")))?;
        Ok(blocks(Bytes::from(data), self.block_size))
    }
}

#[cfg(test)]
mod tests {
    use futures::TryStreamExt;
    use uuid::Uuid;

    use crate::block::build_file::Artifact;
    use crate::peer::testing::cache_fixture;
    use crate::storage::header::{Header, Metadata};

    #[tokio::test]
    async fn size_is_the_size_of_the_cached_diff() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        fixture.add_diff(build_id, Artifact::Memfile, &[7; 8192]);

        let source = fixture
            .cache
            .resolve_seekable(&build_id.to_string(), "memfile")
            .await
            .unwrap();

        assert_eq!(source.size(), 8192);
    }

    #[tokio::test]
    async fn stream_sends_the_requested_range_of_the_diff() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        let content: Vec<u8> = (0..8192u32).map(|byte| (byte % 251) as u8).collect();
        fixture.add_diff(build_id, Artifact::Memfile, &content);
        let source = fixture
            .cache
            .resolve_seekable(&build_id.to_string(), "memfile")
            .await
            .unwrap();

        let chunks: Vec<_> = source.stream(4096, 4096).await.unwrap().try_collect().await.unwrap();

        assert_eq!(chunks.concat(), content[4096..]);
    }

    #[tokio::test]
    async fn stream_past_the_end_of_the_diff_fails_before_sending_anything() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        fixture.add_diff(build_id, Artifact::Rootfs, &[1; 4096]);
        let source = fixture
            .cache
            .resolve_seekable(&build_id.to_string(), "rootfs.ext4")
            .await
            .unwrap();

        let error = source.stream(4096, 4096).await.err().unwrap().to_string();

        assert!(error.starts_with("slice diff at offset 4096: "), "{error}");
    }

    #[tokio::test]
    async fn a_range_is_sent_one_page_per_message_when_the_build_header_is_not_loaded() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        fixture.add_diff(build_id, Artifact::Memfile, &[3; 8192]);
        let source = fixture
            .cache
            .resolve_seekable(&build_id.to_string(), "memfile")
            .await
            .unwrap();

        let chunks: Vec<_> = source.stream(0, 8192).await.unwrap().try_collect().await.unwrap();

        assert_eq!(
            chunks.iter().map(|chunk| chunk.len()).collect::<Vec<_>>(),
            vec![4096, 4096]
        );
    }

    #[tokio::test]
    async fn a_range_is_sent_one_block_per_message_with_the_block_size_of_the_cached_header() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        let memfile = Header::new(Metadata::template(build_id, 8192, 16384), Vec::new()).unwrap();
        fixture.add_template(build_id, memfile, &fixture.dir.path().join("snapfile"));
        fixture.add_diff(build_id, Artifact::Memfile, &[3; 16384]);
        let source = fixture
            .cache
            .resolve_seekable(&build_id.to_string(), "memfile")
            .await
            .unwrap();

        let chunks: Vec<_> = source.stream(0, 16384).await.unwrap().try_collect().await.unwrap();

        assert_eq!(
            chunks.iter().map(|chunk| chunk.len()).collect::<Vec<_>>(),
            vec![8192, 8192]
        );
    }

    #[tokio::test]
    async fn stream_of_no_bytes_sends_nothing() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        fixture.add_diff(build_id, Artifact::Memfile, &[1; 4096]);
        let source = fixture
            .cache
            .resolve_seekable(&build_id.to_string(), "memfile")
            .await
            .unwrap();

        let chunks: Vec<_> = source.stream(0, 0).await.unwrap().try_collect().await.unwrap();

        assert!(chunks.is_empty());
    }
}
