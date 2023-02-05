#![doc = include_str!("../README.md")]
#![deny(rust_2018_idioms)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

// this is vendored code from the `futures-rs` crate, to avoid
// having a huge dependency when we only need a little bit
mod futures;
use crate::futures::{enter::enter, waker_ref, ArcWake, FuturesUnordered};

use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    marker::PhantomData,
    ops,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    task::{Context, Poll},
    thread::{self, Thread},
};

/// A single-threaded, sequential, parameterized async task queue.
///
/// This executor allows you to queue multiple tasks in sequence, and to
/// queue tasks within other tasks. Tasks are done in the order they
/// are queued.
///
/// You can queue a task by using [queue](Cosync::queue), by spawning a [CosyncQueueHandle]
/// and calling [queue](CosyncQueueHandle::queue), or, within a task, calling
/// [queue_task](CosyncInput::queue) on [CosyncInput].
#[derive(Debug)]
pub struct Cosync<T: ?Sized> {
    pool: FuturesUnordered<FutureObject>,
    queue: Arc<Mutex<IncomingQueue>>,
    data: *mut Option<*mut T>,
}

impl<T: 'static + ?Sized> Cosync<T> {
    /// Create a new, empty Cosync.
    pub fn new() -> Self {
        Self {
            pool: FuturesUnordered::new(),
            queue: Arc::default(),
            data: Box::into_raw(Box::new(None)),
        }
    }

    /// Returns the number of tasks queued. This *includes* the task currently being executed. Use
    /// [is_running] to see if there is a task currently being executed (ie, it returned `Pending`
    /// at some point in its execution).
    ///
    /// [is_running]: Self::is_running
    pub fn len(&self) -> usize {
        let one = self.is_running_any() as usize;

        one + self.queue.lock().incoming.len()
    }

    /// Returns true if no futures are being executed *and* there are no futures in the queue.
    pub fn is_empty(&self) -> bool {
        !self.is_running_any() && self.queue.lock().incoming.is_empty()
    }

    /// Returns true if `cosync` has a `Pending` future. It is possible for
    /// the `cosync` to have no `Pending` future, but to have tasks queued still.
    pub fn is_running_any(&self) -> bool {
        !self.pool.is_empty()
    }

    /// Returns true if `cosync` is executing the given `CosyncTaskId`.
    pub fn is_running(&self, task_id: CosyncTaskId) -> bool {
        self.pool.iter().any(|v| v.1 == task_id)
    }

    /// Creates a queue handle which can be used to spawn tasks.
    pub fn create_queue_handle(&self) -> CosyncQueueHandle<T> {
        CosyncQueueHandle {
            heap_ptr: self.data,
            queue: Arc::downgrade(&self.queue),
        }
    }

    /// Tries to force unqueueing a task.
    ///
    /// If that task is already being ran, and you want to external cancel it, run
    /// `stop_running_task`. To see if a task is currently running, see `is_running`.
    ///
    /// This returns `true` on success and `false` on failure.
    pub fn unqueue_task(&mut self, task_id: CosyncTaskId) -> bool {
        let queue_handle = self.create_queue_handle().queue.upgrade().unwrap();

        let incoming = &mut queue_handle.lock().incoming;
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
        self.queue.lock().incoming.clear();
    }

    /// This clears all running tasks and all queued tasks.
    ///
    /// All `CosyncQueueHandler`s are still valid.
    pub fn clear(&mut self) {
        self.pool.clear();
        self.queue.lock().incoming.clear();
    }

    /// Adds a new Task to the TaskQueue.
    pub fn queue<Task, Out>(&mut self, task: Task) -> CosyncTaskId
    where
        Task: FnOnce(CosyncInput<T>) -> Out + Send + 'static,
        Out: Future<Output = ()> + Send,
    {
        let queue_handle = self.create_queue_handle();

        // panics: we can unwrap here because know that the cosync exists because we are the cosync.
        queue_handle.queue(task).unwrap()
    }

    /// Run all tasks in the queue to completion. If a task won't complete, this will infinitely
    /// retry it. You probably want `run_until_stall`, which returns once a task returns
    /// `Polling::Pending`.
    ///
    /// ```
    /// # use cosync::Cosync;
    ///
    /// let mut cosync: Cosync<i32> = Cosync::new();
    /// cosync.queue(move |mut input| async move {
    ///     let mut input = input.get();
    ///     *input = 10;
    /// });
    ///
    /// let mut value = 0;
    /// cosync.run_blocking(&mut value);
    /// assert_eq!(value, 10);
    /// ```
    ///
    /// The function will block the calling thread until *all* tasks in the pool
    /// are complete, including any spawned while running existing tasks.
    pub fn run_blocking(&mut self, parameter: &mut T) {
        // SAFETY: we own this box and make sure it's safe until we drop,
        // so the pointer is still valid if we have an `&mut self`
        unsafe {
            *self.data = Some(parameter as *mut _);
        }

        let _enter = enter().expect(
            "cannot execute `LocalPool` executor from within \
             another executor",
        );

        CURRENT_THREAD_NOTIFY.with(|thread_notify| {
            let waker = waker_ref::waker_ref(thread_notify);
            let mut cx = Context::from_waker(&waker);
            loop {
                if let Poll::Ready(t) = self.poll_pool(&mut cx) {
                    return t;
                }
                // Consume the wakeup that occurred while executing `f`, if any.
                let unparked = thread_notify.unparked.swap(false, Ordering::Acquire);
                if !unparked {
                    // No wakeup occurred. It may occur now, right before parking,
                    // but in that case the token made available by `unpark()`
                    // is guaranteed to still be available and `park()` is a no-op.
                    thread::park();
                    // When the thread is unparked, `unparked` will have been set
                    // and needs to be unset before the next call to `f` to avoid
                    // a redundant loop iteration.
                    thread_notify.unparked.store(false, Ordering::Release);
                }
            }
        });

        // safety: see above
        unsafe {
            *self.data = None;
        }
    }

    /// Runs all tasks in the queue and returns if no more progress can be made
    /// on any task.
    ///
    /// ```
    /// use cosync::{sleep_ticks, Cosync};
    ///
    /// let mut cosync = Cosync::new();
    /// cosync.queue(move |mut input| async move {
    ///     *input.get() = 10;
    ///     // this will make the executor stall for a call
    ///     // we call `run_until_stall` an additional time,
    ///     // so we'll complete this 1 tick sleep.
    ///     sleep_ticks(1).await;
    ///
    ///     *input.get() = 20;
    /// });
    ///
    /// let mut value = 0;
    /// cosync.run_until_stall(&mut value);
    /// assert_eq!(value, 10);
    /// cosync.run_until_stall(&mut value);
    /// assert_eq!(value, 20);
    /// ```
    ///
    /// This function will not block the calling thread and will return the moment
    /// that there are no tasks left for which progress can be made;
    /// remaining incomplete tasks in the pool can continue with further use of one
    /// of the pool's run or poll methods. While the function is running, all tasks
    /// in the pool will try to make progress.
    pub fn run_until_stall(&mut self, parameter: &mut T) {
        // SAFETY: we own this box and make sure it's safe until we drop,
        // so the pointer is still valid if we have an `&mut self`
        unsafe {
            *self.data = Some(parameter as *mut _);
        }

        let _enter = enter().expect(
            "cannot execute `LocalPool` executor from within \
             another executor",
        );

        let _ = CURRENT_THREAD_NOTIFY.with(|thread_notify| {
            let waker = waker_ref::waker_ref(thread_notify);
            let mut cx = Context::from_waker(&waker);
            self.poll_pool(&mut cx)
        });

        // SAFETY: same as above, no one has deallocated this box
        unsafe {
            *self.data = None;
        }
    }

    // Make maximal progress on the entire pool of spawned task, returning `Ready`
    // if the pool is empty and `Pending` if no further progress can be made.
    fn poll_pool(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        // state for the FuturesUnordered, which will never be used
        loop {
            // try to execute the next ready future
            let ret = Pin::new(&mut self.pool).poll_next(cx);

            // no queued tasks; we may be done
            match ret {
                // this means an inner task is pending
                Poll::Pending => return Poll::Pending,
                // the pool was empty already, or we just completed that task.
                Poll::Ready(None) | Poll::Ready(Some(_)) => {
                    // grab our next task...
                    if let Some(task) = self.queue.lock().incoming.pop_front() {
                        self.pool.push(task)
                    } else {
                        return Poll::Ready(());
                    }
                }
            }
        }
    }
}

