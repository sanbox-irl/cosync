// #![deny(rust_2018_idioms)]
// #![allow(clippy::bool_comparison)]

use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    marker::PhantomData,
    ops,
    pin::Pin,
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    thread::{self, Thread},
};

use futures::task::{waker_ref, ArcWake};

use super::futures::{enter::enter, FuturesUnordered};

/// A single-threaded task pool for polling futures to completion.
///
/// This executor allows you to queue multiple tasks in sequence, and to
/// queue tasks within other tasks.
///
/// You can queue a task by using [queue](Cosync::queue), by spawning a [CosyncCoQueue]
/// and calling [queue](CosyncCoQueue::queue), or, within a task, calling [queue_task](CosyncInput::queue)
/// on [CosyncInput].
pub struct Cosync<T> {
    pool: FuturesUnordered<FutureObject>,
    incoming: Arc<Mutex<VecDeque<IncomingObject>>>,
    data: Box<*mut ()>,
    __parameter_type: PhantomData<T>,
}

/// Guarded Input.
///
/// Its primary role is to create a [CosyncInputGuard] by [get] and to queue
/// more tasks by [queue]
///
/// #add_task: Self::queue
/// #get: Self::get
#[derive(Debug)]
pub struct CosyncInput<T> {
    heap_ptr: *mut *mut (),
    stack: Arc<Mutex<VecDeque<IncomingObject>>>,
    __parameter_type: PhantomData<T>,
}

impl<T> CosyncInput<T> {
    /// Gets the underlying [CosyncInputGuard]
    pub fn get(&mut self) -> CosyncInputGuard<'_, T> {
        // we can always dereference this data, as we maintain
        // that it's always present.
        let inner = unsafe { &mut *(*self.heap_ptr as *mut T) };

        CosyncInputGuard(inner)
    }

    /// Adds a new Task to the TaskQueue.
    pub fn queue<Task, Out>(&mut self, task: Task)
    where
        Task: Fn(CosyncInput<T>) -> Out + Send + 'static,
        Out: Future<Output = ()> + Send,
    {
        queue_task(task, unsafe { &mut *self.heap_ptr }, &self.stack)
    }
}

unsafe impl<T> Send for CosyncInput<T> {}
unsafe impl<T> Sync for CosyncInput<T> {}

/// A guarded pointer. This exists to prevent holding onto
/// the `CosyncInputGuard` over `.await` calls. It will need
/// to be fetched again from [CosyncInput] after awaits.
#[derive(Debug)]
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

type IncomingOutput = Pin<Box<dyn Future<Output = ()> + 'static>>;
type IncomingInner = Box<dyn FnOnce() -> IncomingOutput + Send + 'static>;

struct IncomingObject(IncomingInner);
impl IncomingObject {
    /// This should only be done once `data` is valid.
    pub unsafe fn into_future_object(self) -> FutureObject {
        FutureObject(self.0())
    }
}

impl fmt::Debug for IncomingObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IncomingObject").finish_non_exhaustive()
    }
}

