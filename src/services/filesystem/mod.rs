//! `filesystem.Filesystem` Connect service (upstream `internal/services/filesystem`).

pub mod entry;
mod stat;

use std::sync::Arc;

use buffa::MessageField;
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream};

use crate::permissions::authenticate::{AuthUser, get_auth_user};
use crate::permissions::user::UserInfo;
use crate::proto::filesystem::*;
use crate::state::AppState;

/// Upstream `filesystem.Service`.
pub struct FilesystemService {
    state: Arc<AppState>,
}

impl FilesystemService {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    /// Upstream `permissions.GetAuthUser(ctx, s.defaults.User)`; the user put into the
    /// request by `authenticate_username` travels in `ctx.extensions()` (Go: `authn.GetInfo(ctx)`).
    fn auth_user(&self, ctx: &RequestContext) -> Result<UserInfo, ConnectError> {
        get_auth_user(ctx.extensions().get::<AuthUser>(), &self.state.defaults().user)
    }
}

/// Same text as connect-go's generated `UnimplementedFilesystemHandler`.
fn unimplemented(method: &str) -> ConnectError {
    ConnectError::unimplemented(format!("filesystem.Filesystem.{method} is not implemented"))
}

// concrete `ServiceResult<T>` instead of the trait's `impl Encodable` is intended
#[allow(refining_impl_trait)]
impl Filesystem for FilesystemService {
    async fn stat(&self, ctx: RequestContext, request: ServiceRequest<'_, StatRequest>) -> ServiceResult<StatResponse> {
        let entry = self.stat_entry(&ctx, request.path)?;
        Response::ok(StatResponse { entry: MessageField::some(entry), ..Default::default() })
    }

    async fn make_dir(&self, _ctx: RequestContext, _request: ServiceRequest<'_, MakeDirRequest>) -> ServiceResult<MakeDirResponse> {
        Err(unimplemented("MakeDir"))
    }

    async fn r#move(&self, _ctx: RequestContext, _request: ServiceRequest<'_, MoveRequest>) -> ServiceResult<MoveResponse> {
        Err(unimplemented("Move"))
    }

    async fn list_dir(&self, _ctx: RequestContext, _request: ServiceRequest<'_, ListDirRequest>) -> ServiceResult<ListDirResponse> {
        Err(unimplemented("ListDir"))
    }

    async fn remove(&self, _ctx: RequestContext, _request: ServiceRequest<'_, RemoveRequest>) -> ServiceResult<RemoveResponse> {
        Err(unimplemented("Remove"))
    }

    async fn watch_dir(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, WatchDirRequest>,
    ) -> ServiceResult<ServiceStream<WatchDirResponse>> {
        Err(unimplemented("WatchDir"))
    }

    async fn create_watcher(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, CreateWatcherRequest>,
    ) -> ServiceResult<CreateWatcherResponse> {
        Err(unimplemented("CreateWatcher"))
    }

    async fn get_watcher_events(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetWatcherEventsRequest>,
    ) -> ServiceResult<GetWatcherEventsResponse> {
        Err(unimplemented("GetWatcherEvents"))
    }

    async fn remove_watcher(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, RemoveWatcherRequest>,
    ) -> ServiceResult<RemoveWatcherResponse> {
        Err(unimplemented("RemoveWatcher"))
    }
}
