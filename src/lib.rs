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

use futures::{FutureExt, StreamExt};

use pin_project::{pin_project, pinned_drop};

/// An async borrowing contract
///
/// counter contains the number of current borrows + 1
///
/// when the counter is 1 the contract is being terminated by a lessee
#[derive(Debug)]
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
        if previous == 0 {
            return None;
        }
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
/// **Pre-mature dropping will break aliasing rules.**
#[must_use = "pre-mature dropping will break aliasing rules"]
struct Lessor {
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
#[pin_project(PinnedDrop)]
struct Lessee<T: ?Sized> {
    contract: NonNull<Contract>,
    #[pin]
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

#[pinned_drop]
impl<T: ?Sized> PinnedDrop for Lessee<T> {
    fn drop(self: Pin<&mut Self>) {
        unsafe { self.project().contract.as_ref().decrement() };
    }
}

fn create_contract<T: ?Sized>(ptr: NonNull<T>) -> (Lessor, Lessee<T>) {
    let contract = Box::new(Contract::new());
    unsafe { contract.increment_unchecked() };
    let contract = Box::into_raw(contract);
    // // occursed testing things
    // unsafe {
    //     let bingle: usize = contract as usize;
    //     std::thread::spawn(move || {
    //         for _ in 0..100 {
    //             let contract: *const Contract = bingle as *const Contract;
    //             println!("{:?}", contract.as_ref().unwrap());
    //         }
    //     });
    // };
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

pub struct BorrowHandle<'a, T: ?Sized> {
    original: NonNull<T>,
    signal: futures::channel::oneshot::Receiver<()>,
    _borrow: PhantomData<&'a T>,
}

unsafe impl<'a, T: ?Sized + Sync> Sync for BorrowHandle<'a, T> {}

unsafe impl<'a, T: ?Sized + Sync> Send for BorrowHandle<'a, T> {}

impl<'a, T: ?Sized> Future for BorrowHandle<'a, T> {
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

pub struct BorrowMutHandle<'a, T: ?Sized> {
    original: NonNull<T>,
    signal: futures::channel::oneshot::Receiver<()>,
    _borrow: PhantomData<&'a mut T>,
}

unsafe impl<'a, T: ?Sized + Sync> Sync for BorrowMutHandle<'a, T> {}

unsafe impl<'a, T: ?Sized + Send> Send for BorrowMutHandle<'a, T> {}

impl<'a, T: ?Sized> Future for BorrowMutHandle<'a, T> {
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

pub struct PinningBorrowHandle<'a, T: ?Sized> {
    original: NonNull<T>,
    signal: futures::channel::oneshot::Receiver<()>,
    _borrow: PhantomData<Pin<&'a T>>,
}

unsafe impl<'a, T: ?Sized + Sync> Sync for PinningBorrowHandle<'a, T> {}

unsafe impl<'a, T: ?Sized + Sync> Send for PinningBorrowHandle<'a, T> {}

impl<'a, T: ?Sized> Future for PinningBorrowHandle<'a, T> {
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

pub struct PinningBorrowMutHandle<'a, T: ?Sized> {
    original: NonNull<T>,
    signal: futures::channel::oneshot::Receiver<()>,
    _borrow: PhantomData<Pin<&'a mut T>>,
}

unsafe impl<'a, T: ?Sized + Sync> Sync for PinningBorrowMutHandle<'a, T> {}

unsafe impl<'a, T: ?Sized + Send> Send for PinningBorrowMutHandle<'a, T> {}

impl<'a, T: ?Sized> Future for PinningBorrowMutHandle<'a, T> {
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

/// *trait is unsafe as the expectation that awaiting the returned future will await provided future
/// cannot be expressed within the type system.*
pub unsafe trait Spawner {
    type JoinHandle<T: Send + 'static>: Future<Output = T> + Send + 'static;

    type LocalJoinHandle<T: 'static>: Future<Output = T> + 'static;

    /// Monomorphise handle types.
    fn localise_handle<T: Send + 'static>(handle: Self::JoinHandle<T>) -> Self::LocalJoinHandle<T>;

    /// Spawn a new task with 'static lifetime.
    ///
    /// *unsafe trait impl: must return a join handle that when awaited awaits for the spawned future to finish.*
    fn spawn_unscoped<T: Send + 'static>(
        self: Pin<&mut Self>,
        f: impl Future<Output = T> + Send + 'static,
    ) -> Self::JoinHandle<T>;

