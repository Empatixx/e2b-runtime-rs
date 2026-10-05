use std::sync::Arc;

use crate::config::Config;

pub struct AppState {
    pub config: Config,
}

impl AppState {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self { config })
    }
}
