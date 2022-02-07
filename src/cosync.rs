use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    ops,
    pin::Pin,
    ptr::NonNull,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    thread,
};

use crate::{
    futures::{enter::enter, FuturesUnordered},
    thread_notify::ThreadNotify,
};

/// A single-threaded task pool for polling futures to completion.
///
/// This executor allows you to queue multiple tasks in sequence, and to
/// queue tasks within other tasks.
///
/// You can queue a task by using [queue](Cosync::queue), by spawning a [CosyncCoQueue]
/// and calling [queue](CosyncCoQueue::queue), or, within a task, calling
/// [queue_task](CosyncInput::queue) on [CosyncInput].
pub struct Cosync<T> {
    pool: FuturesUnordered<FutureObject>,
    incoming: Arc<Mutex<VecDeque<IncomingObject>>>,
    data: Box<Option<NonNull<T>>>,
}

/// Guarded Input.
///
/// Its primary role is to create a [CosyncInputGuard] by [get] and to queue more tasks by [queue]
///
/// [queue]: Self::queue
/// [get]: Self::get
pub struct CosyncInput<T> {
    heap_ptr: *const Option<NonNull<T>>,
    incoming: Arc<Mutex<VecDeque<IncomingObject>>>,
}

impl<T: 'static> CosyncInput<T> {
    /// Gets the underlying [CosyncInputGuard].
    pub fn get(&mut self) -> CosyncInputGuard<'_, T> {
        // we can always dereference this data, as we maintain
        // that it's always present.
        let box_ref = unsafe { &*self.heap_ptr };

        // when we unwrap this, we can also AsRef it
        let o = unsafe { box_ref.expect("cosync was not initialized this run correctly").as_mut() };

        CosyncInputGuard(o)
    }

    /// Queues a new task. This goes to the back of queue.
    pub fn queue<Task, Out>(&mut self, task: Task)
    where
        Task: Fn(CosyncInput<T>) -> Out + Send + 'static,
        Out: Future<Output = ()> + Send,
    {
        queue_task(task, self.heap_ptr, &self.incoming)
    }
}

unsafe impl<T> Send for CosyncInput<T> {}
unsafe impl<T> Sync for CosyncInput<T> {}

impl<T> fmt::Debug for CosyncInput<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CosyncInput")
            .field(&self.heap_ptr)
            .field(&"IncomingQueue")
            .finish()
    }
}

/// A guarded pointer.
///
/// This exists to prevent holding onto the `CosyncInputGuard` over `.await` calls. It will need to
/// be fetched again from [CosyncInput] after awaits.
pub struct CosyncInputGuard<'a, T>(&'a mut T);

impl<'a, T> ops::Deref for CosyncInputGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl<'a, T> ops::DerefMut for CosyncInputGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }
}

impl<T: 'static> Cosync<T> {
    /// Create a new, empty pool of tasks.
    pub fn new() -> Self {
        Self {
            pool: FuturesUnordered::new(),
            incoming: Default::default(),
            data: Box::new(None),
        }
    }

    /// Adds a new Task to the TaskQueue.
    pub fn queue<Task, Out>(&mut self, task: Task)
    where
        Task: Fn(CosyncInput<T>) -> Out + Send + 'static,
        Out: Future<Output = ()> + Send,
    {
        let position = &*self.data as *const Option<_>;

        queue_task(task, position, &self.incoming);
    }

    /// Run all tasks in the pool to completion.
    ///
    /// ```
    /// use futures::executor::LocalPool;
    ///
    /// let mut pool = LocalPool::new();
    ///
    /// // ... spawn some initial tasks using `spawn.spawn()` or `spawn.spawn_local()`
    ///
    /// // run *all* tasks in the pool to completion, including any newly-spawned ones.
    /// pool.run();
    /// ```
    ///
    /// The function will block the calling thread until *all* tasks in the pool
    /// are complete, including any spawned while running existing tasks.
    pub fn run(&mut self, parameter: &mut T) {
        // hoist the T:
        unsafe {
            *self.data = Some(NonNull::new_unchecked(parameter as *mut _));
        }

        run_executor(|cx| self.poll_pool(cx));

        // for segfault help, we null here
        *self.data = None;
    }

    /// Runs all tasks in the queue and returns if no more progress can be made
    /// on any task.
    ///
    /// ```
    /// use futures::{
    ///     executor::LocalPool,
    ///     future::{pending, ready},
    ///     task::LocalSpawnExt,
    /// };
    ///
    /// let mut pool = LocalPool::new();
    /// let spawner = pool.spawner();
    ///
    /// spawner.spawn_local(ready(())).unwrap();
    /// spawner.spawn_local(ready(())).unwrap();
    /// spawner.spawn_local(pending()).unwrap();
    ///
    /// // Runs the two ready task and returns.
    /// // The empty task remains in the pool.
    /// pool.run_until_stalled();
    /// ```
    ///
    /// This function will not block the calling thread and will return the moment
    /// that there are no tasks left for which progress can be made;
    /// remaining incomplete tasks in the pool can continue with further use of one
    /// of the pool's run or poll methods. While the function is running, all tasks
    /// in the pool will try to make progress.
    pub fn run_until_stalled(&mut self, parameter: &mut T) {
        // hoist the T:
        unsafe {
            *self.data = Some(NonNull::new_unchecked(parameter as *mut _));
        }

        poll_executor(|ctx| {
            let _ = self.poll_pool(ctx);
        });

        // null it
        *self.data = None;
    }

    // Make maximal progress on the entire pool of spawned task, returning `Ready`
    // if the pool is empty and `Pending` if no further progress can be made.
    fn poll_pool(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        // state for the FuturesUnordered, which will never be used
        loop {
            let ret = self.poll_pool_once(cx);

            // we queued up some new tasks; add them and poll again
            // if !self.incoming.is_empty() {
            //     continue;
            // }

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
            if let Some(task) = self.incoming.lock().unwrap().pop_front() {
                unsafe { self.pool.push(task.into_future_object()) }
            }
        }

        // try to execute the next ready future
        Pin::new(&mut self.pool).poll_next(cx)
    }
}

