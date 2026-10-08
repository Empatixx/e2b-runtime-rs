use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use parking_lot::RwLock;

use crate::proto::ServiceInfoStatus;

pub struct NodeStatus {
    state: RwLock<State>,
    outstanding: Arc<AtomicU64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    pub status: ServiceInfoStatus,
    pub changed_at: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Health {
    pub status: &'static str,
    pub code: u16,
}

pub struct Work {
    outstanding: Arc<AtomicU64>,
}

impl Drop for Work {
    fn drop(&mut self) {
        self.outstanding.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Default for NodeStatus {
    fn default() -> Self {
        Self {
            state: RwLock::new(State {
                status: ServiceInfoStatus::Healthy,
                changed_at: SystemTime::now(),
            }),
            outstanding: Arc::default(),
        }
    }
}

impl NodeStatus {
    pub fn state(&self) -> State {
        *self.state.read()
    }

    pub fn override_to(&self, requested: ServiceInfoStatus) -> Result<(), String> {
        let mut state = self.state.write();
        if !allowed(state.status, requested) {
            return Err(format!("cannot override node status to {}", requested.as_str_name()));
        }
        if state.status != requested {
            tracing::info!(
                from = state.status.as_str_name(),
                to = requested.as_str_name(),
                "service status changed"
            );
            *state = State {
                status: requested,
                changed_at: SystemTime::now(),
            };
        }
        Ok(())
    }

    pub fn shut_down(&self) {
        *self.state.write() = State {
            status: ServiceInfoStatus::ShuttingDown,
            changed_at: SystemTime::now(),
        };
    }

    pub fn health(&self) -> Health {
        match self.state().status {
            ServiceInfoStatus::Healthy => Health {
                status: "healthy",
                code: 200,
            },
            ServiceInfoStatus::Unhealthy => Health {
                status: "unhealthy",
                code: 503,
            },
            ServiceInfoStatus::Draining | ServiceInfoStatus::Standby | ServiceInfoStatus::ShuttingDown => Health {
                status: "draining",
                code: 200,
            },
        }
    }

    pub fn track_work(&self) -> Work {
        self.outstanding.fetch_add(1, Ordering::SeqCst);
        Work {
            outstanding: self.outstanding.clone(),
        }
    }

    pub fn outstanding_work(&self) -> u64 {
        self.outstanding.load(Ordering::SeqCst)
    }
}

fn allowed(current: ServiceInfoStatus, requested: ServiceInfoStatus) -> bool {
    match (current, requested) {
        (_, ServiceInfoStatus::ShuttingDown) => false,
        (ServiceInfoStatus::Draining, ServiceInfoStatus::Standby) => false,
        (ServiceInfoStatus::ShuttingDown, requested) => requested == ServiceInfoStatus::ShuttingDown,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_follow_upstream_rules() {
        let status = NodeStatus::default();

        assert_eq!(
            status.override_to(ServiceInfoStatus::ShuttingDown),
            Err("cannot override node status to ShuttingDown".to_owned())
        );
        assert_eq!(status.override_to(ServiceInfoStatus::Draining), Ok(()));
        assert_eq!(
            status.override_to(ServiceInfoStatus::Standby),
            Err("cannot override node status to Standby".to_owned())
        );
        assert_eq!(status.override_to(ServiceInfoStatus::Healthy), Ok(()));
        assert_eq!(status.override_to(ServiceInfoStatus::Standby), Ok(()));

        status.shut_down();
        assert_eq!(
            status.override_to(ServiceInfoStatus::Healthy),
            Err("cannot override node status to Healthy".to_owned())
        );
    }

    #[test]
    fn setting_the_same_status_keeps_its_timestamp() {
        let status = NodeStatus::default();
        status.override_to(ServiceInfoStatus::Draining).unwrap();
        let first = status.state();

        status.override_to(ServiceInfoStatus::Draining).unwrap();

        assert_eq!(status.state(), first);
    }

    #[test]
    fn health_reads_draining_for_every_non_serving_state() {
        let status = NodeStatus::default();
        assert_eq!(
            status.health(),
            Health {
                status: "healthy",
                code: 200
            }
        );
        status.override_to(ServiceInfoStatus::Unhealthy).unwrap();
        assert_eq!(
            status.health(),
            Health {
                status: "unhealthy",
                code: 503
            }
        );
        status.override_to(ServiceInfoStatus::Standby).unwrap();
        assert_eq!(
            status.health(),
            Health {
                status: "draining",
                code: 200
            }
        );
        status.shut_down();
        assert_eq!(
            status.health(),
            Health {
                status: "draining",
                code: 200
            }
        );
    }

    #[test]
    fn work_is_counted_until_its_guard_drops() {
        let status = NodeStatus::default();
        let create = status.track_work();
        let lifecycle = status.track_work();
        assert_eq!(status.outstanding_work(), 2);
        drop(create);
        assert_eq!(status.outstanding_work(), 1);
        drop(lifecycle);
        assert_eq!(status.outstanding_work(), 0);
    }
}
