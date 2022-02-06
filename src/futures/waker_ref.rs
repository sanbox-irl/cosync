use std::{marker::PhantomData, mem::ManuallyDrop, ops::Deref, task::Waker};

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