impl<T: 'static> Default for Cosync<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ?Sized> Drop for Cosync<T> {
    fn drop(&mut self) {
        // SAFETY: this is safe because we made this from
        // a box previously.
        unsafe {
            let _box = Box::from_raw(self.data);
        }
    }
}

#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<T: Send + ?Sized> Send for Cosync<T> {}
unsafe impl<T: Sync + ?Sized> Sync for Cosync<T> {}
impl<T> Unpin for Cosync<T> {}

/// A handle to spawn tasks.
///
/// # Examples
/// ```
/// # use cosync::Cosync;
/// let mut cosync = Cosync::new();
/// let handler = cosync.create_queue_handle();
///
/// // make a thread and join it...
/// std::thread::spawn(move || {
///     handler.queue(|mut input| async move {
///         *input.get() = 20;
///     });
/// })
/// .join()
/// .unwrap();
///
/// let mut value = 1;
/// cosync.run_blocking(&mut value);
/// assert_eq!(value, 20);
/// ```
#[derive(Debug)]
pub struct CosyncQueueHandle<T: ?Sized> {
    heap_ptr: *mut Option<*mut T>,
    queue: Weak<Mutex<IncomingQueue>>,
}

impl<T: 'static + ?Sized> CosyncQueueHandle<T> {
    /// Adds a new Task to the TaskQueue.
    ///
    /// A return of `None` indicates that the task failed to be queued because the
    /// corresponding `Cosync` has been dropped while a `CosyncQueueHandle` still exists.
    ///
    /// ```
    /// # use cosync::Cosync;
    ///
    /// let mut cosync = Cosync::<i32>::new();
    /// let mut queue_handle = cosync.create_queue_handle();
    ///
    /// let task_id = queue_handle.queue(|_| async {});
    /// assert!(task_id.is_some());
    ///
    /// drop(cosync);
    ///
    /// let task_id = queue_handle.queue(|_| async {});
    /// assert!(task_id.is_none());
    /// ```
    pub fn queue<Task, Out>(&self, task: Task) -> Option<CosyncTaskId>
    where
        Task: FnOnce(CosyncInput<T>) -> Out + Send + 'static,
        Out: Future<Output = ()> + Send,
    {
        self.queue.upgrade().map(|queue| {
            // force the future to move...
            let task = task;
            let sec = CosyncInput(CosyncQueueHandle {
                heap_ptr: self.heap_ptr,
                queue: self.queue.clone(),
            });

            let our_cb = Box::pin(async move {
                task(sec).await;
            });

            let mut mutex_lock = queue.lock();
            let id = CosyncTaskId(mutex_lock.counter);
            // note: we use a wrapping add here just so we don't panic on release mode for...
            // absurd numbers of tasks.
            mutex_lock.counter = mutex_lock.counter.wrapping_add(1);
            mutex_lock.incoming.push_back(FutureObject(our_cb, id));

            id
        })
    }

    /// Tries to force unqueueing a task.
    ///
    /// A return of `None` indicates that the task failed to be queued because the
    /// corresponding `Cosync` has been dropped while a `CosyncQueueHandle` still exists.
    ///
    /// If that task is already being ran, and you want to externally cancel it, run
    /// `Cosync::clear_running` instead.
    ///
    /// This returns `Some(true)` on success.
    pub fn unqueue_task(&mut self, task_id: CosyncTaskId) -> Option<bool> {
        self.queue.upgrade().map(|queue_handle| {
            let incoming = &mut queue_handle.lock().incoming;
            let Some(index) = incoming.iter().position(|future_obj| future_obj.1 == task_id) else { return false };
            incoming.remove(index);

            true
        })
    }
}

