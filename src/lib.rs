use std::{
    cell::UnsafeCell,
    future::Future,
    marker::{PhantomData, PhantomPinned},
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    pin::Pin,
    ptr::{null_mut, NonNull},
    sync::atomic::*,
    task::Waker,
};

use async_scoped::{
    spawner::{Blocker, Spawner},
    Scope,
};
use futures::FutureExt;

/// An async borrowing contract
///
/// counter contains the number of current borrows + 1
///
/// when the counter is 1 the contract is being terminated by a lessee
struct Contract {
    counter: AtomicUsize,
    waker: UnsafeCell<MaybeUninit<Waker>>,
    _fragile: PhantomPinned,
}

unsafe impl Sync for Contract {}

unsafe impl Send for Contract {}

impl Contract {
    /// creates a new ontract with the counter equal to 1
    pub fn new() -> Self {
        Contract {
            counter: 1.into(),
            waker: UnsafeCell::new(MaybeUninit::new(futures::task::noop_waker())),
            _fragile: PhantomPinned,
        }
    }

    /// Tries to set the waker for the contract.
    ///
    /// if `Some(())` is returned then the waker was set, otherwise if `None` is returned the contract has been terminated and is ready to drop.
    ///
    /// *only the lessor should call this method.*
    pub unsafe fn set_waker(&self, cx: &mut std::task::Context<'_>) -> Option<()> {
        let previous = self.counter.fetch_add(1, Ordering::Relaxed);
        if previous > 1 {
            // other live references exist, after increment no cleanup can no occur
            *unsafe {
                self.waker
                    .get()
                    .as_mut()
                    .unwrap_unchecked()
                    .assume_init_mut()
            } = cx.waker().clone();

            let previous = self.counter.fetch_sub(1, Ordering::Relaxed);
            if previous == 2 {
                // terminate on last decrement
                drop(unsafe {
                    std::mem::replace(
                        self.waker.get().as_mut().unwrap_unchecked(),
                        MaybeUninit::uninit(),
                    )
                    .assume_init()
                });

                return None;
            }
            return Some(());
        };
        // wait for termination to finish
        while self.count() != 0 {}
        None
    }

    /// Increment without checking if the contract is being terminated.
    pub(self) unsafe fn increment_unchecked(&self) -> usize {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Decrement and start termination if nesacarry.
    pub(self) fn decrement(&self) {
        let previous = self.counter.fetch_sub(1, Ordering::Relaxed);
        if previous == 2 {
            // terminate on last decrement
            unsafe {
                std::mem::replace(
                    self.waker.get().as_mut().unwrap_unchecked(),
                    MaybeUninit::uninit(),
                )
                .assume_init()
                .wake();
            }

            self.counter.store(0, Ordering::Relaxed);
        }
    }

    pub(self) fn count(&self) -> usize {
        self.counter.load(Ordering::Relaxed)
    }
}

/// A wrapper over the `Contract` type representing the lessor.
///
/// Awaiting will wait until borrows end.
///
/// **Dropping will break aliasing rules.**
#[must_use = "dropping will break aliasing rules"]
pub struct Lessor {
    contract: *mut Contract,
}

unsafe impl Sync for Lessor {}

unsafe impl Send for Lessor {}

impl Future for Lessor {
    type Output = ();

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let Some(contract) = (unsafe { self.contract.as_ref() }) else {
            return std::task::Poll::Ready(());
        };
        if let Some(()) = unsafe { contract.set_waker(cx) } {
            return std::task::Poll::Pending;
        };
        drop(unsafe { Box::from_raw(self.contract) });
        self.contract = null_mut();
        std::task::Poll::Ready(())
    }
}

// impl Drop for Lessor {
//     fn drop(&mut self) {
//         if !self.contract.is_null() {
//             // -----------------
//             // DON'T END UP HERE
//             // -----------------
//             let lease = Lessor {
//                 contract: self.contract,
//             };
//             tokio::task::block_in_place(|| {
//                 if let Ok(handle) = tokio::runtime::Handle::try_current() {
//                     handle.block_on(lease)
//                 } else {
//                     let rt = tokio::runtime::Builder::new_current_thread()
//                         .build()
//                         .unwrap();
//                     rt.block_on(lease)
//                 }
//             })
//         }
//     }
// }

