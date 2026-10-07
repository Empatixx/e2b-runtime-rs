mod entry;
mod ops;
mod watch;

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream};

use crate::app::AppState;
use crate::auth::request_user;
use crate::proto::filesystem::*;
use crate::user::User;
use crate::util::keepalive::keepalive_interval;
use crate::util::paths::expand_and_resolve;
use entry::entry_info;
use watch::{FileWatcher, PendingEvents};

pub struct FilesystemService {
    pub state: Arc<AppState>,
    pub watchers: Mutex<HashMap<String, FileWatcher>>,
}

#[allow(refining_impl_trait)]
impl Filesystem for FilesystemService {
    async fn stat(&self, ctx: RequestContext, request: ServiceRequest<'_, StatRequest>) -> ServiceResult<StatResponse> {
        let defaults = self.state.defaults();
        let user = request_user(ctx.extensions().get::<User>(), &defaults.user)?;
        let path = expand_and_resolve(request.path, &user, defaults.workdir.as_deref())
            .map_err(ConnectError::invalid_argument)?;

        let entry = entry_info(&path)?;
        Response::ok(StatResponse {
            entry: entry.into(),
            ..Default::default()
        })
    }

    async fn make_dir(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, MakeDirRequest>,
    ) -> ServiceResult<MakeDirResponse> {
        let defaults = self.state.defaults();
        let user = request_user(ctx.extensions().get::<User>(), &defaults.user)?;
        let path = expand_and_resolve(request.path, &user, defaults.workdir.as_deref())
            .map_err(ConnectError::invalid_argument)?;

        let entry = ops::make_dir(&path, &user)?;
        Response::ok(MakeDirResponse {
            entry: entry.into(),
            ..Default::default()
        })
    }

    async fn r#move(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, MoveRequest>,
    ) -> ServiceResult<MoveResponse> {
        let defaults = self.state.defaults();
        let user = request_user(ctx.extensions().get::<User>(), &defaults.user)?;
        let source = expand_and_resolve(request.source, &user, defaults.workdir.as_deref())
            .map_err(ConnectError::invalid_argument)?;
        let destination = expand_and_resolve(request.destination, &user, defaults.workdir.as_deref())
            .map_err(ConnectError::invalid_argument)?;

        let entry = ops::rename(&source, &destination, &user)?;
        Response::ok(MoveResponse {
            entry: entry.into(),
            ..Default::default()
        })
    }

    async fn list_dir(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, ListDirRequest>,
    ) -> ServiceResult<ListDirResponse> {
        let defaults = self.state.defaults();
        let user = request_user(ctx.extensions().get::<User>(), &defaults.user)?;
        let path = expand_and_resolve(request.path, &user, defaults.workdir.as_deref())
            .map_err(ConnectError::invalid_argument)?;

        let depth = request.depth;
        let entries = tokio::task::spawn_blocking(move || ops::list_dir(&path, depth))
            .await
            .map_err(|e| ConnectError::internal(e.to_string()))??;
        Response::ok(ListDirResponse {
            entries,
            ..Default::default()
        })
    }

    async fn remove(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, RemoveRequest>,
    ) -> ServiceResult<RemoveResponse> {
        let defaults = self.state.defaults();
        let user = request_user(ctx.extensions().get::<User>(), &defaults.user)?;
        let path = expand_and_resolve(request.path, &user, defaults.workdir.as_deref())
            .map_err(ConnectError::invalid_argument)?;

        tokio::task::spawn_blocking(move || ops::remove_all(&path))
            .await
            .map_err(|e| ConnectError::internal(e.to_string()))??;
        Response::ok(RemoveResponse::default())
    }

    async fn watch_dir(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, WatchDirRequest>,
    ) -> ServiceResult<ServiceStream<WatchDirResponse>> {
        let defaults = self.state.defaults();
        let user = request_user(ctx.extensions().get::<User>(), &defaults.user)?;
        let path = expand_and_resolve(request.path, &user, defaults.workdir.as_deref())
            .map_err(ConnectError::invalid_argument)?;
        watch::check_watch_path(&path, request.allow_network_mounts)?;

        let interval = keepalive_interval(ctx.headers());
        let stream = watch::stream_events(path, request.recursive, request.include_entry, interval)?;
        Response::ok(stream)
    }

    async fn create_watcher(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, CreateWatcherRequest>,
    ) -> ServiceResult<CreateWatcherResponse> {
        let defaults = self.state.defaults();
        let user = request_user(ctx.extensions().get::<User>(), &defaults.user)?;
        let path = expand_and_resolve(request.path, &user, defaults.workdir.as_deref())
            .map_err(ConnectError::invalid_argument)?;
        watch::check_watch_path(&path, request.allow_network_mounts)?;

        let pending = Arc::new(Mutex::new(PendingEvents::default()));
        let watcher = watch::watch(&path, request.recursive, {
            let pending = pending.clone();
            let root = path.clone();
            let include_entry = request.include_entry;
            move |result| watch::record_events(&pending, result, &root, include_entry)
        })?;

        let watcher_id = watch::watcher_id();
        self.watchers
            .lock()
            .insert(watcher_id.clone(), FileWatcher { watcher, pending });
        Response::ok(CreateWatcherResponse {
            watcher_id,
            ..Default::default()
        })
    }

    async fn get_watcher_events(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetWatcherEventsRequest>,
    ) -> ServiceResult<GetWatcherEventsResponse> {
        let watchers = self.watchers.lock();
        let watcher = watchers
            .get(request.watcher_id)
            .ok_or_else(|| watcher_not_found(request.watcher_id))?;

        let mut pending = watcher.pending.lock();
        match pending.error.clone() {
            Some(error) => Err(ConnectError::internal(error)),
            None => Response::ok(GetWatcherEventsResponse {
                events: std::mem::take(&mut pending.events),
                ..Default::default()
            }),
        }
    }

    async fn remove_watcher(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, RemoveWatcherRequest>,
    ) -> ServiceResult<RemoveWatcherResponse> {
        self.watchers
            .lock()
            .remove(request.watcher_id)
            .ok_or_else(|| watcher_not_found(request.watcher_id))?;
        Response::ok(RemoveWatcherResponse::default())
    }
}

fn watcher_not_found(watcher_id: &str) -> ConnectError {
    ConnectError::not_found(format!("watcher with id {watcher_id} not found"))
}
