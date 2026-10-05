pub mod auth;
pub mod health;
pub mod signing;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};

use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/health", get(health::get_health))
        .route("/metrics", get(health::not_implemented))
        .route("/freeze", post(health::not_implemented))
        .route("/unfreeze", post(health::not_implemented))
        .route("/fsfreeze", post(health::not_implemented))
        .route("/fsthaw", post(health::not_implemented))
        .route("/collapse", post(health::not_implemented))
}
