use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use connectrpc::ConnectError;

use crate::auth::basic_username;
use crate::user::User;

pub async fn authenticate_user(mut request: Request, next: Next) -> Response {
    match basic_username(request.headers()).map(|name| User::lookup(&name).ok_or(name)) {
        None => next.run(request).await,
        Some(Ok(user)) => {
            request.extensions_mut().insert(user);
            next.run(request).await
        }
        Some(Err(name)) => ConnectError::unauthenticated(format!("invalid username: '{name}'"))
            .into_http_response(request.headers())
            .map(Body::new),
    }
}