/// A wrapper over the `Contract` type representing the lessee.
///
/// Uses a reference count to borrow.
struct Lessee<T: ?Sized> {
    contract: NonNull<Contract>,
    ptr: NonNull<T>,
}

impl<T: ?Sized> Lessee<T> {
    pub fn is_unique(&self) -> bool {
        unsafe { self.contract.as_ref().count() == 2 }
    }

    pub fn as_ref(&self) -> &T {
        unsafe { self.ptr.as_ref() }
    }

    pub unsafe fn as_mut(&mut self) -> &mut T {
        unsafe { self.ptr.as_mut() }
    }
}

impl<T: ?Sized> Clone for Lessee<T> {
    fn clone(&self) -> Self {
        unsafe { self.contract.as_ref().increment_unchecked() };
        Self {
            contract: self.contract.clone(),
            ptr: self.ptr.clone(),
        }
    }
}

impl<T: ?Sized> Drop for Lessee<T> {
    fn drop(&mut self) {
        unsafe { self.contract.as_ref().decrement() };
    }
}

fn create_contract<T: ?Sized>(ptr: NonNull<T>) -> (Lessor, Lessee<T>) {
    let contract = Box::new(Contract::new());
    unsafe { contract.increment_unchecked() };
    let contract = Box::into_raw(contract);
    (
        Lessor { contract },
        Lessee {
            contract: unsafe { NonNull::new_unchecked(contract) },
            ptr,
        },
    )
}

pub struct Borrow<T: ?Sized>(pub(self) Lessee<T>);

unsafe impl<T: ?Sized + Sync> Sync for Borrow<T> {}

unsafe impl<T: ?Sized + Sync> Send for Borrow<T> {}

impl<T: ?Sized> Deref for Borrow<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

pub struct UpgradableBorrow<T: ?Sized>(pub(self) Lessee<T>);

unsafe impl<T: ?Sized + Sync> Sync for UpgradableBorrow<T> {}

unsafe impl<T: ?Sized + Sync + Send> Send for UpgradableBorrow<T> {}

impl<T: ?Sized> Deref for UpgradableBorrow<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl<T: ?Sized> UpgradableBorrow<T> {
    /// Tries to upgrade the borrow.
    ///
    /// *can fail even if unique because of how polling works.*
    pub fn try_upgrade(UpgradableBorrow(lessee): Self) -> Result<BorrowMut<T>, Self> {
        if lessee.is_unique() {
            Ok(BorrowMut(lessee))
        } else {
            Err(UpgradableBorrow(lessee))
        }
    }

    /// Tries to upgrade the pinned borrow.
    ///
    /// *can fail even if unique because of how polling works.*
    pub fn try_upgrade_pinned(borrow: Pin<Self>) -> Result<Pin<BorrowMut<T>>, Pin<Self>> {
        match unsafe { Self::try_upgrade(Pin::into_inner_unchecked(borrow)) } {
            Ok(upgraded) => unsafe { Ok(Pin::new_unchecked(upgraded)) },
            Err(borrow) => unsafe { Err(Pin::new_unchecked(borrow)) },
        }
    }
}

pub struct BorrowMut<T: ?Sized>(pub(self) Lessee<T>);

unsafe impl<T: ?Sized + Sync> Sync for BorrowMut<T> {}

unsafe impl<T: ?Sized + Send> Send for BorrowMut<T> {}

impl<T: ?Sized> Deref for BorrowMut<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl<T: ?Sized> DerefMut for BorrowMut<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.0.as_mut() }
    }
}

impl<T: ?Sized> From<BorrowMut<T>> for UpgradableBorrow<T> {
    fn from(BorrowMut(lessee): BorrowMut<T>) -> Self {
        UpgradableBorrow(lessee)
    }
}

impl<T: ?Sized> From<BorrowMut<T>> for Borrow<T> {
    fn from(BorrowMut(lessee): BorrowMut<T>) -> Self {
        Borrow(lessee)
    }
}

impl<T: ?Sized> From<UpgradableBorrow<T>> for Borrow<T> {
    fn from(UpgradableBorrow(lessee): UpgradableBorrow<T>) -> Self {
        Borrow(lessee)
    }
}

pub struct BorrowGaurd<'scope, 'a: 'scope, T: ?Sized> {
    original: NonNull<T>,
    signal: futures::channel::oneshot::Receiver<()>,
    _borrow: PhantomData<&'a T>,
    _scope: PhantomData<fn(&'scope ()) -> &'scope ()>,
}

