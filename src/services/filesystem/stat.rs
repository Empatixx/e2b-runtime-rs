//! `Stat` RPC (upstream `services/filesystem/stat.go`).

use connectrpc::{ConnectError, RequestContext};

use super::FilesystemService;
use super::entry::entry_info;
use crate::permissions::path::expand_and_resolve;
use crate::proto::filesystem::EntryInfo;

impl FilesystemService {
    /// Upstream `Service.Stat`: auth user -> resolve `path` (relative = from the user's home,
    /// empty = default workdir) -> `entry_info`.
    pub(super) fn stat_entry(&self, ctx: &RequestContext, path: &str) -> Result<EntryInfo, ConnectError> {
        let user = self.auth_user(ctx)?;
        // `defaults()` returns a clone; keep it in a variable so the borrowed `&str` lives long enough
        let defaults = self.state.defaults();
        let resolved =
            expand_and_resolve(path, &user, defaults.workdir.as_deref()).map_err(ConnectError::invalid_argument)?;
        entry_info(&resolved)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use connectrpc::ErrorCode;
    use http::HeaderMap;

    use super::*;
    use crate::permissions::authenticate::AuthUser;
    use crate::permissions::user;
    use crate::proto::filesystem::FileType;
    use crate::state::AppState;

    fn service() -> (FilesystemService, Arc<AppState>) {
        let args = ["envd", "-isnotfc"].iter().map(|s| s.to_string());
        let state = AppState::new(crate::config::parse(args).unwrap());
        (FilesystemService::new(state.clone()), state)
    }

    fn current_user() -> user::UserInfo {
        let name = nix::unistd::User::from_uid(nix::unistd::getuid()).unwrap().unwrap().name;
        user::lookup(&name).unwrap()
    }

    /// Context as the auth middleware leaves it (Go tests: `authn.SetInfo(ctx, u)`).
    fn ctx_as(u: &user::UserInfo) -> RequestContext {
        let mut ext = http::Extensions::new();
        ext.insert(AuthUser(u.clone()));
        RequestContext::new(HeaderMap::new()).with_extensions(ext)
    }

    fn real(p: &Path) -> String {
        std::fs::canonicalize(p).unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn stat_file_by_absolute_path() {
        let (svc, _) = service();
        let tmp = tempfile::tempdir().unwrap();
        let file = Path::new(&real(tmp.path())).join("file.txt");
        std::fs::write(&file, b"Hello, World!").unwrap();

        let me = current_user();
        let e = svc.stat_entry(&ctx_as(&me), file.to_str().unwrap()).unwrap();
        assert_eq!(e.path, file.to_str().unwrap());
        assert_eq!(e.r#type, buffa::EnumValue::from(FileType::FILE_TYPE_FILE));
        assert_eq!(e.size, 13);
        assert_eq!(e.owner, me.name);
    }

    #[test]
    fn empty_path_is_the_users_home() {
        let (svc, _) = service();
        let me = current_user();
        let e = svc.stat_entry(&ctx_as(&me), "").unwrap();
        assert_eq!(Path::new(&e.path), me.home.as_path());
        assert_eq!(e.r#type, buffa::EnumValue::from(FileType::FILE_TYPE_DIRECTORY));
    }

    #[test]
    fn empty_path_uses_default_workdir_from_init() {
        let (svc, state) = service();
        let tmp = tempfile::tempdir().unwrap();
        let dir = real(tmp.path());
        state.defaults.write().unwrap().workdir = Some(dir.clone());

        let e = svc.stat_entry(&ctx_as(&current_user()), "").unwrap();
        assert_eq!(e.path, dir);
    }

    #[test]
    fn no_basic_user_falls_back_to_default_user() {
        // default user from /init is "root"; "~" expands to root's home
        let (svc, _) = service();
        let root = user::lookup("root").unwrap();
        let e = svc.stat_entry(&RequestContext::new(HeaderMap::new()), "~").unwrap();
        assert_eq!(Path::new(&e.path), root.home.as_path());
    }

    #[test]
    fn missing_path_is_not_found() {
        let (svc, _) = service();
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing.txt");
        let err = svc.stat_entry(&ctx_as(&current_user()), missing.to_str().unwrap()).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[test]
    fn other_users_home_is_invalid_argument() {
        let (svc, _) = service();
        let me = current_user();
        let err = svc.stat_entry(&ctx_as(&me), "~nobody/x").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(
            err.message.as_deref(),
            Some(format!("failed to expand path '~nobody/x' for user '{}': cannot expand user-specific home dir", me.name).as_str())
        );
    }

    #[test]
    fn unknown_default_user_is_unauthenticated() {
        let (svc, state) = service();
        state.defaults.write().unwrap().user = "definitely-missing-user".into();
        let err = svc.stat_entry(&RequestContext::new(HeaderMap::new()), "/").unwrap_err();
        assert_eq!(err.code, ErrorCode::Unauthenticated);
        assert_eq!(err.message.as_deref(), Some("invalid default user: 'definitely-missing-user'"));
    }
}
