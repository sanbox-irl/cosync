use std::{
    mem::{self, ManuallyDrop},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{RawWaker, RawWakerVTable, Waker},
    thread::Thread,
};

use super::WakerRef;

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

impl ThreadNotify {
    #[allow(clippy::needless_lifetimes)] // clippy is wrong with ArcSelf here
    pub fn waker_ref<'a>(self: &'a Arc<Self>) -> WakerRef<'a> {
        // simply copy the pointer instead of using Arc::into_raw,
        // as we don't actually keep a refcount by using ManuallyDrop.<
        let ptr = (&**self as *const ThreadNotify) as *const ();

        let waker = ManuallyDrop::new(unsafe { Waker::from_raw(RawWaker::new(ptr, Self::waker_vtable())) });
        WakerRef::new_unowned(waker)
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // Make sure the wakeup is remembered until the next `park()`.
        let unparked = self.unparked.swap(true, Ordering::Relaxed);
        if !unparked {
            // If the thread has not been unparked yet, it must be done
            // now. If it was actually parked, it will run again,
            // otherwise the token made available by `unpark`
            // may be consumed before reaching `park()`, but `unparked`
            // ensures it is not forgotten.
            self.thread.unpark();
        }
    }

    pub fn waker_vtable() -> &'static RawWakerVTable {
        &RawWakerVTable::new(
            Self::clone_arc_raw,
            Self::wake_arc_raw,
            Self::wake_by_ref_arc_raw,
            Self::drop_arc_raw,
        )
    }

    #[allow(clippy::redundant_clone)] // The clone here isn't actually redundant.
    unsafe fn increase_refcount(data: *const ()) {
        // Retain Arc, but don't touch refcount by wrapping in ManuallyDrop
        let arc = mem::ManuallyDrop::new(Arc::<Self>::from_raw(data as *const Self));
        // Now increase refcount, but don't drop new refcount either
        let _arc_clone: mem::ManuallyDrop<_> = arc.clone();
    }

    // used by `waker_ref`
    unsafe fn clone_arc_raw(data: *const ()) -> RawWaker {
        Self::increase_refcount(data);
        RawWaker::new(data, Self::waker_vtable())
    }

    unsafe fn wake_arc_raw(data: *const ()) {
        let arc: Arc<Self> = Arc::from_raw(data as *const Self);
        arc.wake_by_ref();
    }

    // used by `waker_ref`
    unsafe fn wake_by_ref_arc_raw(data: *const ()) {
        // Retain Arc, but don't touch refcount by wrapping in ManuallyDrop
        let arc = mem::ManuallyDrop::new(Arc::<Self>::from_raw(data as *const Self));
        // use futures::task::ArcWake
        arc.wake_by_ref();
    }

    unsafe fn drop_arc_raw(data: *const ()) {
        drop(Arc::<Self>::from_raw(data as *const Self))
    }
}
