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

use futures::{stream::FusedStream, FutureExt, Stream, StreamExt};

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

pub struct Owner<T> {
    share: ShareWrapper<T>,
}

struct ShareWrapper<T> {
    inner: NonNull<WeakShare<T>>,
}

impl<T> Deref for ShareWrapper<T> {
    type Target = WeakShare<T>;

    fn deref(&self) -> &Self::Target {
        unsafe { self.inner.as_ref() }
    }
}

impl<T> Clone for ShareWrapper<T> {
    fn clone(&self) -> Self {
        self.log_reference();
        ShareWrapper { inner: self.inner }
    }
}

impl<T> Drop for ShareWrapper<T> {
    fn drop(&mut self) {
        if self.unlog_reference() == 1 {
            drop(unsafe { Box::from_raw(self.inner.as_ptr()) })
        }
    }
}

struct WeakShare<T> {
    /// Reference count (including owner)
    refs: AtomicUsize,
    share: Share<T>,
}

impl<T> WeakShare<T> {
    fn log_reference(&self) -> usize {
        self.refs.fetch_add(1, Ordering::Relaxed)
    }

    fn unlog_reference(&self) -> usize {
        self.refs.fetch_sub(1, Ordering::Relaxed)
    }
}

impl<T> Deref for WeakShare<T> {
    type Target = Share<T>;

    fn deref(&self) -> &Self::Target {
        &self.share
    }
}

struct Share<T> {
    /// The number of borrows plus 1.
    /// If the counter is 1 then the waker is considered to be aquired.
    counter: AtomicUsize,
    /// The waker for alerting the owner that the borrow is over.
    ///
    /// **must be unset if counter is 0 and set if counter is 2+.**
    ///
    /// May be modified based on counter state:
    ///
    /// ```no_run
    /// +----+----------------+---------------+-------------------+
    /// | 0  | ------- : ---- | &owner : read | &mut owner : &mut |
    /// +----+----------------+---------------+-------------------+
    /// | 1  | visitor : &mut | &owner :  ><  | &mut owner :  ><  |
    /// +----+----------------+---------------+-------------------+
    /// | 2+ | visitor :  ><  | &owner : read | &mut owner : &mut |
    /// +----+----------------+---------------+-------------------+
    /// ```
    waker: UnsafeCell<MaybeUninit<Waker>>,
    /// Flag for specifying if memory is mutable.
    ///
    /// **must be false if counter is 0.**
    ///
    /// May be accessed based on counter state:
    /// ```no_run
    /// +----+----------------+---------------+-------------------+
    /// | 0  | ------- : ---- | &owner : read | &mut owner : &mut |
    /// +----+----------------+---------------+-------------------+
    /// | 1  | visitor : &mut | &owner :  ><  | &mut owner :  ><  |
    /// +----+----------------+---------------+-------------------+
    /// | 2+ | visitor : read | &owner : read | &mut owner : read |
    /// +----+----------------+---------------+-------------------+
    /// ```
    is_mut: UnsafeCell<bool>,
    _shared: PhantomPinned,
    val: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Share<T> {
    /// Increment the count assuming there is a non-dropping visitor currently allocated.
    ///
    /// **only visitors may safely call.**
    unsafe fn increment_unchecked(&self) -> usize {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Read `is_mut` assuming there is a non-dropping visitor currently allocated.
    ///
    /// **only visitors may safely call.**
    unsafe fn check_mut_unchecked(&self) -> bool {
        unsafe { *NonNull::new_unchecked(self.is_mut.get()).as_ref() }
    }

    unsafe fn get_is_mut(&self) -> bool {
        todo!()
    }

    unsafe fn set_is_mut(&self, new: bool) {
        todo!()
    }

    unsafe fn as_ref(&self) -> &T {
        todo!()
    }

    unsafe fn as_mut(&self) -> &mut T {
        todo!()
    }

    fn increment(&self, waker_gen: impl FnOnce() -> Waker) -> usize {
        let n = loop {
            let n = self.counter.load(Ordering::Relaxed);
            if n == 1 {
                continue;
            }
            let success_ordering = if n == 0 {
                Ordering::Acquire
            } else {
                Ordering::Relaxed
            };
            match self
                .counter
                .compare_exchange(n, n + 1, success_ordering, Ordering::Relaxed)
            {
                Ok(_) => break n,
                Err(_) => continue,
            }
            #[allow(unreachable_code)]
            {
                unreachable!()
            }
        };
        if n == 0 {
            unsafe {
                drop(std::mem::replace(
                    NonNull::new_unchecked(self.waker.get()).as_mut(),
                    MaybeUninit::new((waker_gen)()),
                ))
            }
            self.counter.store(2, Ordering::Release);
        };
        n
    }

    fn decrement(&self) -> usize {
        let n = loop {
            let n = self.counter.load(Ordering::Relaxed);
            if n == 1 {
                continue;
            }
            let success_ordering = if n == 2 {
                Ordering::Acquire
            } else {
                Ordering::Relaxed
            };
            match self
                .counter
                .compare_exchange(n, n - 1, success_ordering, Ordering::Relaxed)
            {
                Ok(_) => break n,
                Err(_) => continue,
            }
            #[allow(unreachable_code)]
            {
                unreachable!()
            }
        };
        if n == 2 {
            unsafe {
                std::mem::replace(
                    NonNull::new_unchecked(self.waker.get()).as_mut(),
                    MaybeUninit::uninit(),
                )
                .assume_init()
                .wake();
            };
            *unsafe { NonNull::new_unchecked(self.is_mut.get()).as_mut() } = false;
            self.counter.store(0, Ordering::Release);
        }
        n
    }

    /// Tries to borrow the share.
    ///
    /// Fails is currently borrowed mutably.
    pub fn try_borrow_ref(&self) -> Option<NonNull<Self>> {
        // Create visitor
        self.increment(futures::task::noop_waker);
        if unsafe { self.check_mut_unchecked() } {
            // Delete visitor if invalid
            self.decrement();
            None
        } else {
            // Return borrow
            Some(unsafe { NonNull::new_unchecked((self as *const Self) as *mut Self) })
        }
    }

    /// Try borrow the share mutably.
    ///
    /// Only works when count is 0.
    pub fn try_borrow_mut(&self) -> Option<NonNull<Self>> {
        if let Err(_) = self
            .counter
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        {
            return None;
        }
        *unsafe { NonNull::new_unchecked(self.is_mut.get()).as_mut() } = true;
        self.counter.store(2, Ordering::Release);
        Some(unsafe { NonNull::new_unchecked((self as *const Self) as *mut Self) })
    }
}

/// Spawning handle for an Anchor.
///
/// *can be safely broadened as it can only spawn tasks.*
#[derive(Debug, Clone)]
pub struct Spawner</* In */ 'env, /* In */ T>(
    futures::channel::mpsc::UnboundedSender</* In */ Pin<Box<dyn Future<Output = T> + 'env>>>,
);

impl<'env, T> Spawner<'env, T> {
    pub fn spawn(
        &mut self,
        future: impl Future<Output = T> + 'env,
    ) -> Result<(), futures::channel::mpsc::SendError> {
        self.0.start_send(Box::pin(future))
    }
}

/// An anchor to handle safe scoped task "spawning".
#[derive(Debug)]
pub struct Anchor</* Mix */ 'env, /* Mix */ T> {
    receiver: futures::channel::mpsc::UnboundedReceiver<
        /* Out */ Pin<Box<dyn Future<Output = T> + 'env>>,
    >,
    pub spawner: Spawner</* In */ 'env, /* In */ T>,
}

