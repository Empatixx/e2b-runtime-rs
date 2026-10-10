use tokio::sync::mpsc::Sender;
use tonic::Status;

use super::chrooted::file::ConfinedFile;
use super::failure::{internal, process_error};
use super::service::VolumeServer;
use crate::proto::{
    GetFileRequest, GetFileResponse, VolumeFileGetResponseContent, VolumeFileGetResponseFinish,
    VolumeFileGetResponseStart, get_file_response,
};

const FILE_STREAM_CHUNK_SIZE: usize = 1024 * 1024;

pub struct FileDownload {
    file: ConfinedFile,
    size: i64,
}

impl VolumeServer {
    pub fn open_download(&self, request: &GetFileRequest) -> Result<FileDownload, Status> {
        let confined = self.confined(request.volume.as_ref(), &request.path)?;
        let file = confined
            .fs
            .open_read(&confined.path)
            .map_err(|e| process_error("failed to open file", &e))?;
        let size = confined
            .fs
            .stat(&confined.path)
            .map_err(|e| process_error("failed to stat file", &e))?
            .size;
        Ok(FileDownload { file, size })
    }
}

impl FileDownload {
    pub fn send_all(mut self, sender: &Sender<Result<GetFileResponse, Status>>) {
        let sent = sender
            .blocking_send(Ok(message(get_file_response::Message::Start(
                VolumeFileGetResponseStart { size: self.size },
            ))))
            .is_ok()
            && self.send_content(sender)
            && sender
                .blocking_send(Ok(message(get_file_response::Message::Finish(
                    VolumeFileGetResponseFinish {},
                ))))
                .is_ok();
        if !sent {
            tracing::debug!(file = self.file.name(), "file download stopped before the end");
        }
    }

    fn send_content(&mut self, sender: &Sender<Result<GetFileResponse, Status>>) -> bool {
        let mut buffer = vec![0u8; FILE_STREAM_CHUNK_SIZE];
        loop {
            match self.file.read(&mut buffer) {
                Ok(0) => return true,
                Ok(read) => {
                    let content = message(get_file_response::Message::Content(VolumeFileGetResponseContent {
                        content: buffer[..read].to_vec(),
                    }));
                    if sender.blocking_send(Ok(content)).is_err() {
                        return false;
                    }
                }
                Err(e) => {
                    drop(sender.blocking_send(Err(internal(format!("failed to read file: {e}")))));
                    return false;
                }
            }
        }
    }
}

fn message(message: get_file_response::Message) -> GetFileResponse {
    GetFileResponse { message: Some(message) }
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::*;
    use crate::proto::UserErrorCode;
    use crate::volumes::test_support::{TestVolume, assert_user_error};

    fn download(volume: &TestVolume, path: &str) -> Result<FileDownload, Status> {
        volume.server.open_download(&GetFileRequest {
            volume: volume.info.clone(),
            path: path.to_owned(),
        })
    }

    fn collect(download: FileDownload) -> Vec<Result<GetFileResponse, Status>> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        download.send_all(&sender);
        drop(sender);
        std::iter::from_fn(|| receiver.blocking_recv()).collect()
    }

    fn messages(responses: Vec<Result<GetFileResponse, Status>>) -> Vec<get_file_response::Message> {
        responses
            .into_iter()
            .map(|response| response.unwrap().message.unwrap())
            .collect()
    }

    #[test]
    fn streams_start_content_and_finish() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("test-get.txt", "hello world");

        let streamed = messages(collect(download(&volume, "test-get.txt").unwrap()));

        assert_eq!(
            streamed,
            [
                get_file_response::Message::Start(VolumeFileGetResponseStart { size: 11 }),
                get_file_response::Message::Content(VolumeFileGetResponseContent {
                    content: b"hello world".to_vec()
                }),
                get_file_response::Message::Finish(VolumeFileGetResponseFinish {}),
            ]
        );
    }

    #[test]
    fn an_empty_file_streams_start_and_finish_only() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("empty", "");

        let streamed = messages(collect(download(&volume, "empty").unwrap()));

        assert_eq!(
            streamed,
            [
                get_file_response::Message::Start(VolumeFileGetResponseStart { size: 0 }),
                get_file_response::Message::Finish(VolumeFileGetResponseFinish {}),
            ]
        );
    }

    #[test]
    fn a_big_file_is_split_into_megabyte_chunks() {
        let Some(volume) = TestVolume::new() else { return };
        let content = vec![7u8; FILE_STREAM_CHUNK_SIZE * 2 + 5];
        std::fs::write(volume.host("big"), &content).unwrap();

        let sizes: Vec<usize> = messages(collect(download(&volume, "big").unwrap()))
            .into_iter()
            .filter_map(|message| match message {
                get_file_response::Message::Content(content) => Some(content.content.len()),
                _ => None,
            })
            .collect();

        assert_eq!(sizes.iter().sum::<usize>(), content.len());
        assert!(sizes.iter().all(|size| *size <= FILE_STREAM_CHUNK_SIZE));
    }

    #[test]
    fn a_missing_file_is_not_found_before_anything_is_streamed() {
        let Some(volume) = TestVolume::new() else { return };

        let status = download(&volume, "non-existent").err().unwrap();

        assert_user_error(&status, Code::NotFound, UserErrorCode::PathNotFound);
        assert_eq!(
            status.message(),
            "failed to open file: open /non-existent: no such file or directory"
        );
    }

    #[test]
    fn a_path_through_a_regular_file_is_an_invalid_request() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("traverse-file.txt", "test");

        let status = download(&volume, "traverse-file.txt/sub").err().unwrap();

        assert_user_error(&status, Code::InvalidArgument, UserErrorCode::InvalidRequest);
    }

    #[test]
    fn a_directory_fails_when_it_is_read_after_the_start_message() {
        let Some(volume) = TestVolume::new() else { return };
        std::fs::create_dir(volume.host("dir")).unwrap();

        let responses = collect(download(&volume, "dir").unwrap());

        assert_eq!(responses.len(), 2);
        assert!(responses[0].is_ok());
        assert_eq!(
            responses[1].as_ref().unwrap_err().message(),
            "failed to read file: read /dir: is a directory"
        );
    }

    #[test]
    fn a_link_is_read_inside_the_volume() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("inside.txt", "inside");
        std::os::unix::fs::symlink("/inside.txt", volume.host("link")).unwrap();

        let streamed = messages(collect(download(&volume, "link").unwrap()));

        assert_eq!(
            streamed[1],
            get_file_response::Message::Content(VolumeFileGetResponseContent {
                content: b"inside".to_vec()
            })
        );
    }
}