// safety:
// we guarantee with a kill counter that the main `.get` of CosyncInput
// never dereferences invalid data, and it's only made in the same thread
// as Cosync, so we should never have a problem with multithreaded access
// at the same time.
unsafe impl<T: ?Sized> Send for CosyncQueueHandle<T> {}
unsafe impl<T: ?Sized> Sync for CosyncQueueHandle<T> {}

impl<T: ?Sized> Clone for CosyncQueueHandle<T> {
    fn clone(&self) -> Self {
        Self {
            heap_ptr: self.heap_ptr,
            queue: self.queue.clone(),
        }
    }
}

/// A guarded pointer to create a [CosyncInputGuard] by [get] and to queue more tasks by [queue]
///
/// [queue]: Self::queue
/// [get]: Self::get
#[derive(Debug)]
pub struct CosyncInput<T: ?Sized>(CosyncQueueHandle<T>);

impl<T: 'static + ?Sized> CosyncInput<T> {
    /// Gets the underlying [CosyncInputGuard].
    pub fn get(&mut self) -> CosyncInputGuard<'_, T> {
        // if you find this guard, it means that you somehow moved the `CosyncInput` out of
        // the closure, and then dropped the `Cosync`. Why would you do that? Don't do that.
        assert!(Weak::strong_count(&self.0.queue) == 1, "cosync was dropped improperly");

        // SAFETY: we can always dereference this data, as we maintain
        // that it's always present.
        let operation = unsafe {
            let heap_ptr: *mut T = (*self.0.heap_ptr).expect("cosync was not initialized this run correctly");
            &mut *heap_ptr
        };

        CosyncInputGuard(operation, PhantomData)
    }

    /// Queues a new task. This goes to the back of queue.
    pub fn queue<Task, Out>(&self, task: Task) -> CosyncTaskId
    where
        Task: FnOnce(CosyncInput<T>) -> Out + Send + 'static,
        Out: Future<Output = ()> + Send,
    {
        // if you find this unwrap, it means that you somehow moved the `CosyncInput` out of
        // the closure, and then dropped the `Cosync`. Don't do that.
        self.0.queue(task).expect("cosync was dropped improperly")
    }

    /// Creates a queue handle which can be used to spawn tasks.
    pub fn create_queue_handle(&self) -> CosyncQueueHandle<T> {
        self.0.clone()
    }
}

