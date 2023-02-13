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
    ///
    /// This *includes* the task currently being executed. Use [is_running_any] to see if there is a
    /// task currently being executed.
    ///
    /// [is_running_any]: Self::is_running_any
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

    /// Returns true if `SerialCosync` is executing the given `CosyncTaskId`.
    pub fn is_running(&self, task_id: CosyncTaskId) -> bool {
        self.0.is_running(task_id)
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
        let incoming = &mut unlock_mutex(&self.queue).incoming;
        let Some(index) = incoming.iter().position(|future_obj| future_obj.1 == task_id) else { return false };
        incoming.remove(index);

        true
    }

    /// Stops a currently running task of the given id.
    ///
    /// If that task is only queued, but `run` hasn't been called on the Cosync, then it's only
    /// queued -- run `unqueue_task` instead. To see if a task is currently running, see
    /// `is_running`.
    pub fn stop_running_task(&mut self, task_id: CosyncTaskId) -> bool {
        // check the pool
        for f in self.pool.iter_mut() {
            if f.1 == task_id {
                // we replace that task with an empty one instead. This is a hack...
                // which might work?
                f.0 = Box::pin(async move {});

                return true;
            }
        }

        false
    }

    /// Cosync keeps track of running task(s) and queued task(s). You can use this function to clear
    /// only the running tasks. This generally is a bad idea, producing complicated logic. In
    /// general, it's much easier to cancel internally in a future.
    ///
    /// For Cosync's which are `PollMultiple`, this will cancel all running tasks. It is not
    /// possible to only cancel a single running task right now. If you need that, please submit
    /// an issue.
    pub fn clear_running(&mut self) {
        self.pool.clear();
    }

    /// Clears all queued tasks. Currently running tasks are unaffected. All `CosyncQueueHandler`s
    /// are still valid.
    pub fn clear_queue(&mut self) {
        unlock_mutex(&self.queue).incoming.clear();
    }

    /// This clears all running tasks and all queued tasks.
    ///
    /// All `CosyncQueueHandler`s are still valid.
    pub fn clear(&mut self) {
        self.pool.clear();
        unlock_mutex(&self.queue).incoming.clear();
    }

    /// Adds a new Task into the Pool directly.
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

    /// Runs all tasks to completion, possibly looping forever.
    pub fn run_blocking(&mut self, parameter: &mut T) {
        super::run_blocking(self.0.data, parameter, |ctx| Self::poll_pool(&mut self.0, ctx));
    }

    /// Runs all tasks in the queue and returns if no more progress can be made
    /// on any task.
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
