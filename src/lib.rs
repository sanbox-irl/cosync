#![doc = include_str!("../README.md")]
#![deny(rust_2018_idioms)]
#![deny(missing_docs)]
#![deny(rustdoc::all)]

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
    incoming: Arc<Mutex<VecDeque<FutureObject>>>,
    data: *mut Option<*mut T>,
}

impl<T: 'static + ?Sized> Cosync<T> {
    /// Create a new, empty Cosync.
    pub fn new() -> Self {
        Self {
            pool: FuturesUnordered::new(),
            incoming: Default::default(),
            data: Box::into_raw(Box::new(None)),
        }
    }

    /// Returns the number of tasks queued. This *includes* the task currently being executed. Use
    /// [is_executing] to see if there is a task currently being executed (ie, it returned `Pending`
    /// at some point in its execution).
    ///
    /// [is_executing]: Self::is_executing
    pub fn len(&self) -> usize {
        let one = self.is_executing() as usize;

        one + self.incoming.lock().len()
    }

    /// Returns true if no futures are being executed *and* there are no futures in the queue.
    pub fn is_empty(&self) -> bool {
        !self.is_executing() && self.incoming.lock().is_empty()
    }

    /// Returns true if `cosync` has a `Pending` future. It is possible for
    /// the `cosync` to have no `Pending` future, but to have tasks queued still.
    pub fn is_executing(&self) -> bool {
        !self.pool.is_empty()
    }

    /// Creates a queue handle which can be used to spawn tasks.
    pub fn create_queue_handle(&self) -> CosyncQueueHandle<T> {
        CosyncQueueHandle {
            heap_ptr: self.data,
            incoming: Arc::downgrade(&self.incoming),
        }
    }

    /// Adds a new Task to the TaskQueue.
    pub fn queue<Task, Out>(&mut self, task: Task)
    where
        Task: FnOnce(CosyncInput<T>) -> Out + Send + 'static,
        Out: Future<Output = ()> + Send,
    {
        let queue_handle = self.create_queue_handle();

        queue_handle.queue(task)
    }

    /// Run all tasks in the queue to completion. You probably want `run_until_stall`.
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

        run_executor(|cx| self.poll_pool(cx));

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

        poll_executor(|ctx| {
            let _output = self.poll_pool(ctx);
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
            let ret = self.poll_pool_once(cx);

            // no queued tasks; we may be done
            match ret {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(()),
                _ => {}
            }
        }
    }

    // Try make minimal progress on the pool of spawned tasks
    fn poll_pool_once(&mut self, cx: &mut Context<'_>) -> Poll<Option<()>> {
        // grab our next task...
        if self.pool.is_empty() {
            if let Some(task) = self.incoming.lock().pop_front() {
                self.pool.push(task)
            }
        }

        // try to execute the next ready future
        Pin::new(&mut self.pool).poll_next(cx)
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
    incoming: Weak<Mutex<VecDeque<FutureObject>>>,
}

impl<T: 'static + ?Sized> CosyncQueueHandle<T> {
    /// Adds a new Task to the TaskQueue.
    pub fn queue<Task, Out>(&self, task: Task)
    where
        Task: FnOnce(CosyncInput<T>) -> Out + Send + 'static,
        Out: Future<Output = ()> + Send,
    {
        if let Some(incoming) = self.incoming.upgrade() {
            // force the future to move...
            let task = task;
            let sec = CosyncInput(CosyncQueueHandle {
                heap_ptr: self.heap_ptr,
                incoming: self.incoming.clone(),
            });

            let our_cb = Box::pin(async move {
                task(sec).await;
            });

            incoming.lock().push_back(FutureObject(our_cb));
        }
    }
}

// safety:
// we guarantee with a kill counter that the main `.get` of CosyncInput
// never dereferences invalid data, and it's only made in the same thread
// as Cosync, so we should never have a problem with multithreaded access
// at the same time.
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<T: ?Sized> Send for CosyncQueueHandle<T> {}
unsafe impl<T: ?Sized> Sync for CosyncQueueHandle<T> {}

impl<T: ?Sized> Clone for CosyncQueueHandle<T> {
    fn clone(&self) -> Self {
        Self {
            heap_ptr: self.heap_ptr,
            incoming: self.incoming.clone(),
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
        assert!(
            Weak::strong_count(&self.0.incoming) == 1,
            "cosync was dropped improperly"
        );

        // SAFETY: we can always dereference this data, as we maintain
        // that it's always present.
        let operation = unsafe {
            let heap_ptr: *mut T = (*self.0.heap_ptr).expect("cosync was not initialized this run correctly");
            &mut *heap_ptr
        };

        CosyncInputGuard(operation, PhantomData)
    }

    /// Queues a new task. This goes to the back of queue.
    pub fn queue<Task, Out>(&self, task: Task)
    where
        Task: FnOnce(CosyncInput<T>) -> Out + Send + 'static,
        Out: Future<Output = ()> + Send,
    {
        self.0.queue(task)
    }

    /// Creates a queue handle which can be used to spawn tasks.
    pub fn create_queue_handle(&self) -> CosyncQueueHandle<T> {
        self.0.clone()
    }
}

// safety:
// we create `CosyncInput` per task, and it doesn't escape our closure.
// therefore, it's `*const` field should only be accessible when we know it's valid.
#[allow(clippy::non_send_fields_in_send_ty)]
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

impl<T: 'static> Default for Cosync<T> {
    fn default() -> Self {
        Self::new()
    }
}

struct FutureObject(Pin<Box<dyn Future<Output = ()> + 'static>>);
impl Future for FutureObject {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl fmt::Debug for FutureObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FutureObject").finish()
    }
}

pub(crate) struct ThreadNotify {
    /// The (single) executor thread.
    pub thread: Thread,
    /// A flag to ensure a wakeup (i.e. `unpark()`) is not "forgotten"
    /// before the next `park()`, which may otherwise happen if the code
    /// being executed as part of the future(s) being polled makes use of
    /// park / unpark calls of its own, i.e. we cannot assume that no other
    /// code uses park / unpark on the executing `thread`.
    pub unparked: AtomicBool,
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

// Set up and run a basic single-threaded spawner loop, invoking `f` on each
// turn.
fn run_executor<T, F>(mut work_on_future: F) -> T
where
    F: FnMut(&mut Context<'_>) -> Poll<T>,
{
    let _enter = enter().expect(
        "cannot execute `LocalPool` executor from within \
         another executor",
    );

    CURRENT_THREAD_NOTIFY.with(|thread_notify| {
        let waker = waker_ref::waker_ref(thread_notify);
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(t) = work_on_future(&mut cx) {
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
    })
}

fn poll_executor<T, F: FnMut(&mut Context<'_>) -> T>(mut f: F) -> T {
    let _enter = enter().expect(
        "cannot execute `LocalPool` executor from within \
         another executor",
    );

    CURRENT_THREAD_NOTIFY.with(|thread_notify| {
        let waker = waker_ref::waker_ref(thread_notify);
        let mut cx = Context::from_waker(&waker);
        f(&mut cx)
    })
}

/// Sleep the `Cosync` for a given number of calls to `run_until_stall`.
///
/// If you run `run_until_stall` once per tick in your main loop, then
/// this will sleep for that number of ticks.
/// If you run `run`
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

/// The `Cosync` struct from which this `CosyncQueueHandle` was created was dropped already,
/// so this task was not queued.
#[derive(Debug)]
pub struct CosyncWasDropped;

impl fmt::Display for CosyncWasDropped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("the `Cosync` for this `CosyncQueueHandle` was dropped already. task was not queued")
    }
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
}