// safety:
// we create `CosyncInput` per task, and it doesn't escape our closure.
// therefore, it's `*const` field should only be accessible when we know it's valid.
unsafe impl<T: ?Sized> Send for CosyncInput<T> {}
// safety:
// we create `CosyncInput` per task, and it doesn't escape our closure.
// therefore, it's `*const` field should only be accessible when we know it's valid.
unsafe impl<T: ?Sized> Sync for CosyncInput<T> {}

/// A guarded pointer.
///
/// This exists to prevent holding onto the `CosyncInputGuard` over `.await` calls. It will need to
/// be fetched again from [CosyncInput] after awaits.
pub struct CosyncInputGuard<'a, T: ?Sized>(&'a mut T, PhantomData<*const u8>);

impl<'a, T: ?Sized> ops::Deref for CosyncInputGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl<'a, T: ?Sized> ops::DerefMut for CosyncInputGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }
}

struct FutureObject(Pin<Box<dyn Future<Output = ()> + 'static>>, CosyncTaskId);
impl Future for FutureObject {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl fmt::Debug for FutureObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FutureObject").finish_non_exhaustive()
    }
}

/// Sleep the `Cosync` for a given number of calls to `run_until_stall`.
///
/// If you run `run_until_stall` once per tick in your main loop, then
/// this will sleep for that number of ticks.
pub fn sleep_ticks(ticks: usize) -> SleepForTick {
    SleepForTick::new(ticks)
}

