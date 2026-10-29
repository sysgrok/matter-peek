//! How the hub runs its controller: the controller's tasks (the transport, mDNS
//! and each handler of its node) on a work-stealing executor, on as many
//! threads as the host has cores, the controller's own thread included.

use std::future::Future;
use std::thread;

use async_executor::Executor;
use futures_lite::future;
use futures_util::future::select_all;
use log::warn;

use matter_peek_core::api::platform::{Runner, Task};

/// The stack size of each thread: as the controller's own.
const THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;

/// Runs the controller's tasks on threads that take them from each other, and
/// its service's own future on the controller's thread.
pub(crate) struct Threads;

impl Runner for Threads {
    fn run<'a>(self, tasks: Vec<Task<'a>>, main: impl Future<Output = ()> + 'a) {
        let executor = Executor::new();
        let threads = thread::available_parallelism().map_or(1, |threads| threads.get());

        thread::scope(|scope| {
            // Closed once dropped, even when unwinding: the threads end then.
            let (stop, stopped) = async_channel::bounded::<()>(1);

            for i in 1..threads {
                let (executor, stopped) = (&executor, stopped.clone());
                let spawned = thread::Builder::new()
                    .name(format!("matter-peek-{i}"))
                    .stack_size(THREAD_STACK_SIZE)
                    .spawn_scoped(scope, move || {
                        let _ = async_io::block_on(executor.run(stopped.recv()));
                    });

                if let Err(e) = spawned {
                    warn!("controller: cannot start thread {i}: {e}");
                    break;
                }
            }

            let tasks: Vec<_> = tasks.into_iter().map(|task| executor.spawn(task)).collect();
            let ended = async {
                if tasks.is_empty() {
                    future::pending::<()>().await;
                }
                select_all(tasks).await;
            };

            async_io::block_on(executor.run(future::or(Box::pin(main), ended)));

            drop(stop);
        });
    }
}
