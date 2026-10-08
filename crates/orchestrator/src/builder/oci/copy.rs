use std::fmt;
use std::path::Path;

use crate::go_exec::GoCommand;
use crate::go_humanize::bytes;

const NO_SPACE: &str = "No space left on device";
const ADVICE: &str = "Please reduce your Docker image size (e.g., use a smaller base image, multi-stage builds, or remove unnecessary files)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageTooLarge {
    pub image_size: i64,
    pub max_size: i64,
}

impl fmt::Display for ImageTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.image_size > 0 {
            true => write!(
                f,
                "the uncompressed Docker image size ({}) exceeds the maximum filesystem size ({}). {ADVICE}",
                bytes(self.image_size as u64),
                bytes(self.max_size.max(0) as u64)
            ),
            false => write!(
                f,
                "the Docker image is too large for the maximum filesystem size of {}. {ADVICE}",
                bytes(self.max_size.max(0) as u64)
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyError {
    TooLarge(ImageTooLarge),
    Failed(String),
}

pub async fn copy_files(source: &Path, destination: &Path, max_size: i64) -> Result<(), CopyError> {
    let source_arg = format!("{}/", source.display());
    let outcome = GoCommand::new("rsync")
        .args(["-aH", "--whole-file", "--inplace", &source_arg])
        .args([destination])
        .current_dir("/")
        .combined_output()
        .await;
    let Err(failure) = outcome else {
        return Ok(());
    };
    let message = format!(
        "while copying files from {} to {}: {}: {}",
        source.display(),
        destination.display(),
        failure.message,
        String::from_utf8_lossy(&failure.output)
    );
    match message.contains(NO_SPACE) {
        true => Err(CopyError::TooLarge(ImageTooLarge {
            image_size: dir_size(source).await.unwrap_or(0),
            max_size,
        })),
        false => Err(CopyError::Failed(message)),
    }
}

pub async fn dir_size(dir: &Path) -> Result<i64, String> {
    let output = GoCommand::new("du")
        .args(["-sb".as_ref(), dir.as_os_str()])
        .run()
        .await
        .map_err(|e| format!("error running du: {}", e.message))?;
    let output = String::from_utf8_lossy(&output).into_owned();
    output
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("unexpected du output: {output}"))?
        .parse()
        .map_err(|e| format!("error parsing du output: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn too_large_texts_match_upstream() {
        let sized = ImageTooLarge {
            image_size: 30_000_000_000,
            max_size: 26_214_400_000,
        };
        let without_size = ImageTooLarge {
            image_size: 0,
            max_size: 26_214_400_000,
        };

        assert_eq!(
            sized.to_string(),
            "the uncompressed Docker image size (30 GB) exceeds the maximum filesystem size (26 GB). Please reduce your Docker image size (e.g., use a smaller base image, multi-stage builds, or remove unnecessary files)"
        );
        assert_eq!(
            without_size.to_string(),
            "the Docker image is too large for the maximum filesystem size of 26 GB. Please reduce your Docker image size (e.g., use a smaller base image, multi-stage builds, or remove unnecessary files)"
        );
    }
}
