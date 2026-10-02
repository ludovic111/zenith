//! The tokio runtime the server connection runs on, beside GPUI's own executor. Futures from
//! `zenith_client` (channels) can be awaited from GPUI tasks; anything needing tokio's timers
//! or IO runs here through [`spawn`].

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::Runtime;

pub fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("zenith-io")
            .enable_all()
            .build()
            .expect("the IO runtime")
    })
}

/// Runs `future` on the IO runtime; the returned handle can be awaited from a GPUI task.
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime().spawn(future)
}
