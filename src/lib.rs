#![doc = include_str!("../README.md")]
#![deny(rust_2018_idioms)]
#![deny(missing_docs)]
#![warn(clippy::dbg_macro)]
#![cfg_attr(not(test), warn(clippy::print_stdout))]
#![warn(clippy::missing_errors_doc)]
#![warn(clippy::missing_panics_doc)]
#![warn(clippy::todo)]
#![deny(rustdoc::broken_intra_doc_links)]

// this is vendored code from the `futures-rs` crate, to avoid
// having a huge dependency when we only need a little bit
mod futures;
use crate::futures::{enter::enter, waker_ref, ArcWake, FuturesUnordered};

mod serial;
pub use serial::SerialCosync;

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
        self.pool.len() + unlock_mutex(&self.queue).incoming.len()
    }

    /// Returns true if no futures are being executed *and* there are no futures in the queue.
    pub fn is_empty(&self) -> bool {
        !self.is_running_any() && unlock_mutex(&self.queue).incoming.is_empty()
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

    /// Adds a new Task into the Pool directly. This does not queue it at all, for the sake of
    /// saving a mutex unlock call.
    pub fn queue<Task, Out>(&mut self, task: Task) -> CosyncTaskId
    where
        Task: FnOnce(CosyncInput<T>) -> Out + Send + 'static,
        Out: Future<Output = ()> + Send,
    {
        let cosync_input = CosyncInput(self.create_queue_handle());
        let id = next_cosync_task_id(&self.queue);
        self.pool.push(create_future_object(task, cosync_input, id));

        id
    }

    /// Run all tasks in the queue to completion. If a task won't complete, this will infinitely
    /// retry it. You probably want `run`, which returns once a task returns
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
        run_blocking(self.data, parameter, |ctx| self.poll_pool(ctx));
    }

    /// Runs all tasks in the queue and returns if no more progress can be made
    /// on any task.
    ///
    /// ```
    /// use cosync::{yield_now, Cosync};
    ///
    /// let mut cosync = Cosync::new();
    /// cosync.queue(move |mut input| async move {
    ///     *input.get() = 10;
    ///     // this will make the executor stall for a call
    ///     yield_now().await;
    ///
    ///     *input.get() = 20;
    /// });
    ///
    /// let mut value = 0;
    /// 
    /// // this will run until the `.await`
    /// cosync.run(&mut value);
    /// assert_eq!(value, 10);
    ///
    /// // we call `run` an additional time
    /// cosync.run(&mut value);
    /// assert_eq!(value, 20);
    /// ```
    ///
    /// This function will not block the calling thread and will return the moment
    /// that there are no tasks left for which progress can be made;
    /// remaining incomplete tasks in the pool can continue with further use of one
    /// of the pool's run or poll methods. While the function is running, all tasks
    /// in the pool will try to make progress.
    pub fn run(&mut self, parameter: &mut T) {
        run(self.data, parameter, |ctx| self.poll_pool(ctx))
    }

    #[deprecated(note = "use `run` instead", since = "0.3.0")]
    #[doc(hidden)]
    pub fn run_until_stall(&mut self, parameter: &mut T) {
        self.run(parameter)
    }

    fn poll_pool(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        // dump all the tasks in
        {
            let mut queue = unlock_mutex(&self.queue);
            for task in queue.incoming.drain(..) {
                self.pool.push(task);
            }
        }

        self.pool.increment_counter();
        loop {
            let pinned_pool = Pin::new(&mut self.pool);
            let output = pinned_pool.poll_next(cx);

            // do we have new tasks? we can take care of that now too
            let mut queue = unlock_mutex(&self.queue);
            if queue.incoming.is_empty() {
                break output;
            }

            // if we have new tasks, time to go around the horn again
            for task in queue.incoming.drain(..) {
                self.pool.push(task);
            }
        }
    }
}

impl<T: ?Sized + 'static> Default for Cosync<T> {
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
        self.queue.upgrade().map(|queue| raw_queue(&queue, task, self.heap_ptr))
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
            let incoming = &mut unlock_mutex(&queue_handle).incoming;
            let Some(index) = incoming.iter().position(|future_obj| future_obj.1 == task_id) else { return false };
            incoming.remove(index);

            true
        })
    }
}

