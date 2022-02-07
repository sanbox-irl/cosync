use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::Thread,
};

use crate::futures::arc_wake::ArcWake;

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
