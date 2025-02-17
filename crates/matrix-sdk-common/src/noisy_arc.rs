use std::{
    fmt::Debug,
    ops::Deref,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use tracing::{error, info};

#[derive(Debug)]
pub struct NoisyArc<T: ?Sized + Debug> {
    ptr: Arc<NoisyArcInner<T>>,

    /// unique id for this ref to the inner
    id: u64,
}

impl<T: ?Sized + Debug> Clone for NoisyArc<T> {
    fn clone(&self) -> Self {
        let res = Self { ptr: self.ptr.clone(), id: self.ptr.get_next_id() };
        if self.ptr.noisy {
            info!(
                "NoisyArc::clone({}) {:?}. Refcount now {}",
                res.id,
                self.ptr.inner,
                Arc::strong_count(&self.ptr)
            );
        }
        res
    }
}

impl<T: ?Sized + Debug> Drop for NoisyArc<T> {
    fn drop(&mut self) {
        if self.ptr.noisy {
            info!(
                "NoisyArc::drop({}) {:?}. Refcount before drop {}",
                self.id,
                self.ptr.inner,
                Arc::strong_count(&self.ptr)
            );
        }
    }
}

impl<T: ?Sized + Debug> Deref for NoisyArc<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.ptr.inner.deref()
    }
}

impl<T: Debug + ?Sized> NoisyArc<T> {
    /// Wrap the given object in a silent NoisyArc
    pub fn new(data: T) -> NoisyArc<T>
    where
        T: Sized,
    {
        Self::from_box(Box::new(data), false)
    }

    /// Wrap the given object in a noisy NoisyArc
    pub fn new_noisy(data: T) -> NoisyArc<T>
    where
        T: Sized,
    {
        Self::from_box(Box::new(data), true)
    }

    pub fn from_box(inner: Box<T>, noisy: bool) -> NoisyArc<T> {
        use uuid::Uuid;

        Self {
            ptr: Arc::new(NoisyArcInner {
                noisy,
                inner,
                next_id: AtomicU64::new(1),
                base_id: Uuid::new_v4().to_string(),
            }),
            id: 0,
        }
    }

    // SAFETY: T and U must be compatible types such that transmuting between
    // them directly would be acceptable.
    pub unsafe fn transmute<U: Debug + ?Sized>(self) -> NoisyArc<U> {
        let id = self.id;
        let ptr = Arc::into_raw(self.ptr.clone());
        drop(self);

        // SAFETY: T and U are compatible for transmuting, NoisyArcInner is
        // #[repr(C)], thus NoisyArcInner<T> and NoisyArcInner<U> have the same
        // layout and safety invariants.
        let ptr = unsafe { Arc::from_raw(ptr as _) };
        let res = NoisyArc { ptr, id };

        error!(
            "NoisyArc::transmute {:?}. Refcount now {}",
            res.ptr.inner,
            Arc::strong_count(&res.ptr)
        );
        res
    }

    pub fn as_ref(&self) -> &T {
        self.ptr.inner.as_ref()
    }
}

#[derive(Debug)]
#[repr(C)]
pub struct NoisyArcInner<T: ?Sized> {
    noisy: bool,
    inner: Box<T>,
    base_id: String,
    next_id: AtomicU64,
}

impl<T: ?Sized> NoisyArcInner<T> {
    pub fn get_next_id(&self) -> u64 {
        return self.next_id.fetch_add(1, Ordering::SeqCst);
    }
}