unsafe impl<'scope, 'a: 'scope, T: ?Sized + Sync> Sync for BorrowGaurd<'scope, 'a, T> {}

unsafe impl<'scope, 'a: 'scope, T: ?Sized + Sync> Send for BorrowGaurd<'scope, 'a, T> {}

impl<'scope, 'a: 'scope, T: ?Sized> Future for BorrowGaurd<'scope, 'a, T> {
    type Output = &'a T;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.signal
            .poll_unpin(cx)
            .map(|_| unsafe { self.original.as_ref() })
    }
}

pub struct BorrowMutGaurd<'scope, 'a: 'scope, T: ?Sized> {
    original: NonNull<T>,
    signal: futures::channel::oneshot::Receiver<()>,
    _borrow: PhantomData<&'a mut T>,
    _scope: PhantomData<fn(&'scope ()) -> &'scope ()>,
}

unsafe impl<'scope, 'a: 'scope, T: ?Sized + Sync> Sync for BorrowMutGaurd<'scope, 'a, T> {}

unsafe impl<'scope, 'a: 'scope, T: ?Sized + Send> Send for BorrowMutGaurd<'scope, 'a, T> {}

impl<'scope, 'a: 'scope, T: ?Sized> Future for BorrowMutGaurd<'scope, 'a, T> {
    type Output = &'a mut T;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.signal
            .poll_unpin(cx)
            .map(|_| unsafe { self.original.as_mut() })
    }
}

pub struct PinningBorrowGaurd<'scope, 'a: 'scope, T: ?Sized> {
    original: NonNull<T>,
    signal: futures::channel::oneshot::Receiver<()>,
    _borrow: PhantomData<Pin<&'a T>>,
    _scope: PhantomData<fn(&'scope ()) -> &'scope ()>,
}

unsafe impl<'scope, 'a: 'scope, T: ?Sized + Sync> Sync for PinningBorrowGaurd<'scope, 'a, T> {}

unsafe impl<'scope, 'a: 'scope, T: ?Sized + Sync> Send for PinningBorrowGaurd<'scope, 'a, T> {}

impl<'scope, 'a: 'scope, T: ?Sized> Future for PinningBorrowGaurd<'scope, 'a, T> {
    type Output = Pin<&'a T>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.signal
            .poll_unpin(cx)
            .map(|_| unsafe { Pin::new_unchecked(self.original.as_ref()) })
    }
}

pub struct PinningBorrowMutGaurd<'scope, 'a: 'scope, T: ?Sized> {
    original: NonNull<T>,
    signal: futures::channel::oneshot::Receiver<()>,
    _borrow: PhantomData<Pin<&'a mut T>>,
    _scope: PhantomData<fn(&'scope ()) -> &'scope ()>,
}

unsafe impl<'scope, 'a: 'scope, T: ?Sized + Sync> Sync for PinningBorrowMutGaurd<'scope, 'a, T> {}

unsafe impl<'scope, 'a: 'scope, T: ?Sized + Send> Send for PinningBorrowMutGaurd<'scope, 'a, T> {}

impl<'scope, 'a: 'scope, T: ?Sized> Future for PinningBorrowMutGaurd<'scope, 'a, T> {
    type Output = Pin<&'a mut T>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.signal
            .poll_unpin(cx)
            .map(|_| unsafe { Pin::new_unchecked(self.original.as_mut()) })
    }
}

pub trait ScopeExt<'scope> {
    fn lease<'a: 'scope, T: ?Sized>(
        &mut self,
        ptr: &'a T,
    ) -> (BorrowGaurd<'scope, 'a, T>, Borrow<T>);

    fn lease_mut<'a: 'scope, T: ?Sized>(
        &mut self,
        ptr: &'a mut T,
    ) -> (BorrowMutGaurd<'scope, 'a, T>, BorrowMut<T>);

    fn pinning_lease<'a: 'scope, T: ?Sized>(
        &mut self,
        ptr: Pin<&'a T>,
    ) -> (PinningBorrowGaurd<'scope, 'a, T>, Pin<Borrow<T>>);

    fn pinning_lease_mut<'a: 'scope, T: ?Sized>(
        &mut self,
        ptr: Pin<&'a mut T>,
    ) -> (PinningBorrowMutGaurd<'scope, 'a, T>, Pin<BorrowMut<T>>);
}

