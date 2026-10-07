mod entry;
mod ops;

use std::sync::Arc;

use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream};

use crate::app::AppState;
use crate::auth::request_user;
use crate::proto::filesystem::*;
use crate::user::User;
use crate::util::paths::expand_and_resolve;
use entry::entry_info;

pub struct FilesystemService {
    pub state: Arc<AppState>,
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

        let entries = ops::list_dir(&path, request.depth)?;
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

        ops::remove_all(&path)?;
        Response::ok(RemoveResponse::default())
    }

    async fn watch_dir(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, WatchDirRequest>,
    ) -> ServiceResult<ServiceStream<WatchDirResponse>> {
        Err(ConnectError::unimplemented(
            "filesystem.Filesystem.WatchDir is not implemented",
        ))
    }

    async fn create_watcher(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, CreateWatcherRequest>,
    ) -> ServiceResult<CreateWatcherResponse> {
        Err(ConnectError::unimplemented(
            "filesystem.Filesystem.CreateWatcher is not implemented",
        ))
    }

    async fn get_watcher_events(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetWatcherEventsRequest>,
    ) -> ServiceResult<GetWatcherEventsResponse> {
        Err(ConnectError::unimplemented(
            "filesystem.Filesystem.GetWatcherEvents is not implemented",
        ))
    }

    async fn remove_watcher(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, RemoveWatcherRequest>,
    ) -> ServiceResult<RemoveWatcherResponse> {
        Err(ConnectError::unimplemented(
            "filesystem.Filesystem.RemoveWatcher is not implemented",
        ))
    }
}
