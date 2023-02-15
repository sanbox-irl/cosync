use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::{
    create_future_object, next_cosync_task_id, unlock_mutex, Cosync, CosyncInput, CosyncQueueHandle, CosyncTaskId,
};

/// A `SerialCosync` has the same API as `Cosync`, but *only* runs one task at a time,
/// unlike `Cosync`, which runs all tasks concurrently. This means that in `SerialCosync`
/// a task must fully complete (return `Pending::Result`) before the next queued task is ran,
/// and so on and so on.
#[derive(Debug)]
pub struct SerialCosync<T: ?Sized>(Cosync<T>);

impl<T: ?Sized + 'static> SerialCosync<T> {
    /// Creates a new, empty [SerialCosync].
    pub fn new() -> Self {
        Self(Cosync::new())
    }

    /// Returns the `number of tasks queued + 1` if there is any task being executed.
    pub fn len(&self) -> usize {
        self.is_running_any() as usize + unlock_mutex(&self.0.queue).incoming.len()
    }

    /// Returns true if nothing is being executed and the queue is empty
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns true if the `SerialCosync` is executing a task. In general, that means the task has
    /// return `Pending` at least once after called `run_until_stall`.
    pub fn is_running_any(&self) -> bool {
        self.0.is_running_any()
    }

    /// Returns the id of the current task, if it is running.
    pub fn current_task_id(&self) -> Option<CosyncTaskId> {
        // we ensure that pool's length is always 1 so this will be the only task
        self.0.pool.iter().next().map(|v| v.1)
    }

    /// Creates a queue handle which can be used to spawn tasks.
    pub fn create_queue_handle(&self) -> CosyncQueueHandle<T> {
        self.0.create_queue_handle()
    }

    /// Tries to force unqueueing a task.
    ///
    /// If that task is already being ran, and you want to external cancel it, run
    /// `stop_running_task`. To see if a task is currently running, see `is_running`.
    ///
    /// This returns `true` on success and `false` on failure.
    pub fn unqueue_task(&mut self, task_id: CosyncTaskId) -> bool {
        let incoming = &mut unlock_mutex(&self.0.queue).incoming;
        let Some(index) = incoming.iter().position(|future_obj| future_obj.1 == task_id) else { return false };
        incoming.remove(index);

        true
    }

    /// Stops the current running task.
    pub fn stop_running_task(&mut self) {
        self.0.pool.clear();
    }

    /// Clears all queued tasks. The running task is unaffected. All `CosyncQueueHandler`s
    /// are still valid.
    pub fn clear_queue(&mut self) {
        unlock_mutex(&self.0.queue).incoming.clear();
    }

    /// This clears all running tasks and all queued tasks.
    ///
    /// All `CosyncQueueHandler`s are still valid.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Adds a new Task into the Queue. When `run_until` is called, if there are no running tasks,
    /// it will be de-queued and ran at `run_until`.
    pub fn queue<Task, Out>(&mut self, task: Task) -> CosyncTaskId
    where
        Task: FnOnce(CosyncInput<T>) -> Out + Send + 'static,
        Out: Future<Output = ()> + Send,
    {
        let cosync_input = CosyncInput(self.0.create_queue_handle());
        let id = next_cosync_task_id(&self.0.queue);

        let mut lock = unlock_mutex(&self.0.queue);
        lock.incoming.push_back(create_future_object(task, cosync_input, id));

        id
    }

    /// Runs tasks to completion, possibly looping forever.
    ///
    /// If a task is completed, progress will be made on the *next* task.
    ///
    /// This function only returns when all tasks in the pool and in the queue are completed.
    pub fn run_blocking(&mut self, parameter: &mut T) {
        super::run_blocking(self.0.data, parameter, |ctx| Self::poll_pool(&mut self.0, ctx));
    }

    /// Runs tasks in the queue and returns if no more progress can be made
    /// on any task.
    ///
    /// If a task is completed, progress will be made on the *next* task.
    ///
    /// This function returns when any task returns `Poll::Pending`.
    pub fn run_until_stall(&mut self, parameter: &mut T) {
        super::run_until_stall(self.0.data, parameter, |ctx| Self::poll_pool(&mut self.0, ctx))
    }

    fn poll_pool(cosync: &mut Cosync<T>, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            cosync.pool.increment_counter();
            // try to execute the next ready future
            let pinned_pool = Pin::new(&mut cosync.pool);
            let ret = pinned_pool.poll_next(cx);

            // no queued tasks; we may be done
            match ret {
                // this means an inner task is pending
                Poll::Pending => return Poll::Pending,
                // the pool was empty already, or we just completed that task.
                Poll::Ready(()) => {
                    // grab our next task...
                    if let Some(task) = unlock_mutex(&cosync.queue).incoming.pop_front() {
                        cosync.pool.push(task);

                        // and now let's re-run this bad boy
                    } else {
                        return Poll::Ready(());
                    }
                }
            }
        }
    }
}

impl<T: ?Sized + 'static> Default for SerialCosync<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::sleep_ticks;

    use super::*;

    #[test]
    #[allow(clippy::needless_late_init)]
    fn pool_remains_sequential() {
        // notice that value is declared here
        let mut value;

        let mut executor: SerialCosync<i32> = SerialCosync::new();
        executor.queue(move |mut input| async move {
            *input.get() = 10;

            sleep_ticks(100).await;

            *input.get() = 20;
        });

        executor.queue(move |mut input| async move {
            assert_eq!(*input.get(), 20);
        });

        value = 0;
        executor.run_until_stall(&mut value);
    }

    #[test]
    fn cancelling_a_task() {
        let mut cosync: SerialCosync<i32> = SerialCosync::new();

        cosync.queue(|mut input| async move {
            *input.get() += 1;

            sleep_ticks(1).await;

            *input.get() += 1;
        });

        let mut value = 0;

        cosync.run_until_stall(&mut value);

        assert_eq!(value, 1);

        cosync.stop_running_task();

        cosync.run_until_stall(&mut value);

        // it's still 1 because we cancelled the task which would have otherwise gotten it to 2
        assert_eq!(value, 1);

        assert!(cosync.is_empty());
        let id = cosync.queue(|_| async {});
        assert_eq!(cosync.len(), 1);
        let success = cosync.unqueue_task(id);
        assert!(success);
        assert!(cosync.is_empty());
    }
}