/// A helper struct which registers a sleep for a given number of ticks.
#[must_use = "futures do nothing unless you `.await` or poll them"]
#[derive(Clone, Copy, Debug)]
#[doc(hidden)] // so users only see `sleep_ticks` above.
pub struct SleepForTick(pub usize);

impl SleepForTick {
    /// Sleep for the number of ticks provided.
    pub fn new(ticks: usize) -> Self {
        Self(ticks)
    }
}

impl Future for SleepForTick {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.0 == 0 {
            Poll::Ready(())
        } else {
            self.0 -= 1;

            // temp: this is relatively expensive.
            // we should be able to just register this at will
            cx.waker().wake_by_ref();

            Poll::Pending
        }
    }
}

/// An opaque task identifier, wrapping around a u64. You can use comparison operations to check
/// task id equality and task id age. In general, a task made later will have a greater identity.
///
/// *However,* this is implementing with `wrapping_add`, so if you make `usize::MAX` tasks, then the
/// "age" of a task will not be deducible from comparison operations; ie, you'll get a
/// `CosyncTaskId(0)` eventually. You will probably have ran out of RAM at that point, so in
/// practice, this won't come up.
///
/// ```
/// # use cosync::Cosync;
/// let mut cosync: Cosync<i32> = Cosync::new();
///
/// let first_task = cosync.queue(|_input| async {});
/// let second_task = cosync.queue(|_input| async {});
/// assert!(second_task > first_task);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct CosyncTaskId(usize);

impl CosyncTaskId {
    /// Creates a CosyncTaskId set to `usize::MAX`. Theoretically, this could refer to a valid task
    /// but would be exceedingly unlikely.
    pub const DANGLING: CosyncTaskId = CosyncTaskId(usize::MAX);
}

#[derive(Debug, Default)]
struct IncomingQueue {
    incoming: VecDeque<FutureObject>,
    counter: usize,
}

struct ThreadNotify {
    /// The (single) executor thread.
    thread: Thread,
    /// A flag to ensure a wakeup (i.e. `unpark()`) is not "forgotten"
    /// before the next `park()`, which may otherwise happen if the code
    /// being executed as part of the future(s) being polled makes use of
    /// park / unpark calls of its own, i.e. we cannot assume that no other
    /// code uses park / unpark on the executing `thread`.
    unparked: AtomicBool,
}

impl ArcWake for ThreadNotify {
    fn wake_by_ref(this: &Arc<Self>) {
        // Make sure the wakeup is remembered until the next `park()`.
        let unparked = this.unparked.swap(true, Ordering::Relaxed);
        if !unparked {
            // If the thread has not been unparked yet, it must be done
            // now. If it was actually parked, it will run again,
            // otherwise the token made available by `unpark`
            // may be consumed before reaching `park()`, but `unparked`
            // ensures it is not forgotten.
            this.thread.unpark();
        }
    }
}