impl<T: 'static> Default for Cosync<T> {
    fn default() -> Self {
        Self::new()
    }
}

type IncomingOutput = Pin<Box<dyn Future<Output = ()> + 'static>>;
type IncomingInner = Box<dyn FnOnce() -> IncomingOutput + Send + 'static>;

struct IncomingObject(IncomingInner);
impl IncomingObject {
    /// This should only be done once `data` is valid.
    pub unsafe fn into_future_object(self) -> FutureObject {
        FutureObject(self.0())
    }
}

struct FutureObject(Pin<Box<dyn Future<Output = ()> + 'static>>);
impl Future for FutureObject {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
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
        let waker = futures::task::waker_ref(thread_notify);
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
        let waker = futures::task::waker_ref(thread_notify);
        let mut cx = Context::from_waker(&waker);
        f(&mut cx)
    })
}

/// Adds a new Task to the TaskQueue.
fn queue_task<T: 'static, Task, Out>(
    task: Task,
    heap_ptr: *const Option<NonNull<T>>,
    incoming: &Arc<Mutex<VecDeque<IncomingObject>>>,
) where
    Task: Fn(CosyncInput<T>) -> Out + Send + 'static,
    Out: Future<Output = ()> + Send,
{
    // force the future to move...
    let task = task;
    let sec = CosyncInput {
        heap_ptr,
        incoming: incoming.clone(),
    };

    let our_cb = Box::new(move || {
        let output = Box::pin(async move {
            task(sec).await;
        });

        // force the typings to upcast it...
        output as IncomingOutput
    }) as IncomingInner;

    incoming.lock().unwrap().push_back(IncomingObject(our_cb));
}

/// Sleep for a given number of ticks.
pub fn sleep_ticks(ticks: usize) -> SleepForTick {
    SleepForTick::new(ticks)
}

/// A helper struct which registers a sleep for a given number of ticks.
#[derive(Clone, Copy, Debug)]
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

            cx.waker().wake_by_ref();

            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
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
            // we call `run_until_stalled` an additional time,
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
        executor.run_until_stalled(&mut value);
        value = 30;
        executor.run_until_stalled(&mut value);
        assert_eq!(value, 0);
    }

    #[test]
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
        executor.run_until_stalled(&mut value);
    }

    #[test]
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
        executor.run_until_stalled(&mut value);
        assert_eq!(value, 30);
    }

    // THIS SHOULD NOT COMPILE!!
    // #[test]
    // fn do_not_hold_ctx_through_await() {
    //     let mut executor: SingleExecutor<i32> = SingleExecutor::new();
    //     executor.add_task(move |mut input| async move {
    //         let inner_input = input.as_ref_mut();
    //         let inner_inner_input: &mut i32 = &mut inner_input;
    //         assert_eq!(*inner_inner_input, 10);

    //         let sleep = SleepForTick(1);
    //         sleep.await;

    //         let input = inner_inner_input;

    //         assert_eq!(*input, 100);
    //     });

    //     let mut one_value = 10;
    //     executor.run_until_stalled(&mut one_value);
    //     let mut two_value = 100;
    //     executor.run_until_stalled(&mut two_value);
    // }
}
