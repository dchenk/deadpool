//! Helpers for writing pools for objects that don't support async and need to
//! be run inside a thread.

use std::{
    any::Any,
    fmt,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use crate::{runtime::SpawnBlockingError, Runtime};

/// Possible errors returned when [`SyncWrapper::interact()`] fails.
#[derive(Debug)]
pub enum InteractError<E> {
    /// Provided callback has panicked.
    Panic(Box<dyn Any + Send + 'static>),

    /// Callback was aborted.
    Aborted,

    /// Backend returned an error.
    Backend(E),
}

impl<E: fmt::Display> fmt::Display for InteractError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Panic(_) => write!(f, "Panic"),
            Self::Aborted => write!(f, "Aborted"),
            Self::Backend(e) => write!(f, "Backend error: {}", e),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for InteractError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Panic(_) | Self::Aborted => None,
            Self::Backend(e) => Some(e),
        }
    }
}

/// Wrapper for objects which only provides blocking functions that need to be
/// called on a separate thread.
///
/// Access to the wrapped object is provided via the [`SyncWrapper::interact()`]
/// method.
#[must_use]
pub struct SyncWrapper<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    obj: Arc<Mutex<T>>,
    runtime: Runtime,
    _error: PhantomData<fn() -> E>,
}

// Implemented manually to avoid unnecessary trait bound on `E` type parameter.
impl<T, E> fmt::Debug for SyncWrapper<T, E>
where
    T: fmt::Debug + Send + 'static,
    E: Send + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncWrapper")
            .field("obj", &self.obj)
            .field("runtime", &self.runtime)
            .field("_error", &self._error)
            .finish()
    }
}

impl<T, E> SyncWrapper<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    /// Creates a new wrapped object.
    pub async fn new<F>(runtime: Runtime, f: F) -> Result<Self, E>
    where
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        let result = match runtime.spawn_blocking(move || f()).await {
            // FIXME: Panicking when the creation panics is not nice.
            // In order to handle this properly the Manager::create
            // methods needs to support a custom error enum which
            // supports a Panic variant.
            Err(SpawnBlockingError::Panic(e)) => panic!("{:?}", e),
            Ok(obj) => obj,
        };
        result.map(|obj| Self {
            obj: Arc::new(Mutex::new(obj)),
            runtime,
            _error: PhantomData::default(),
        })
    }

    /// Interacts with the underlying object.
    ///
    /// Expects a closure that takes the object as its parameter.
    /// The closure is executed in a separate thread so that the async runtime
    /// is not blocked.
    pub async fn interact<F, R>(&self, f: F) -> Result<R, InteractError<E>>
    where
        F: FnOnce(&mut T) -> Result<R, E> + Send + 'static,
        R: Send + 'static,
    {
        let arc = self.obj.clone();
        self.runtime
            .spawn_blocking(move || {
                let mut conn = arc.lock().unwrap();
                f(&mut *conn)
            })
            .await
            .map_err(|e| match e {
                SpawnBlockingError::Panic(p) => InteractError::Panic(p),
            })?
            .map_err(InteractError::Backend)
    }

    /// Get the underlying object wrapped in a Mutex that's wrapped in an Arc.
    ///
    /// Note: Anything you do with the object should be wrapped in a `spawn_blocking` closure
    /// so that the async runtime is not blocked.
    pub fn inner_obj(&self) -> Arc<Mutex<T>> {
        self.obj.clone()
    }

    /// Indicates whether the underlying [`Mutex`] has been poisoned.
    ///
    /// This happens when a panic occurs while interacting with the object.
    pub fn is_mutex_poisoned(&self) -> bool {
        self.obj.is_poisoned()
    }
}

impl<T, E> Drop for SyncWrapper<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    fn drop(&mut self) {
        let arc = self.obj.clone();
        // Drop the internal connection inside a `spawn_blocking`
        // as the `drop` function of it can block.
        self.runtime
            .spawn_blocking_background(move || match arc.lock() {
                Ok(guard) => drop(guard),
                Err(e) => drop(e.into_inner()),
            })
            .unwrap();
    }
}