fn run_blocking<T: ?Sized + 'static>(
    heap_ptr: *mut Option<*mut T>,
    parameter: &mut T,
    mut cback: impl FnMut(&mut Context<'_>) -> Poll<()>,
) {
    // SAFETY: we own this box and make sure it's safe until we drop,
    // so the pointer is still valid if we have an `&mut self`
    unsafe {
        *heap_ptr = Some(parameter as *mut _);
    }

    let _enter = enter().expect(
        "cannot execute `LocalPool` executor from within \
         another executor",
    );

    CURRENT_THREAD_NOTIFY.with(|thread_notify| {
        let waker = waker_ref::waker_ref(thread_notify);
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(t) = cback(&mut cx) {
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
        *heap_ptr = None;
    }
}

fn run<T: ?Sized>(
    heap_ptr: *mut Option<*mut T>,
    parameter: &mut T,
    mut cback: impl FnMut(&mut Context<'_>) -> Poll<()>,
) {
    // SAFETY: we own this box and make sure it's safe until we drop,
    // so the pointer is still valid if we have an `&mut self`
    unsafe {
        *heap_ptr = Some(parameter as *mut _);
    }

    let _enter = enter().expect(
        "cannot execute `LocalPool` executor from within \
         another executor",
    );

    let _ = CURRENT_THREAD_NOTIFY.with(|thread_notify| {
        let waker = waker_ref::waker_ref(thread_notify);
        let mut cx = Context::from_waker(&waker);
        cback(&mut cx)
    });

    // SAFETY: same as above, no one has deallocated this box
    unsafe {
        *heap_ptr = None;
    }
}

fn raw_queue<T, Task, Out>(queue: &Arc<Mutex<IncomingQueue>>, task: Task, heap_ptr: *mut Option<*mut T>) -> CosyncTaskId
where
    T: ?Sized + 'static,
    Task: FnOnce(CosyncInput<T>) -> Out + Send + 'static,
    Out: Future<Output = ()> + Send,
{
    // force the future to move...
    let cosync_input = CosyncInput(CosyncQueueHandle {
        heap_ptr,
        queue: Arc::downgrade(queue),
    });

    let id = next_cosync_task_id(queue);
    let future_object = create_future_object(task, cosync_input, id);

    let mut mutex_lock = unlock_mutex(queue);
    mutex_lock.incoming.push_back(future_object);

    id
}

fn next_cosync_task_id(m: &Arc<Mutex<IncomingQueue>>) -> CosyncTaskId {
    let mut lock = unlock_mutex(m);
    let id = CosyncTaskId(lock.counter);
    lock.counter = lock.counter.wrapping_add(1);

    id
}

fn create_future_object<T, Task, Out>(task: Task, cosync_input: CosyncInput<T>, id: CosyncTaskId) -> FutureObject
where
    T: ?Sized + 'static,
    Task: FnOnce(CosyncInput<T>) -> Out + Send + 'static,
    Out: Future<Output = ()> + Send,
{
    let our_cb = Box::pin(async move {
        task(cosync_input).await;
    });

    FutureObject(our_cb, id)
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
    ///
    /// # Panics
    ///
    /// This can panic if you move the `CosyncInput` out of a closure (with, eg an mspc channel). Do
    /// not do that.
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

/// Sleep the `Cosync` for a given number of calls to `run`.
///
/// If you run `run` once per tick in your main loop, then
/// this will sleep for that number of ticks.
#[inline]
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

/// Immediately yields control back, returning a `Pending`. The next time the future is
/// polled, it will continue. This is semantically equivalent to `yield_now()`, but more
/// performant.
#[inline]
pub fn yield_now() -> Yield {
    Yield(false)
}

/// Helper struct for yielding for a tick.
#[must_use = "futures do nothing unless you `.await` or poll them"]
#[derive(Clone, Copy, Debug)]
#[doc(hidden)] // so users only see `yield_now`
pub struct Yield(bool);

impl Future for Yield {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.0 {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(())
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
pub struct CosyncTaskId(u64);

impl CosyncTaskId {
    /// Creates a CosyncTaskId set to `usize::MAX`. Theoretically, this could refer to a valid task
    /// but would be exceedingly unlikely.
    pub const DANGLING: CosyncTaskId = CosyncTaskId(u64::MAX);
}

#[derive(Debug, Default)]
struct IncomingQueue {
    incoming: VecDeque<FutureObject>,
    counter: u64,
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

#[cfg(feature = "parking_lot")]
type Mutex<T> = parking_lot::Mutex<T>;
#[cfg(feature = "parking_lot")]
fn unlock_mutex<T>(mutex: &Mutex<T>) -> parking_lot::MutexGuard<'_, T> {
    mutex.lock()
}

#[cfg(not(feature = "parking_lot"))]
type Mutex<T> = std::sync::Mutex<T>;

#[cfg(not(feature = "parking_lot"))]
fn unlock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap()
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
        cosync.run(&mut value);
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
            // we call `run` an additional time,
            // so we'll complete this 1 tick sleep.
            yield_now().await;

            let input = &mut *input.get();
            assert_eq!(*input, 30);
            *input = 0;
        });

        // initialized here, after tasks are made
        // (so code is correctly being deferred)
        value = 10;
        executor.run(&mut value);
        value = 30;
        executor.run(&mut value);
        assert_eq!(value, 0);
    }

    #[test]
    #[allow(clippy::needless_late_init)]
    fn multi_pool_is_parallel() {
        // notice that value is declared here
        let mut value;

        let mut executor: Cosync<(i32, i32)> = Cosync::new();
        executor.queue(move |mut input| async move {
            let mut input = input.get();

            assert_eq!((*input).0, 10);
            (*input).0 = 30;
        });

        executor.queue(move |mut input| async move {
            let mut input = input.get();

            assert_eq!((*input).1, 20);
            (*input).1 = 20;
        });

        // initialized here, after tasks are made
        // (so code is correctly being deferred)
        value = (10, 20);
        executor.run(&mut value);
        assert_eq!(value, (30, 20));
    }

    #[test]
    fn run_stalls() {
        let mut cosync = Cosync::new();

        cosync.queue(move |mut input| async move {
            *input.get() = 10;
            // this will make the executor stall for a call
            // we call `run` an additional time,
            // so we'll complete this 1 tick sleep.
            yield_now().await;

            *input.get() = 20;
        });

        let mut value = 0;
        cosync.run(&mut value);
        assert_eq!(value, 10);
        cosync.run(&mut value);
        assert_eq!(value, 20);
    }

    #[test]
    fn run_multiple() {
        // notice that value is declared here
        let mut value = (10, 20);

        let mut executor: Cosync<(i32, i32)> = Cosync::new();
        executor.queue(move |mut input| async move {
            {
                let mut input_guard = input.get();

                assert_eq!((*input_guard).0, 10);
                (*input_guard).0 = 30;
            }

            yield_now().await;

            {
                let mut input_guard = input.get();
                (*input_guard).0 = 40;
            }
        });

        executor.queue(move |mut input| async move {
            {
                let mut input = input.get();

                assert_eq!((*input).1, 20);
                (*input).1 = 20;
            }

            yield_now().await;

            {
                let mut input = input.get();

                (*input).1 = 40;
            }
        });

        executor.run(&mut value);
        assert_eq!(value, (30, 20));

        executor.run(&mut value);
        assert_eq!(value, (40, 40));
    }

    #[test]
    fn run_concurrent_weird() {
        // notice that value is declared here
        let mut value = (10, 20, 30);

        let mut executor: Cosync<(i32, i32, i32)> = Cosync::new();

        executor.queue(move |mut input| async move {
            {
                let mut input = input.get();

                assert_eq!(input.2, 30);
                input.2 = 20;
            }

            yield_now().await;

            input.get().2 = 30;

            yield_now().await;

            input.get().2 = 40;
        });

        executor.queue(move |mut input| async move {
            {
                let mut input_guard = input.get();

                assert_eq!((*input_guard).0, 10);
                (*input_guard).0 = 30;
            }

            yield_now().await;

            {
                let mut input_guard = input.get();
                (*input_guard).0 = 40;
            }
        });

        executor.queue(move |mut input| async move {
            {
                let mut input = input.get();

                assert_eq!((*input).1, 20);
                (*input).1 = 20;
            }

            yield_now().await;

            {
                let mut input = input.get();

                (*input).1 = 40;
            }
        });

        executor.run(&mut value);
        assert_eq!(value, (30, 20, 20));

        executor.run(&mut value);
        assert_eq!(value, (40, 40, 30));

        executor.run(&mut value);
        assert_eq!(value, (40, 40, 40));
    }

    #[test]
    fn pool_remains_sequential_multi() {
        let mut value = 0;

        let mut executor: Cosync<i32> = Cosync::new();
        executor.queue(move |mut input| async move {
            *input.get() = 10;

            input.queue(|mut input| async move {
                println!("running this guy!!");

                *input.get() = 20;

                input.queue(|mut input| async move {
                    println!("running final guy!!");

                    *input.get() = 30;
                });
            });
        });

        executor.run(&mut value);

        assert_eq!(value, 30);
    }

    #[test]
    fn multi_sequential_on_spawn() {
        struct TestMe {
            one: i32,
            two: i32,
        }

        let mut value = TestMe { one: 0, two: 0 };

        let mut executor: Cosync<TestMe> = Cosync::new();
        executor.queue(move |mut input| async move {
            input.get().one = 10;

            input.queue(|mut input| async move {
                println!("running?/");
                input.get().two = 20;
            });

            yield_now().await;
        });

        executor.run(&mut value);

        assert_eq!(value.one, 10);
        assert_eq!(value.two, 20);
    }

    #[test]
    #[allow(clippy::needless_late_init)]
    fn pool_is_still_sequential() {
        // notice that value is declared here
        let mut value;

        let mut executor: Cosync<i32> = Cosync::new();
        executor.queue(move |mut input| async move {
            *input.get() = 10;

            input.queue(move |mut input| async move {
                assert_eq!(*input.get(), 20);

                *input.get() = 30;
            });
        });

        executor.queue(move |mut input| async move {
            *input.get() = 20;
        });

        // initialized here, after tasks are made
        // (so code is correctly being deferred)
        value = 0;
        executor.run(&mut value);
        assert_eq!(value, 30);
    }

    #[test]
    #[allow(clippy::needless_late_init)]
    fn cosync_can_be_moved() {
        // notice that value is declared here
        let mut value;

        let mut executor: Cosync<i32> = Cosync::new();
        executor.queue(move |mut input| async move {
            *input.get() = 10;

            yield_now().await;

            *input.get() = 20;
        });

        // initialized here, after tasks are made
        // (so code is correctly being deferred)
        value = 0;
        executor.run(&mut value);
        assert_eq!(value, 10);

        // move it somewhere else..
        let mut executor = Box::new(executor);
        executor.run(&mut value);

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
            yield_now().await;
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

            yield_now().await;

            {
                let inner: &mut dyn DynDispatch = &mut *input.get();
                assert_eq!(inner.test(), 3);
            }
        });

        cosync.run(&mut 3);
        cosync.run(&mut "3");
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
    fn stop_a_running_task() {
        let mut cosync: Cosync<i32> = Cosync::new();

        let id = cosync.queue(|mut input| async move {
            *input.get() += 1;

            yield_now().await;

            *input.get() += 1;
        });

        let mut value = 0;

        cosync.run(&mut value);

        assert_eq!(value, 1);

        let success = cosync.stop_running_task(id);
        assert!(success);

        cosync.run(&mut value);

        // it's still 1 because we cancelled the task which would have otherwise gotten it to 2
        assert_eq!(value, 1);
    }

    #[test]
    fn sleep_ticker() {
        let mut cosync: Cosync<i32> = Cosync::new();

        let mut value = 0;
        cosync.queue(|mut input| async move {
            sleep_ticks(5).await;
            *input.get() = 10;
        });

        for _ in 0..5 {
            cosync.run(&mut value);
            assert_eq!(value, 0);
        }
        cosync.run(&mut value);
        assert_eq!(value, 10);
    }
}
