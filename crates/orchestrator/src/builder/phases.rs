pub mod base;

use std::sync::Arc;

use super::config::BuilderConfig;
use super::oci::pull::Puller;
use crate::server::node::Node;
use crate::storage::backend::Storage;

pub struct BuildEnv {
    pub node: Arc<Node>,
    pub config: BuilderConfig,
    pub puller: Puller,
    pub build_storage: Storage,
    pub development: bool,
}
