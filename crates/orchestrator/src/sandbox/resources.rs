use futures::future::OptionFuture;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::cgroup::Cgroup;
use crate::exposure::Exposure;
use crate::firecracker::process::FirecrackerProcess;
use crate::memory::server::MemoryServer;
use crate::network::slot::Slot;
use crate::sandbox::config::Held;
use crate::system::all_ok;

#[derive(Default)]
pub struct Resources {
    pub disk: Option<Exposure>,
    pub slot: Option<Slot>,
    pub cgroup: Option<Cgroup>,
    pub vm: Option<FirecrackerProcess>,
    pub memory: Option<MemoryServer>,
    pub prefetch: Option<JoinHandle<Result<(), String>>>,
    pub held: Vec<Held>,
}

impl Resources {
    pub fn is_empty(&self) -> bool {
        self.disk.is_none()
            && self.slot.is_none()
            && self.cgroup.is_none()
            && self.vm.is_none()
            && self.memory.is_none()
            && self.prefetch.is_none()
            && self.held.is_empty()
    }

    pub async fn release(self) -> Result<(), String> {
        let Self {
            disk,
            slot,
            cgroup,
            vm,
            memory,
            prefetch,
            held,
        } = self;
        if let Some(prefetch) = prefetch {
            prefetch.abort();
        }
        let killed = OptionFuture::from(vm.map(FirecrackerProcess::kill)).await;
        let served = OptionFuture::from(memory.map(MemoryServer::stop)).await;
        let closed = OptionFuture::from(disk.map(Exposure::close)).await;
        let removed = OptionFuture::from(cgroup.map(remove_cgroup)).await;
        let destroyed = OptionFuture::from(slot.as_ref().map(Slot::destroy)).await;
        drop(held);
        all_ok([killed, served, closed, removed, destroyed].into_iter().flatten())
    }

    pub fn release_in_background(self, sandbox_id: String) {
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(log_failure(sandbox_id, self.release()));
            }
            Err(_) => {
                tracing::error!(sandbox = %sandbox_id, "sandbox dropped outside a tokio runtime; its resources leak")
            }
        }
    }
}

async fn remove_cgroup(mut cgroup: Cgroup) -> Result<(), String> {
    cgroup
        .remove(&CancellationToken::new())
        .await
        .map_err(|e| format!("remove cgroup: {e}"))
}

async fn log_failure(sandbox_id: String, released: impl Future<Output = Result<(), String>>) {
    if let Err(e) = released.await {
        tracing::error!(sandbox = %sandbox_id, error = %e, "releasing a dropped sandbox failed");
    }
}
