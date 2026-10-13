use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::time::Instant;

use super::identity::Identity;
use super::scope::{Scope, ScopeError};
use super::{Cache, LOOKUP_STAT_BUDGET};
use crate::fs::path_error;

const STALLED: &str = "envd binary source stat stalled";

pub trait SourceStat: Send + Sync {
    fn stat(&self, path: &Path) -> io::Result<Identity>;
}

pub struct Filesystem;

impl SourceStat for Filesystem {
    fn stat(&self, path: &Path) -> io::Result<Identity> {
        std::fs::metadata(path).and_then(|metadata| Identity::of(&metadata))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatFailure {
    pub kind: io::ErrorKind,
    pub message: String,
}

impl StatFailure {
    pub fn of(path: &Path, error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: path_error("stat", path, error),
        }
    }

    pub fn not_found(&self) -> bool {
        self.kind == io::ErrorKind::NotFound
    }
}

impl fmt::Display for StatFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

pub type StatResult = Result<Identity, StatFailure>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupKind {
    Stalled,
    Scope(ScopeError),
    Stat(StatFailure),
    NoCache,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupError {
    pub kind: LookupKind,
    message: String,
}

impl LookupError {
    pub fn no_cache() -> Self {
        Self {
            kind: LookupKind::NoCache,
            message: "no envd binary cache".to_owned(),
        }
    }

    fn stalled(detail: String) -> Self {
        Self {
            kind: LookupKind::Stalled,
            message: format!("{STALLED}: {detail}"),
        }
    }

    fn scope(error: ScopeError) -> Self {
        Self {
            kind: LookupKind::Scope(error),
            message: error.to_string(),
        }
    }

    fn stat(failure: StatFailure) -> Self {
        Self {
            message: failure.message.clone(),
            kind: LookupKind::Stat(failure),
        }
    }

    pub fn for_source(self, source: &Path) -> Self {
        Self {
            message: format!("stat envd binary {:?}: {}", source.display(), self.message),
            kind: self.kind,
        }
    }

    pub fn is_stalled(&self) -> bool {
        matches!(self.kind, LookupKind::Stalled)
    }

    pub fn scope_error(&self) -> Option<ScopeError> {
        match self.kind {
            LookupKind::Scope(error) => Some(error),
            _ => None,
        }
    }

    pub fn is_no_cache(&self) -> bool {
        matches!(self.kind, LookupKind::NoCache)
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for LookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LookupError {}

struct FlightState {
    result: Option<StatResult>,
    warm_on_landing: bool,
}

pub struct Flight {
    pub started: Instant,
    state: Mutex<FlightState>,
    done: watch::Sender<bool>,
}

impl Flight {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            state: Mutex::new(FlightState {
                result: None,
                warm_on_landing: false,
            }),
            done: watch::Sender::new(false),
        }
    }

    async fn landed(&self) -> StatResult {
        let mut landed = self.done.subscribe();
        let _ = landed.wait_for(|done| *done).await;
        self.state
            .lock()
            .result
            .clone()
            .expect("a landed flight carries its result")
    }
}

impl Default for Flight {
    fn default() -> Self {
        Self::new()
    }
}

fn source_stat_label(result: &StatResult) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(failure) if failure.not_found() => "not_found",
        Err(_) => "error",
    }
}

