use std::future::Future;

pub fn run<F>(future: F) -> Option<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()
            .map(|runtime| runtime.block_on(future))
    })
    .join()
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn futures_run_to_completion_on_their_own_thread_even_inside_a_runtime() {
        assert_eq!(run(async { 7 }), Some(7));
    }
}