impl<'env, T> Anchor<'env, T> {
    pub fn new() -> Self {
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        Anchor {
            receiver,
            spawner: Spawner(sender),
        }
    }

    pub fn stream(self) -> Pool<'env, T> {
        Pool {
            receiver: self.receiver,
            tasks: futures::stream::FuturesUnordered::new(),
        }
    }
}

impl<'env, T> Deref for Anchor<'env, T> {
    type Target = Spawner<'env, T>;

    fn deref(&self) -> &Self::Target {
        &self.spawner
    }
}

impl<'env, T> DerefMut for Anchor<'env, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.spawner
    }
}

/// A future for awaiting a collection of futures.
///
/// *can safely be narrowed as no more tasks can be spawned to it.*
#[derive(Debug)]
#[must_use = "streams do nothing unless polled"]
pub struct Pool</* Out */ 'env, /* Out */ T> {
    receiver: futures::channel::mpsc::UnboundedReceiver<
        /* Out */ Pin<Box<dyn Future<Output = T> + 'env>>,
    >,
    tasks:
        futures::stream::FuturesUnordered</* Out */ Pin<Box<dyn Future<Output = T> + 'env>>>,
}

impl<'env, T> Stream for Pool<'env, T> {
    type Item = T;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        if self.receiver.is_terminated() {
            // only the tasks now
            return self.tasks.poll_next_unpin(cx);
        }
        loop {
            match self.receiver.poll_next_unpin(cx) {
                Poll::Ready(None) => {
                    if self.tasks.is_terminated() {
                        // end stream
                        return Poll::Ready(None);
                    } else {
                        break;
                    }
                }
                Poll::Ready(Some(task)) => {
                    self.tasks.push(task);
                    continue;
                }
                Poll::Pending => break,
            }
            #[allow(unreachable_code)]
            {
                unreachable!()
            }
        }
        // we have awaited all we can from the receiver
        if !self.tasks.is_terminated() {
            // more tasks to run
            match self.tasks.poll_next_unpin(cx) {
                Poll::Ready(Some(val)) => return Poll::Ready(Some(val)),
                Poll::Ready(None) => {
                    if self.receiver.is_terminated() {
                        // end stream
                        return Poll::Ready(None);
                    }
                }
                Poll::Pending => (),
            }
        };
        // we have awaited all we can for the tasks
        Poll::Pending
    }
}

impl<'env, T> FusedStream for Pool<'env, T> {
    fn is_terminated(&self) -> bool {
        self.tasks.is_terminated() && self.receiver.is_terminated()
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use crate::Anchor;

    #[tokio::test]
    async fn vibe_check() {
        let mut x: u32 = 0;
        {
            let mut anchor = Anchor::<&mut u32>::new();
            let rx = &mut x;
            anchor
                .spawn(async move {
                    *rx += 1;
                    rx
                })
                .unwrap();
            {
                let stream = anchor.stream();
                for ref_mut in stream.collect::<Vec<&mut u32>>().await {
                    *ref_mut += 2;
                }
            }
        }
        assert_eq!(x, 3)
    }
}