async fn stat_blocking(source: Arc<dyn SourceStat>, path: PathBuf) -> StatResult {
    let probed = path.clone();
    match tokio::task::spawn_blocking(move || source.stat(&probed)).await {
        Ok(Ok(identity)) => Ok(identity),
        Ok(Err(error)) => Err(StatFailure::of(&path, &error)),
        Err(join) => Err(StatFailure {
            kind: io::ErrorKind::Other,
            message: format!("stat {}: {join}", path.display()),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundedError {
    Stat(StatFailure),
    Scope(ScopeError),
}

impl fmt::Display for BoundedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BoundedError::Stat(failure) => failure.fmt(f),
            BoundedError::Scope(error) => error.fmt(f),
        }
    }
}

impl Cache {
    pub(crate) fn current_source(&self) -> Arc<dyn SourceStat> {
        self.stat_source.read().clone()
    }

    #[cfg(test)]
    pub(crate) fn replace_source(&self, source: Arc<dyn SourceStat>) {
        *self.stat_source.write() = source;
    }

    pub(crate) async fn stat_source_bounded(&self, scope: &Scope, path: &Path) -> Result<Identity, BoundedError> {
        let stat = stat_blocking(self.current_source(), path.to_path_buf());
        tokio::select! {
            result = stat => result.map_err(BoundedError::Stat),
            error = scope.done() => Err(BoundedError::Scope(error)),
        }
    }

    pub(crate) async fn stat_source_for_lookup(
        self: &Arc<Self>,
        scope: &Scope,
        path: &Path,
        warm_late: bool,
    ) -> Result<Identity, LookupError> {
        let budget = self.lookup_stat_budget();
        let (flight, started_here) = self.join_flight(path);
        if started_here {
            self.run_flight(path, &flight, scope.detached());
        }
        let remaining = budget.saturating_sub(flight.started.elapsed());
        if remaining.is_zero() {
            return self.give_up(&flight, warm_late, || {
                LookupError::stalled(format!("in flight for {}", rounded(flight.started.elapsed())))
            });
        }
        tokio::select! {
            result = flight.landed() => result.map_err(LookupError::stat),
            () = tokio::time::sleep(remaining) => self.give_up(&flight, warm_late, || {
                LookupError::stalled(format!("no answer within {}", rounded(budget)))
            }),
            error = scope.done() => self.give_up(&flight, warm_late, || LookupError::scope(error)),
        }
    }

    fn join_flight(&self, path: &Path) -> (Arc<Flight>, bool) {
        let mut state = self.state.lock();
        match state.stat_flights.get(path) {
            Some(flight) => (flight.clone(), false),
            None => {
                let flight = Arc::new(Flight::new());
                state.stat_flights.insert(path.to_path_buf(), flight.clone());
                (flight, true)
            }
        }
    }

    fn run_flight(self: &Arc<Self>, path: &Path, flight: &Arc<Flight>, warm_scope: Scope) {
        let cache = self.clone();
        let flight = flight.clone();
        let path = path.to_path_buf();
        tokio::spawn(async move {
            let result = stat_blocking(cache.current_source(), path.clone()).await;
            cache
                .metrics
                .source_stat(flight.started.elapsed(), source_stat_label(&result));
            let warm = cache.land_flight(&path, &flight, result.clone());
            if let (true, Ok(identity)) = (warm, &result)
                && cache.lookup_identity(&path, *identity).is_none()
            {
                cache.warm_async_for(&warm_scope, &path, Some(*identity));
            }
        });
    }

    fn give_up(
        &self,
        flight: &Flight,
        warm_late: bool,
        unanswered: impl FnOnce() -> LookupError,
    ) -> Result<Identity, LookupError> {
        match self.abandon_flight(flight, warm_late) {
            Some(landed) => landed.map_err(LookupError::stat),
            None => Err(unanswered()),
        }
    }

    pub(crate) fn land_flight(&self, path: &Path, flight: &Arc<Flight>, result: StatResult) -> bool {
        let mut state = self.state.lock();
        let warm = {
            let mut inner = flight.state.lock();
            inner.result = Some(result);
            inner.warm_on_landing
        };
        if state
            .stat_flights
            .get(path)
            .is_some_and(|current| Arc::ptr_eq(current, flight))
        {
            state.stat_flights.remove(path);
        }
        drop(state);
        flight.done.send_replace(true);
        warm
    }

    pub(crate) fn abandon_flight(&self, flight: &Flight, warm: bool) -> Option<StatResult> {
        let _state = self.state.lock();
        let mut inner = flight.state.lock();
        if let Some(result) = &inner.result {
            return Some(result.clone());
        }
        inner.warm_on_landing |= warm;
        None
    }

    pub async fn bounded_stat(self: &Arc<Self>, scope: &Scope, path: &Path) -> Result<Identity, LookupError> {
        self.stat_source_for_lookup(scope, path, false).await
    }

    pub(crate) fn note_source_stalled(&self, path: &Path, cause: &LookupError) {
        if self.should_warn_stall(path) {
            tracing::warn!(
                path = %path.display(),
                error = %cause,
                "envd binary cache: source stat stalled; resumes skip the envd upgrade until the mount answers"
            );
        }
    }

    pub(crate) async fn lookup_source(
        self: &Arc<Self>,
        scope: &Scope,
        source: &Path,
    ) -> Result<(Option<Arc<super::Entry>>, Identity), LookupError> {
        let identity = self
            .stat_source_for_lookup(scope, source, true)
            .await
            .map_err(|error| error.for_source(source))?;
        Ok((self.lookup_identity(source, identity), identity))
    }
}

pub async fn bounded_stat_without_cache(scope: &Scope, path: &Path) -> Result<Identity, LookupError> {
    let stat = stat_blocking(Arc::new(Filesystem), path.to_path_buf());
    tokio::select! {
        result = stat => result.map_err(LookupError::stat),
        () = tokio::time::sleep(LOOKUP_STAT_BUDGET) => Err(LookupError::stalled(format!("no answer within {}", rounded(LOOKUP_STAT_BUDGET)))),
        error = scope.done() => Err(LookupError::scope(error)),
    }
}

fn rounded(duration: Duration) -> String {
    format!("{:?}", Duration::from_millis(duration.as_millis() as u64))
}