    /// Spawn a new local task with 'static lifetime.
    ///
    /// *unsafe trait impl: must return a join handle that when awaited awaits for the spawned future to finish.*
    fn spawn_local_unscoped<T: 'static>(
        self: Pin<&mut Self>,
        f: impl Future<Output = T> + 'static,
    ) -> Self::LocalJoinHandle<T>;

    /// Evaluate future whilst blocking this thread.
    ///
    /// *used for handling premature drops*
    fn block_on<T>(self: Pin<&mut Self>, f: impl Future<Output = T>) -> T;
}

/// An anchor to handle safe scoped task spawning and async borrowing contracts.
///
/// **failing to `.await` this before dropping will result in performance issues and potential deadlocks**
#[must_use = "failing to `.await` this before dropping will result in performance issues and potential deadlocks"]
#[pin_project(PinnedDrop)]
pub struct Anchor<'env, S: Spawner> {
    /// Inner spawner that is used to spawn tasks.
    #[pin]
    spawner: S,
    /// Unordered stream containing the futures spawned within this scope.
    ///
    /// Will **always** be `Some` outside of `drop`.
    tasks: Option<futures::stream::FuturesUnordered<S::LocalJoinHandle<()>>>,
    /// Contravarient lifetime parameter allowing the scope to be broadened.
    /// Broadening the scope is safe because it only acts to limit what it can borrow.
    _env: PhantomData<fn(Pin<&'env ()>)>,
}

impl<'env, S: Spawner> Future for Anchor<'env, S> {
    type Output = ();

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let projected = self.project();
        // Will **always** be `Some` outside of `drop`.
        let tasks = unsafe { projected.tasks.as_mut().unwrap_unchecked() };
        if let std::task::Poll::Ready(None) = tasks.poll_next_unpin(cx) {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    }
}

#[pinned_drop]
impl<'env, S: Spawner> PinnedDrop for Anchor<'env, S> {
    fn drop(self: Pin<&mut Self>) {
        let projected = self.project();
        // Will **always** be `Some` outside of `drop`.
        let mut tasks = unsafe { projected.tasks.take().unwrap_unchecked() };
        if tasks.is_empty() {
            return;
        }
        // -----------------
        // DON'T END UP HERE
        // -----------------
        projected
            .spawner
            .block_on(async move { while let Some(_) = tasks.next().await {} })
    }
}

impl<'env, S: Spawner> Anchor<'env, S> {
    pub fn new(spawner: S) -> Self {
        Anchor {
            spawner,
            tasks: Some(futures::stream::FuturesUnordered::new()),
            _env: PhantomData::<fn(Pin<&'env ()>)>,
        }
    }

    /// Spawns a new task.
    ///
    /// **supports growing of the `'env` lifetime as this can only limit what is spawned.**
    pub fn spawn(self: Pin<&mut Self>, f: impl Future<Output = ()> + Send + 'env) {
        let projected = self.project();
        // Will **always** be `Some` outside of `drop`.
        let tasks = unsafe { projected.tasks.as_ref().unwrap_unchecked() };
        type TransSrc<'env> = Pin<Box<dyn Future<Output = ()> + Send + 'env>>;
        type TransDst = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
        let join_handle = projected.spawner.spawn_unscoped(unsafe {
            std::mem::transmute::<TransSrc<'env>, TransDst>(Box::pin(f))
        });
        tasks.push(<S as Spawner>::localise_handle(join_handle));
    }

    /// Spawns a new task on the same thread, circumventing the requirement for the future to be `Send`.
    ///
    /// **supports growing of the `'env` lifetime as this can only limit what is spawned.**
    pub fn spawn_local(self: Pin<&mut Self>, f: impl Future<Output = ()> + 'env) {
        let projected = self.project();
        // Will **always** be `Some` outside of `drop`.
        let tasks = unsafe { projected.tasks.as_ref().unwrap_unchecked() };
        type TransSrc<'env> = Pin<Box<dyn Future<Output = ()> + 'env>>;
        type TransDst = Pin<Box<dyn Future<Output = ()> + 'static>>;
        let join_handle = projected.spawner.spawn_local_unscoped(unsafe {
            std::mem::transmute::<TransSrc<'env>, TransDst>(Box::pin(f))
        });
        tasks.push(join_handle);
    }

    /// Leases immutable access to the memory referenced by `ptr` to arbitrary async tasks using reference counting.
    ///
    /// **`'a: 'env` communicates to the type system that `ptr` may be borrowed by `self` or the returned `BorrowHandle`,
    /// requiring both to be dropped for the borrow to end.**
    pub fn lease<'a: 'env, T>(
        self: Pin<&mut Self>,
        ptr: &'a T,
    ) -> (BorrowHandle<'a, T>, Borrow<T>) {
        let ptr = NonNull::from(ptr);
        let (lessor, lessee) = create_contract(ptr);
        let (sender, receiver) = futures::channel::oneshot::channel::<()>();
        let borrow = Borrow(lessee);
        let handle = BorrowHandle {
            original: ptr,
            signal: receiver,
            _borrow: PhantomData::<&'a T>,
        };
        self.spawn(async {
            lessor.await;
            sender.send(()).unwrap_or(());
        });
        (handle, borrow)
    }

    /// Leases mutable access to the memory referenced by `ptr` to arbitrary async tasks using reference counting.
    ///
    /// **`'a: 'env` communicates to the type system that `ptr` may be borrowed by `self` or the returned `BorrowHandle`,
    /// requiring both to be dropped for the borrow to end.**
    pub fn lease_mut<'a: 'env, T>(
        self: Pin<&mut Self>,
        ptr: &'a mut T,
    ) -> (BorrowMutHandle<'a, T>, BorrowMut<T>) {
        let ptr = NonNull::from(ptr);
        let (lessor, lessee) = create_contract(ptr);
        let (sender, receiver) = futures::channel::oneshot::channel::<()>();
        let borrow = BorrowMut(lessee);
        let handle = BorrowMutHandle {
            original: ptr,
            signal: receiver,
            _borrow: PhantomData::<&'a mut T>,
        };
        self.spawn(async {
            lessor.await;
            sender.send(()).unwrap_or(());
        });
        (handle, borrow)
    }

    /// Leases pinned immutable access to the memory referenced by `ptr` to arbitrary async tasks using reference counting.
    ///
    /// **`'a: 'env` communicates to the type system that `ptr` may be borrowed by `self` or the returned `BorrowHandle`,
    /// requiring both to be dropped for the borrow to end.**
    pub fn lease_pinned<'a: 'env, T>(
        self: Pin<&mut Self>,
        ptr: Pin<&'a T>,
    ) -> (PinningBorrowHandle<'a, T>, Pin<Borrow<T>>) {
        let ptr = NonNull::from(unsafe { Pin::into_inner_unchecked(ptr) });
        let (lessor, lessee) = create_contract(ptr);
        let (sender, receiver) = futures::channel::oneshot::channel::<()>();
        let borrow = unsafe { Pin::new_unchecked(Borrow(lessee)) };
        let handle = PinningBorrowHandle {
            original: ptr,
            signal: receiver,
            _borrow: PhantomData::<Pin<&'a T>>,
        };
        self.spawn(async {
            lessor.await;
            sender.send(()).unwrap_or(());
        });
        (handle, borrow)
    }

    /// Leases pinned mutable access to the memory referenced by `ptr` to arbitrary async tasks using reference counting.
    ///
    /// **`'a: 'env` communicates to the type system that `ptr` may be borrowed by `self` or the returned `BorrowHandle`,
    /// requiring both to be dropped for the borrow to end.**
    pub fn lease_pinned_mut<'a: 'env, T>(
        self: Pin<&mut Self>,
        ptr: Pin<&'a mut T>,
    ) -> (PinningBorrowMutHandle<'a, T>, Pin<BorrowMut<T>>) {
        let ptr = NonNull::from(unsafe { Pin::into_inner_unchecked(ptr) });
        let (lessor, lessee) = create_contract(ptr);
        let (sender, receiver) = futures::channel::oneshot::channel::<()>();
        let borrow = unsafe { Pin::new_unchecked(BorrowMut(lessee)) };
        let handle = PinningBorrowMutHandle {
            original: ptr,
            signal: receiver,
            _borrow: PhantomData::<Pin<&'a mut T>>,
        };
        self.spawn(async {
            lessor.await;
            sender.send(()).unwrap_or(());
        });
        (handle, borrow)
    }
}

// #[cfg(test)]
// mod tests {
//     use crate::ScopeExt;

//     #[tokio::test]
//     async fn vibe_check() {
//         let mut x: u32 = 0;
//         unsafe {
//             async_scoped::TokioScope::<()>::scope(|scope| {
//                 // borrow
//                 let (handle, mut borrow) = scope.lease_mut(&mut x);
//                 // spawn global
//                 tokio::task::spawn(async move {
//                     *borrow += 1;
//                 });
//                 scope.spawn_cancellable(
//                     async move {
//                         // await guard
//                         let rx = handle.await;
//                         *rx += 2;
//                     },
//                     Default::default,
//                 );
//             })
//             .0
//             .collect()
//             .await
//         };
//     }
// }
