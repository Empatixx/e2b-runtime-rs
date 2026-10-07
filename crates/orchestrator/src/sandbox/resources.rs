use futures::future::OptionFuture;

use crate::cgroup::Cgroup;
use crate::exposure::nbd::Exposure;
use crate::firecracker::process::FirecrackerProcess;
use crate::network::slot::Slot;
use crate::system::all_ok;

#[derive(Default)]
pub struct Resources {
    pub disk: Option<Exposure>,
    pub slot: Option<Slot>,
    pub cgroup: Option<Cgroup>,
    pub vm: Option<FirecrackerProcess>,
}

impl Resources {
    pub fn is_empty(&self) -> bool {
        self.disk.is_none() && self.slot.is_none() && self.cgroup.is_none() && self.vm.is_none()
    }

    pub async fn release(self) -> Result<(), String> {
        let Self { disk, slot, cgroup, vm } = self;
        let killed = OptionFuture::from(vm.map(FirecrackerProcess::kill)).await;
        let closed = OptionFuture::from(disk.map(Exposure::close)).await;
        let removed = cgroup.map(|cgroup| cgroup.remove().map_err(|e| format!("remove cgroup: {e}")));
        let destroyed = OptionFuture::from(slot.as_ref().map(Slot::destroy)).await;
        all_ok([killed, closed, removed, destroyed].into_iter().flatten())
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

async fn log_failure(sandbox_id: String, released: impl Future<Output = Result<(), String>>) {
    if let Err(e) = released.await {
        tracing::error!(sandbox = %sandbox_id, error = %e, "releasing a dropped sandbox failed");
    }
}
