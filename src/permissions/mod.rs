//! Who runs the operation and where (upstream `internal/permissions`).

pub mod path;
pub mod user;

use connectrpc::ConnectError;

use crate::execcontext::resolve_default_username;
use user::UserInfo;

/// User for a Connect request: Basic-auth username if present, otherwise the default user from `/init`.
pub fn get_auth_user(basic: Option<&str>, default_user: &str) -> Result<UserInfo, ConnectError> {
    if let Some(name) = basic {
        return user::lookup(name).map_err(|_| ConnectError::unauthenticated(format!("invalid username: '{name}'")));
    }
    let name = resolve_default_username(None, default_user)
        .map_err(|_| ConnectError::unauthenticated("no user specified"))?;
    user::lookup(&name).map_err(|_| ConnectError::unauthenticated(format!("invalid default user: '{name}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_basic_user_is_unauthenticated() {
        let err = get_auth_user(Some("definitely-missing-user"), "root").unwrap_err();
        assert_eq!(err.code, connectrpc::ErrorCode::Unauthenticated);
        assert_eq!(err.message.as_deref(), Some("invalid username: 'definitely-missing-user'"));
    }

    #[test]
    fn falls_back_to_default_user() {
        assert_eq!(get_auth_user(None, "root").unwrap().name, "root");
    }

    #[test]
    fn empty_default_user_is_unauthenticated() {
        let err = get_auth_user(None, "").unwrap_err();
        assert_eq!(err.code, connectrpc::ErrorCode::Unauthenticated);
    }
}
