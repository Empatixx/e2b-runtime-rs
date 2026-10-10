use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::error::{Kind, WarmError};
use super::identity::Identity;
use super::scope::{Scope, ScopeError};
use super::{COPY_CHUNK, Cache, TEMP_PREFIX};
use crate::go_os::{path_error, reason};
use crate::go_strconv::quote;

#[derive(Debug)]
pub enum CopyFailure {
    Scope(ScopeError),
    Read(std::io::Error),
    Write(std::io::Error),
}

pub fn copy_context(
    scope: &Scope,
    destination: &mut impl Write,
    source: &mut impl Read,
) -> (u64, Result<(), CopyFailure>) {
    let mut chunk = vec![0u8; COPY_CHUNK];
    let mut written = 0u64;
    loop {
        if let Some(error) = scope.error() {
            return (written, Err(CopyFailure::Scope(error)));
        }
        let read = match source.read(&mut chunk) {
            Ok(0) => return (written, Ok(())),
            Ok(read) => read,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return (written, Err(CopyFailure::Read(e))),
        };
        if let Err(e) = destination.write_all(&chunk[..read]) {
            return (written, Err(CopyFailure::Write(e)));
        }
        written += read as u64;
    }
}

struct Reservation {
    cache: Arc<Cache>,
    destination: PathBuf,
    kept: bool,
}

impl Reservation {
    fn keep(mut self) {
        self.kept = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.kept {
            self.cache.release_in_flight(&self.destination);
        }
    }
}

struct Staged {
    temporary: tempfile::NamedTempFile,
    written: u64,
    copy: Result<(), CopyFailure>,
}

impl Cache {
    pub(crate) async fn copy_in(
        self: &Arc<Self>,
        scope: &Scope,
        source: &Path,
        identity: Identity,
    ) -> Result<PathBuf, WarmError> {
        if self.dir.as_os_str().is_empty() {
            self.note_failure(source);
            return Err(WarmError::other("envd binary cache directory is not configured"));
        }
        match self.copy_in_unchecked(scope, source, identity).await {
            Ok(destination) => Ok(destination),
            Err(error) if error.is(Kind::Superseded) => Err(WarmError::of(Kind::Superseded)),
            Err(error) => {
                self.note_failure(source);
                Err(error)
            }
        }
    }

    async fn copy_in_unchecked(
        self: &Arc<Self>,
        scope: &Scope,
        source: &Path,
        identity: Identity,
    ) -> Result<PathBuf, WarmError> {
        let destination = self.dir.join(identity.local_name(source));
        let cache = self.clone();
        let (staging_scope, staging_source) = (scope.clone(), source.to_path_buf());
        let staged_destination = destination.clone();
        let (reservation, staged) =
            tokio::task::spawn_blocking(move || cache.stage(&staging_scope, &staging_source, &staged_destination))
                .await
                .map_err(|e| WarmError::other(format!("stage envd binary copy: {e}")))??;

        let Staged {
            temporary,
            written,
            copy,
        } = staged;
        let outcome = self.settle(scope, source, identity, written, copy).await;
        outcome?;
        self.install(temporary, &destination)?;
        reservation.keep();
        Ok(destination)
    }

    fn stage(
        self: &Arc<Self>,
        scope: &Scope,
        source: &Path,
        destination: &Path,
    ) -> Result<(Reservation, Staged), WarmError> {
        std::fs::create_dir_all(&self.dir).map_err(|e| {
            WarmError::tagged(
                Kind::CacheUnusable,
                format!("create dir {}: {}", quote(&self.dir.display().to_string()), reason(&e)),
            )
        })?;
        self.reap_temps.call_once(|| reap_temp_files(&self.dir));
        self.reserve_in_flight(destination);
        let reservation = Reservation {
            cache: self.clone(),
            destination: destination.to_path_buf(),
            kept: false,
        };
        let mut input = std::fs::File::open(source).map_err(|e| {
            WarmError::other(format!(
                "open envd binary {}: {}",
                quote(&source.display().to_string()),
                path_error("open", source, &e)
            ))
        })?;
        let mut temporary = tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .tempfile_in(&self.dir)
            .map_err(|e| WarmError::tagged(Kind::CacheUnusable, format!("create temp file: {}", reason(&e))))?;
        let (written, copy) = copy_context(scope, temporary.as_file_mut(), &mut input);
        Ok((
            reservation,
            Staged {
                temporary,
                written,
                copy,
            },
        ))
    }

    async fn settle(
        &self,
        scope: &Scope,
        source: &Path,
        identity: Identity,
        written: u64,
        copy: Result<(), CopyFailure>,
    ) -> Result<(), WarmError> {
        let name = quote(&source.display().to_string());
        if copy.is_ok() && written != identity.size {
            return match self.stat_source_bounded(scope, source).await {
                Ok(current) if current != identity => Err(WarmError::of(Kind::Superseded)),
                _ => Err(WarmError::other(format!(
                    "copy envd binary {name}: short read of {name}: copied {written} of {} bytes",
                    identity.size
                ))),
            };
        }
        match copy {
            Ok(()) => Ok(()),
            Err(CopyFailure::Scope(error)) => Err(WarmError::other(format!("copy envd binary {name}: {error}"))),
            Err(CopyFailure::Read(e)) => Err(WarmError::other(format!("copy envd binary {name}: {}", reason(&e)))),
            Err(CopyFailure::Write(e)) => Err(WarmError::tagged(Kind::CacheUnusable, format!("write: {}", reason(&e)))),
        }
    }

    fn install(&self, temporary: tempfile::NamedTempFile, destination: &Path) -> Result<(), WarmError> {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .sync_all()
            .map_err(|e| WarmError::tagged(Kind::CacheUnusable, format!("close temp file: {}", reason(&e))))?;
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o755))
            .map_err(|e| WarmError::tagged(Kind::CacheUnusable, format!("chmod: {}", reason(&e))))?;
        temporary
            .persist(destination)
            .map(|_| ())
            .map_err(|e| WarmError::tagged(Kind::CacheUnusable, format!("rename: {}", reason(&e.error))))
    }

    pub(crate) fn reserve_in_flight(&self, destination: &Path) {
        *self
            .state
            .lock()
            .in_flight
            .entry(destination.to_path_buf())
            .or_insert(0) += 1;
    }

    pub(crate) fn release_in_flight(&self, destination: &Path) {
        let mut state = self.state.lock();
        match state.in_flight.get_mut(destination) {
            Some(count) if *count > 1 => *count -= 1,
            _ => {
                state.in_flight.remove(destination);
            }
        }
    }
}

pub fn reap_temp_files(directory: &Path) {
    let Ok(names) = std::fs::read_dir(directory) else {
        return;
    };
    names
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_dir()))
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
        .for_each(|entry| drop(std::fs::remove_file(entry.path())));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn copying_stops_on_a_cancelled_scope() {
        let (scope, cancel) = Scope::cancellable();
        cancel.cancel();
        let data = vec![b'x'; COPY_CHUNK * 3];

        let (written, outcome) = copy_context(&scope, &mut std::io::sink(), &mut data.as_slice());

        assert_eq!(written, 0);
        assert!(matches!(outcome, Err(CopyFailure::Scope(ScopeError::Cancelled))));
    }

    #[tokio::test]
    async fn a_complete_copy_reports_every_byte() {
        let data = vec![b'x'; COPY_CHUNK * 2 + 7];

        let (written, outcome) = copy_context(&Scope::background(), &mut std::io::sink(), &mut data.as_slice());

        assert_eq!(written, data.len() as u64);
        assert!(outcome.is_ok());
    }
}