thread_local! {
    static CURRENT_THREAD_NOTIFY: Arc<ThreadNotify> = Arc::new(ThreadNotify {
        thread: thread::current(),
        unparked: AtomicBool::new(false),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    static_assertions::assert_not_impl_all!(CosyncInputGuard<'_, i32>: Send, Sync);

    #[test]
    fn ordering() {
        let mut cosync = Cosync::new();

        let mut value = 0;
        cosync.queue(|_i| async move {
            println!("actual task body!");
        });
        cosync.run_until_stall(&mut value);
    }

    #[test]
    #[allow(clippy::needless_late_init)]
    fn pool_is_sequential() {
        // notice that value is declared here
        let mut value;

        let mut executor: Cosync<i32> = Cosync::new();
        executor.queue(move |mut input| async move {
            let mut input = input.get();

            assert_eq!(*input, 10);
            *input = 10;
        });

        executor.queue(move |mut input| async move {
            assert_eq!(*input.get(), 10);

            // this will make the executor sleep, stall,
            // and exit out of this tick
            // we call `run_until_stall` an additional time,
            // so we'll complete this 1 tick sleep.
            let sleep = SleepForTick(1);
            sleep.await;

            let input = &mut *input.get();
            assert_eq!(*input, 30);
            *input = 0;
        });

        // initialized here, after tasks are made
        // (so code is correctly being deferred)
        value = 10;
        executor.run_until_stall(&mut value);
        value = 30;
        executor.run_until_stall(&mut value);
        assert_eq!(value, 0);
    }

    #[test]
    fn run_until_stalled_stalls() {
        let mut cosync = Cosync::new();

        cosync.queue(move |mut input| async move {
            *input.get() = 10;
            // this will make the executor stall for a call
            // we call `run_until_stall` an additional time,
            // so we'll complete this 1 tick sleep.
            sleep_ticks(1).await;

            *input.get() = 20;
        });

        let mut value = 0;
        cosync.run_until_stall(&mut value);
        assert_eq!(value, 10);
        cosync.run_until_stall(&mut value);
        assert_eq!(value, 20);
    }

    #[test]
    #[allow(clippy::needless_late_init)]
    fn pool_remains_sequential() {
        // notice that value is declared here
        let mut value;

        let mut executor: Cosync<i32> = Cosync::new();
        executor.queue(move |mut input| async move {
            println!("starting task 1");
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
    #[allow(clippy::needless_late_init)]
    fn pool_is_still_sequential() {
        // notice that value is declared here
        let mut value;

        let mut executor: Cosync<i32> = Cosync::new();
        executor.queue(move |mut input| async move {
            println!("starting task 1");
            *input.get() = 10;

            input.queue(move |mut input| async move {
                println!("starting task 3");
                assert_eq!(*input.get(), 20);

                *input.get() = 30;
            });
        });

        executor.queue(move |mut input| async move {
            println!("starting task 2");
            *input.get() = 20;
        });

        // initialized here, after tasks are made
        // (so code is correctly being deferred)
        value = 0;
        executor.run_until_stall(&mut value);
        assert_eq!(value, 30);
    }

    #[test]
    #[allow(clippy::needless_late_init)]
    fn cosync_can_be_moved() {
        // notice that value is declared here
        let mut value;

        let mut executor: Cosync<i32> = Cosync::new();
        executor.queue(move |mut input| async move {
            println!("starting task 1");
            *input.get() = 10;

            sleep_ticks(1).await;

            *input.get() = 20;
        });

        // initialized here, after tasks are made
        // (so code is correctly being deferred)
        value = 0;
        executor.run_until_stall(&mut value);
        assert_eq!(value, 10);

        // move it somewhere else..
        let mut executor = Box::new(executor);
        executor.run_until_stall(&mut value);

        assert_eq!(value, 20);
    }

    #[test]
    #[should_panic(expected = "cosync was dropped improperly")]
    fn ub_on_move_is_prevented() {
        let (sndr, rx) = std::sync::mpsc::channel();
        let mut executor: Cosync<i32> = Cosync::new();

        executor.queue(move |input| async move {
            let sndr: std::sync::mpsc::Sender<_> = sndr;
            sndr.send(input).unwrap();
        });

        let mut value = 0;
        executor.run_blocking(&mut value);
        drop(executor);

        // the executor was dropped. whoopsie!
        let mut v = rx.recv().unwrap();
        *v.get() = 20;
    }

    #[test]
    #[should_panic(expected = "cosync was not initialized this run correctly")]
    fn ub2_on_move_is_prevented() {
        let (sndr, rx) = std::sync::mpsc::channel();
        let mut executor: Cosync<i32> = Cosync::new();

        executor.queue(move |input| async move {
            let sndr: std::sync::mpsc::Sender<_> = sndr;
            sleep_ticks(1).await;
            sndr.send(input).unwrap();
        });

        let mut value = 0;
        executor.run_blocking(&mut value);

        // the executor was dropped. whoopsie!
        let mut v = rx.recv().unwrap();
        *v.get() = 20;
    }

    #[test]
    #[cfg_attr(miri, ignore = "miri cannot handle threading yet")]
    fn threading() {
        let mut cosync = Cosync::new();
        let handler = cosync.create_queue_handle();

        // make a thread and join it...
        std::thread::spawn(move || {
            handler.queue(|mut input| async move {
                *input.get() = 20;
            });
        })
        .join()
        .unwrap();

        let mut value = 1;
        cosync.run_blocking(&mut value);
        assert_eq!(value, 20);
    }

    #[test]
    #[cfg_attr(miri, ignore = "miri will explode if we run this")]
    fn trybuild() {
        let t = trybuild::TestCases::new();
        t.compile_fail("tests/try_build/*.rs");
    }

    #[test]
    fn dynamic_dispatch() {
        trait DynDispatch {
            fn test(&self) -> i32;
        }

        impl DynDispatch for i32 {
            fn test(&self) -> i32 {
                *self
            }
        }

        impl DynDispatch for &'static str {
            fn test(&self) -> i32 {
                self.parse().unwrap()
            }
        }

        let mut cosync: Cosync<dyn DynDispatch> = Cosync::new();
        cosync.queue(|mut input: CosyncInput<dyn DynDispatch>| async move {
            {
                let inner: &mut dyn DynDispatch = &mut *input.get();
                assert_eq!(inner.test(), 3);
            }

            sleep_ticks(1).await;

            {
                let inner: &mut dyn DynDispatch = &mut *input.get();
                assert_eq!(inner.test(), 3);
            }
        });

        cosync.run_until_stall(&mut 3);
        cosync.run_until_stall(&mut "3");
    }

    #[test]
    fn unsized_type() {
        let mut cosync: Cosync<str> = Cosync::new();

        cosync.queue(|mut input| async move {
            let input_guard = input.get();
            let inner_str: &str = &input_guard;
            println!("inner str = {}", inner_str);
        });
    }

    #[test]
    fn can_move_non_copy() {
        let mut cosync: Cosync<i32> = Cosync::new();

        let my_vec = vec![10];

        cosync.queue(|_input| async move {
            let mut vec = my_vec;
            vec.push(10);

            assert_eq!(*vec, [10, 10]);
        });
    }

    #[test]
    fn task_id_numbers() {
        let mut cosync: Cosync<i32> = Cosync::new();

        let id = cosync.queue(|_input| async {});
        assert_eq!(id, CosyncTaskId(0));
        let second_task = cosync.queue(|_input| async {});
        assert!(second_task > id);
    }

    #[test]
    fn cancelling_a_task() {
        let mut cosync: Cosync<i32> = Cosync::new();

        cosync.queue(|mut input| async move {
            *input.get() += 1;

            sleep_ticks(1).await;

            *input.get() += 1;
        });

        let mut value = 0;

        cosync.run_until_stall(&mut value);

        assert_eq!(value, 1);

        cosync.clear_running();

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

    #[test]
    fn stop_a_running_task() {
        let mut cosync: Cosync<i32> = Cosync::new();

        let id = cosync.queue(|mut input| async move {
            *input.get() += 1;

            sleep_ticks(1).await;

            *input.get() += 1;
        });

        let mut value = 0;

        cosync.run_until_stall(&mut value);

        assert_eq!(value, 1);

        let success = cosync.stop_running_task(id);
        assert!(success);

        cosync.run_until_stall(&mut value);

        // it's still 1 because we cancelled the task which would have otherwise gotten it to 2
        assert_eq!(value, 1);
    }
}
