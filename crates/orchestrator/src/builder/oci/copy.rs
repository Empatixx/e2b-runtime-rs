use std::fmt;
use std::path::Path;

use crate::command::Command;
use bytesize::ByteSize;

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
                ByteSize::b(self.image_size as u64).display().si(),
                ByteSize::b(self.max_size.max(0) as u64).display().si()
            ),
            false => write!(
                f,
                "the Docker image is too large for the maximum filesystem size of {}. {ADVICE}",
                ByteSize::b(self.max_size.max(0) as u64).display().si()
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
    let outcome = Command::new("rsync")
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
    let output = Command::new("du")
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
    fn too_large_texts_name_both_sizes() {
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
            "the uncompressed Docker image size (30.0 GB) exceeds the maximum filesystem size (26.2 GB). Please reduce your Docker image size (e.g., use a smaller base image, multi-stage builds, or remove unnecessary files)"
        );
        assert_eq!(
            without_size.to_string(),
            "the Docker image is too large for the maximum filesystem size of 26.2 GB. Please reduce your Docker image size (e.g., use a smaller base image, multi-stage builds, or remove unnecessary files)"
        );
    }
}