impl<'scope, Sp: Spawner<()> + Blocker> ScopeExt<'scope> for Scope<'scope, (), Sp> {
    fn lease<'a: 'scope, T: ?Sized>(
        &mut self,
        ptr: &'a T,
    ) -> (BorrowGaurd<'scope, 'a, T>, Borrow<T>) {
        let ptr = NonNull::from(ptr);
        let (lessor, lessee) = create_contract(ptr);
        let (sender, receiver) = futures::channel::oneshot::channel::<()>();
        let borrow = Borrow(lessee);
        let gaurd = BorrowGaurd {
            original: ptr,
            signal: receiver,
            _borrow: PhantomData::<&'a T>,
            _scope: PhantomData::<fn(&'scope ()) -> &'scope ()>,
        };
        self.spawn(async {
            lessor.await;
            sender.send(()).unwrap_or(());
        });
        (gaurd, borrow)
    }

    fn lease_mut<'a: 'scope, T: ?Sized>(
        &mut self,
        ptr: &'a mut T,
    ) -> (BorrowMutGaurd<'scope, 'a, T>, BorrowMut<T>) {
        let ptr = NonNull::from(ptr);
        let (lessor, lessee) = create_contract(ptr);
        let (sender, receiver) = futures::channel::oneshot::channel::<()>();
        let borrow = BorrowMut(lessee);
        let gaurd = BorrowMutGaurd {
            original: ptr,
            signal: receiver,
            _borrow: PhantomData::<&'a mut T>,
            _scope: PhantomData::<fn(&'scope ()) -> &'scope ()>,
        };
        self.spawn(async {
            lessor.await;
            sender.send(()).unwrap_or(());
        });
        (gaurd, borrow)
    }

    fn pinning_lease<'a: 'scope, T: ?Sized>(
        &mut self,
        ptr: Pin<&'a T>,
    ) -> (PinningBorrowGaurd<'scope, 'a, T>, Pin<Borrow<T>>) {
        let ptr = unsafe { NonNull::from(Pin::into_inner_unchecked(ptr)) };
        let (lessor, lessee) = create_contract(ptr);
        let (sender, receiver) = futures::channel::oneshot::channel::<()>();
        let borrow = unsafe { Pin::new_unchecked(Borrow(lessee)) };
        let gaurd = PinningBorrowGaurd {
            original: ptr,
            signal: receiver,
            _borrow: PhantomData::<Pin<&'a T>>,
            _scope: PhantomData::<fn(&'scope ()) -> &'scope ()>,
        };
        self.spawn(async {
            lessor.await;
            sender.send(()).unwrap_or(());
        });
        (gaurd, borrow)
    }

    fn pinning_lease_mut<'a: 'scope, T: ?Sized>(
        &mut self,
        ptr: Pin<&'a mut T>,
    ) -> (PinningBorrowMutGaurd<'scope, 'a, T>, Pin<BorrowMut<T>>) {
        let ptr = unsafe { NonNull::from(Pin::into_inner_unchecked(ptr)) };
        let (lessor, lessee) = create_contract(ptr);
        let (sender, receiver) = futures::channel::oneshot::channel::<()>();
        let borrow = unsafe { Pin::new_unchecked(BorrowMut(lessee)) };
        let gaurd = PinningBorrowMutGaurd {
            original: ptr,
            signal: receiver,
            _borrow: PhantomData::<Pin<&'a mut T>>,
            _scope: PhantomData::<fn(&'scope ()) -> &'scope ()>,
        };
        self.spawn(async {
            lessor.await;
            sender.send(()).unwrap_or(());
        });
        (gaurd, borrow)
    }
}

#[cfg(test)]
mod tests {
    use crate::ScopeExt;

    #[tokio::test]
    async fn vibe_check() {
        let mut x: u32 = 0;
        unsafe {
            async_scoped::TokioScope::<()>::scope(|scope| {
                // borrow
                let (gaurd, mut borrow) = scope.lease_mut(&mut x);
                // spawn global
                tokio::task::spawn(async move {
                    *borrow += 1;
                });
                scope.spawn_cancellable(
                    async move {
                        // await guard
                        let rx = gaurd.await;
                    },
                    Default::default,
                );
            })
            .0
            .collect()
            .await
        };
    }
}
