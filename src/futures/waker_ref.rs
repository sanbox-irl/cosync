use std::{
    marker::PhantomData,
    mem::ManuallyDrop,
    ops::Deref,
    sync::Arc,
    task::{RawWaker, RawWakerVTable, Waker},
};

use super::arc_wake::ArcWake;

/// A [`Waker`] that is only valid for a given lifetime.
///
/// Note: this type implements [`Deref<Target = Waker>`](std::ops::Deref),
/// so it can be used to get a `&Waker`.
#[derive(Debug)]
pub struct WakerRef<'a> {
    waker: ManuallyDrop<Waker>,
    _marker: PhantomData<&'a ()>,
}

impl<'a> WakerRef<'a> {
    /// Create a new [`WakerRef`] from a [`Waker`] that must not be dropped.
    ///
    /// Note: this if for rare cases where the caller created a [`Waker`] in
    /// an unsafe way (that will be valid only for a lifetime to be determined
    /// by the caller), and the [`Waker`] doesn't need to or must not be
    /// destroyed.
    pub fn new_unowned(waker: ManuallyDrop<Waker>) -> Self {
        Self {
            waker,
            _marker: PhantomData,
        }
    }
}

impl Deref for WakerRef<'_> {
    type Target = Waker;

    fn deref(&self) -> &Waker {
        &self.waker
    }
}

#[allow(clippy::needless_lifetimes)] // clippy is wrong with ArcSelf here
pub fn waker_ref<T: ArcWake>(this: &Arc<T>) -> WakerRef<'_> {
    // simply copy the pointer instead of using Arc::into_raw,
    // as we don't actually keep a refcount by using ManuallyDrop.<
    let ptr = (&**this as *const T) as *const ();

    let waker = ManuallyDrop::new(unsafe { Waker::from_raw(RawWaker::new(ptr, waker_vtable::<T>())) });
    WakerRef::new_unowned(waker)
}

pub fn waker_vtable<T: ArcWake>() -> &'static RawWakerVTable {
    &RawWakerVTable::new(
        clone_arc_raw::<T>,
        wake_arc_raw::<T>,
        wake_by_ref_arc_raw::<T>,
        drop_arc_raw::<T>,
    )
}

#[allow(clippy::redundant_clone)] // The clone here isn't actually redundant.
unsafe fn increase_refcount<T>(data: *const ()) {
    // Retain Arc, but don't touch refcount by wrapping in ManuallyDrop
    let arc = ManuallyDrop::new(Arc::<T>::from_raw(data as *const T));
    // Now increase refcount, but don't drop new refcount either
    let _arc_clone: ManuallyDrop<_> = arc.clone();
}

// used by `waker_ref`
unsafe fn clone_arc_raw<T: ArcWake>(data: *const ()) -> RawWaker {
    increase_refcount::<T>(data);
    RawWaker::new(data, waker_vtable::<T>())
}

unsafe fn wake_arc_raw<T: ArcWake>(data: *const ()) {
    let arc: Arc<T> = Arc::from_raw(data as *const T);
    ArcWake::wake(arc);
}

// used by `waker_ref`
unsafe fn wake_by_ref_arc_raw<T: ArcWake>(data: *const ()) {
    // Retain Arc, but don't touch refcount by wrapping in ManuallyDrop
    let arc = ManuallyDrop::new(Arc::<T>::from_raw(data as *const T));
    ArcWake::wake_by_ref(&arc)
}

unsafe fn drop_arc_raw<T>(data: *const ()) {
    drop(Arc::<T>::from_raw(data as *const T))
}
