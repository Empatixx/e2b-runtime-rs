use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use connectrpc::ConnectError;

use crate::auth::basic_username;
use crate::user::User;

pub async fn authenticate_user(mut request: Request, next: Next) -> Response {
    let Some(name) = basic_username(request.headers()) else {
        return next.run(request).await;
    };

    match User::lookup(&name) {
        Some(user) => {
            request.extensions_mut().insert(user);
            next.run(request).await
        }
        None => ConnectError::unauthenticated(format!("invalid username: '{name}'"))
            .into_http_response(request.headers())
            .map(Body::new),
    }
}