struct FutureObject(Pin<Box<dyn Future<Output = ()> + 'static>>);
impl Future for FutureObject {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

pub(crate) struct ThreadNotify {
    /// The (single) executor thread.
    thread: Thread,
    /// A flag to ensure a wakeup (i.e. `unpark()`) is not "forgotten"
    /// before the next `park()`, which may otherwise happen if the code
    /// being executed as part of the future(s) being polled makes use of
    /// park / unpark calls of its own, i.e. we cannot assume that no other
    /// code uses park / unpark on the executing `thread`.
    unparked: AtomicBool,
}

thread_local! {
    static CURRENT_THREAD_NOTIFY: Arc<ThreadNotify> = Arc::new(ThreadNotify {
        thread: thread::current(),
        unparked: AtomicBool::new(false),
    });
}

impl ArcWake for ThreadNotify {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        // Make sure the wakeup is remembered until the next `park()`.
        let unparked = arc_self.unparked.swap(true, Ordering::Relaxed);
        if !unparked {
            // If the thread has not been unparked yet, it must be done
            // now. If it was actually parked, it will run again,
            // otherwise the token made available by `unpark`
            // may be consumed before reaching `park()`, but `unparked`
            // ensures it is not forgotten.
            arc_self.thread.unpark();
        }
    }
}

// Set up and run a basic single-threaded spawner loop, invoking `f` on each
// turn.
// fn run_executor<T, F>(mut work_on_future: F) -> T
// where
//     F: FnMut(&mut Context<'_>) -> Poll<T>,
// {
//     let _enter = enter().expect(
//         "cannot execute `LocalPool` executor from within \
//          another executor",
//     );

//     CURRENT_THREAD_NOTIFY.with(|thread_notify| {
//         let waker = waker_ref(thread_notify);
//         let mut cx = Context::from_waker(&waker);
//         loop {
//             if let Poll::Ready(t) = work_on_future(&mut cx) {
//                 return t;
//             }
//             // Consume the wakeup that occurred while executing `f`, if any.
//             let unparked = thread_notify.unparked.swap(false, Ordering::Acquire);
//             if !unparked {
//                 // No wakeup occurred. It may occur now, right before parking,
//                 // but in that case the token made available by `unpark()`
//                 // is guaranteed to still be available and `park()` is a no-op.
//                 thread::park();
//                 // When the thread is unparked, `unparked` will have been set
//                 // and needs to be unset before the next call to `f` to avoid
//                 // a redundant loop iteration.
//                 thread_notify.unparked.store(false, Ordering::Release);
//             }
//         }
//     })
// }

fn poll_executor<T, F: FnMut(&mut Context<'_>) -> T>(mut f: F) -> T {
    let _enter = enter().expect(
        "cannot execute `LocalPool` executor from within \
         another executor",
    );

    CURRENT_THREAD_NOTIFY.with(|thread_notify| {
        let waker = waker_ref(thread_notify);
        let mut cx = Context::from_waker(&waker);
        f(&mut cx)
    })
}

/// Adds a new Task to the TaskQueue.
fn queue_task<T, Task, Out>(
    task: Task,
    heap_ptr: &mut *mut (),
    incoming: &Arc<Mutex<VecDeque<IncomingObject>>>,
) where
    Task: Fn(CosyncInput<T>) -> Out + Send + 'static,
    Out: Future<Output = ()> + Send,
{
    // force the future to move...
    let sec = CosyncInput {
        heap_ptr,
        stack: incoming.clone(),
        __parameter_type: PhantomData,
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

impl<T> Cosync<T> {
    /// Create a new, empty pool of tasks.
    pub fn new() -> Self {
        Self {
            pool: FuturesUnordered::new(),
            incoming: Default::default(),
            data: Box::new(ptr::null_mut()),
            __parameter_type: PhantomData,
        }
    }

    /// Adds a new Task to the TaskQueue.
    pub fn queue<Task, Out>(&mut self, task: Task)
    where
        Task: Fn(CosyncInput<T>) -> Out + Send + 'static,
        Out: Future<Output = ()> + Send,
    {
        queue_task(task, &mut self.data, &self.incoming);
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
    // pub fn run(&mut self, parameter: &mut T) {
    //     // hoist the T:
    //     unsafe {
    //         *self.data = Some(NonNull::new_unchecked(parameter as *mut _));
    //     }

    //     run_executor(|cx| self.poll_pool(cx));

    //     // for segfault help, we null here
    //     *self.data = None;
    // }

    /// Runs all the tasks in the pool until the given future completes.
    ///
    /// ```
    /// use futures::executor::LocalPool;
    ///
    /// let mut pool = LocalPool::new();
    /// # let my_app  = async {};
    ///
    /// // run tasks in the pool until `my_app` completes
    /// pool.run_until(my_app);
    /// ```
    ///
    /// The function will block the calling thread *only* until the future `f`
    /// completes; there may still be incomplete tasks in the pool, which will
    /// be inert after the call completes, but can continue with further use of
    /// one of the pool's run or poll methods. While the function is running,
    /// however, all tasks in the pool will try to make progress.
    // pub fn run_until<F: Future>(&mut self, future: F) -> F::Output {
    //     pin_mut!(future);

    //     run_executor(|cx| {
    //         {
    //             // if our main task is done, so are we
    //             let result = future.as_mut().poll(cx);
    //             if let Poll::Ready(output) = result {
    //                 return Poll::Ready(output);
    //             }
    //         }

    //         let _ = self.poll_pool(cx);
    //         Poll::Pending
    //     })
    // }

    /// Runs all tasks and returns after completing one future or until no more progress
    /// can be made. Returns `true` if one future was completed, `false` otherwise.
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
    /// // Run the two ready tasks and return true for them.
    /// pool.try_run_one(); // returns true after completing one of the ready futures
    /// pool.try_run_one(); // returns true after completing the other ready future
    ///
    /// // the remaining task can not be completed
    /// assert!(!pool.try_run_one()); // returns false
    /// ```
    ///
    /// This function will not block the calling thread and will return the moment
    /// that there are no tasks left for which progress can be made or after exactly one
    /// task was completed; Remaining incomplete tasks in the pool can continue with
    /// further use of one of the pool's run or poll methods.
    /// Though only one task will be completed, progress may be made on multiple tasks.
    // pub fn try_run_one(&mut self) -> bool {
    //     poll_executor(|ctx| {
    //         loop {
    //             let ret = self.poll_pool_once(ctx);

    //             // return if we have executed a future
    //             if let Poll::Ready(Some(_)) = ret {
    //                 return true;
    //             }

    //             // if there are no new incoming futures
    //             // then there is no feature that can make progress
    //             // and we can return without having completed a single future
    //             if self.incoming.borrow().is_empty() {
    //                 return false;
    //             }
    //         }
    //     })
    // }

    /// Runs all tasks in the pool and returns if no more progress can be made
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
    pub fn run_until_stalled(&mut self, mut parameter: T) {
        // hoist the T:
        *self.data = &mut parameter as *mut T as *mut _;

        poll_executor(|ctx| {
            let _ = self.poll_pool(ctx);
        });

        *self.data = ptr::null_mut();
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

// /// Turn a stream into a blocking iterator.
// ///
// /// When `next` is called on the resulting `BlockingStream`, the caller
// /// will be blocked until the next element of the `Stream` becomes available.
// pub fn block_on_stream<S: Stream + Unpin>(stream: S) -> BlockingStream<S> {
//     BlockingStream { stream }
// }

// /// An iterator which blocks on values from a stream until they become available.
// #[derive(Debug)]
// pub struct BlockingStream<S: Stream + Unpin> {
//     stream: S,
// }

// impl<S: Stream + Unpin> Deref for BlockingStream<S> {
//     type Target = S;
//     fn deref(&self) -> &Self::Target {
//         &self.stream
//     }
// }

// impl<S: Stream + Unpin> DerefMut for BlockingStream<S> {
//     fn deref_mut(&mut self) -> &mut Self::Target {
//         &mut self.stream
//     }
// }

// impl<S: Stream + Unpin> BlockingStream<S> {
//     /// Convert this `BlockingStream` into the inner `Stream` type.
//     pub fn into_inner(self) -> S {
//         self.stream
//     }
// }

// impl<S: Stream + Unpin> Iterator for BlockingStream<S> {
//     type Item = S::Item;

//     fn next(&mut self) -> Option<Self::Item> {
//         LocalPool::new().run_until(self.stream.next())
//     }

//     fn size_hint(&self) -> (usize, Option<usize>) {
//         self.stream.size_hint()
//     }
// }

// impl Spawn for LocalSpawner {
//     fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
//         if let Some(incoming) = self.incoming.upgrade() {
//             incoming.borrow_mut().push(future.into());
//             Ok(())
//         } else {
//             Err(SpawnError::shutdown())
//         }
//     }

//     fn status(&self) -> Result<(), SpawnError> {
//         if self.incoming.upgrade().is_some() {
//             Ok(())
//         } else {
//             Err(SpawnError::shutdown())
//         }
//     }
// }

// impl LocalSpawn for LocalSpawner {
//     fn spawn_local_obj(&self, future: LocalFutureObj<'static, ()>) -> Result<(), SpawnError> {
//         if let Some(incoming) = self.incoming.upgrade() {
//             incoming.borrow_mut().push(future);
//             Ok(())
//         } else {
//             Err(SpawnError::shutdown())
//         }
//     }

//     fn status_local(&self) -> Result<(), SpawnError> {
//         if self.incoming.upgrade().is_some() {
//             Ok(())
//         } else {
//             Err(SpawnError::shutdown())
//         }
//     }
// }

/// Sleep for a given number of ticks.
pub fn sleep_ticks(ticks: usize) -> SleepForTick {
    SleepForTick::new(ticks)
}

/// A helper struct which registers a sleep for a given number
/// of ticks.
#[derive(Clone, Copy, Debug)]
pub struct SleepForTick(usize);

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

        let mut executor: Cosync<&mut i32> = Cosync::new();
        executor.queue(move |mut input| async move {
            let mut input = input.get::<&mut i32>();

            assert_eq!(**input, 10);
            **input = 0;
        });

        // initialized here, after tasks are made
        // (so code is correctly being deferred)
        value = 10;
        executor.run_until_stalled(&mut value);
        assert_eq!(value, 0);
    }

    #[test]
    fn second_thing() {
        let value = 16;

        struct Passthrough<'a>(&'a i32);
        let pass_through = Passthrough(&value);

        let mut executor: Cosync<Passthrough<'_>> = Cosync::new();
        executor.queue(move |mut input| async move {
            let input = input.get::<Passthrough>();
            let inner = &*input;
            assert_eq!(*inner.0, 16);
        });

        executor.run_until_stalled(pass_through);
    }

    // #[test]
    // fn pool_remains_sequential() {
    //     // notice that value is declared here
    //     let mut value;

    //     let mut executor: Cosync<&mut i32> = Cosync::new();
    //     executor.queue(move |mut input| async move {
    //         println!("starting task 1");
    //         **input.get::<&mut i32>() = 10;

    //         sleep_ticks(100).await;

    //         **input.get::<&mut i32>() = 20;
    //     });

    //     executor.queue(move |mut input| async move {
    //         assert_eq!(**input.get::<&mut i32>(), 20);
    //     });

    //     value = 0;
    //     executor.run_until_stalled(&mut value);
    // }

    // #[test]
    // fn pool_is_still_sequential() {
    //     // notice that value is declared here
    //     let mut value;

    //     let mut executor: Cosync<&mut i32> = Cosync::new();
    //     executor.queue(move |mut input| async move {
    //         println!("starting task 1");
    //         let mut input = input.get::<&mut i32>();
    //         // input
    //         // let input = **input;
    //         // *input = 10;

    //         // input.queue(move |mut input| async move {
    //         //     println!("starting task 3");
    //         //     assert_eq!(**input.get(), 20);

    //         //     **input.get() = 30;
    //         // });
    //     });

    //     executor.queue(move |mut input| async move {
    //         println!("starting task 2");
    //         **input.get::<&mut i32>() = 20;
    //     });

    //     // initialized here, after tasks are made
    //     // (so code is correctly being deferred)
    //     value = 0;
    //     executor.run_until_stalled(&mut value);
    //     assert_eq!(value, 20);
    // }

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
