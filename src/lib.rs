use std::{
    cell::UnsafeCell,
    future::Future,
    marker::{PhantomData, PhantomPinned},
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::atomic::*,
    task::Waker,
};

use futures::{stream::FusedStream, Stream, StreamExt};

pub mod collections;

/// A view to the memory that is being borrowed from the perspective of the owner.
/// 
/// Borrows the owner mutably.
struct OwnerView<'owner, T> {
    /// The pointer to the contract.
    /// 
    /// Will never be null.
    contract: *const Contract<T>,
    _owner: PhantomData<&'owner mut ()>,
}

impl<'owner, T> OwnerView<'owner, T> {
    pub unsafe fn from_raw(contract: *const Contract<T>) -> Self {
        OwnerView { contract, _owner: PhantomData }
    }

    pub async fn aquire(self) -> UniqueView<'owner, T> {
        let contract = self.contract;
        let _owner = self._owner;
        self.await;
        // all borrows are done
        UniqueView { contract: contract as *mut _, _owner }
    }

    pub unsafe fn aquire_unchecked(self) -> UniqueView<'owner, T> {
        UniqueView { contract: self.contract as *mut _, _owner: self._owner }
    }
}

impl<'owner, T> Future for OwnerView<'owner, T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        match unsafe { self.contract.as_ref().unwrap_unchecked().try_set_waker(cx) } {
            // waker was set, so we are not done
            Some(_) => std::task::Poll::Pending,
            // waker was not set, so we are done
            None => std::task::Poll::Ready(()),
        }
    }
}

/// A view to an inactive contract from the perspective of the owner.
/// 
/// Borrows the owner mutably.
struct UniqueView<'owner, T> {
    /// The pointer to the contract.
    /// 
    /// Will never be null.
    contract: *mut Contract<T>,
    _owner: PhantomData<&'owner mut ()>,
}

impl<'owner, T> UniqueView<'owner, T> {
    pub fn as_ref(&self) -> &T {
        unsafe { self.contract.as_ref().unwrap_unchecked().as_ref() }
    }

    pub fn as_mut(&mut self) -> &mut T {
        unsafe { self.contract.as_ref().unwrap_unchecked().as_mut() }
    }

    pub fn share(self) -> (OwnerView<'owner, T>, VisitorView<T>) {
        let _owner = self._owner;
        unsafe { self.contract.as_mut().unwrap_unchecked().init_visitor() };
        let contract = self.contract as *const _;
        (OwnerView { contract, _owner }, VisitorView { contract })
    }

    pub fn as_owner(self) -> OwnerView<'owner, T> {
        OwnerView { contract: self.contract, _owner: self._owner }
    }
}

/// A view to the memory that is being borrowed from the perspective of the visitor.
struct VisitorView<T> {
    /// The pointer to the contract.
    /// 
    /// Will never be null.
    contract: *const Contract<T>,
}

impl<T> VisitorView<T> {
    pub fn as_ref(&self) -> &T {
        unsafe { self.contract.as_ref().unwrap_unchecked().as_ref() }
    }

    pub unsafe fn as_mut(&mut self) -> &mut T {
        unsafe { self.contract.as_ref().unwrap_unchecked().as_mut() }
    }
}

impl<T> Clone for VisitorView<T> {
    fn clone(&self) -> Self {
        unsafe { self.contract.as_ref().unwrap_unchecked().increment_unchecked() };
        VisitorView { contract: self.contract }
    }
}

impl<T> Drop for VisitorView<T> {
    fn drop(&mut self) {
        unsafe { self.contract.as_ref().unwrap_unchecked().decrement() };
    }
}

/// Represents a contract allowing memory borrowing using async/await.
/// 
/// Even in the case of direct ownership, no &mut can touch the memory to prevent aliasing issues unless all borrows have ended.
#[derive(Debug)]
struct Contract<T> {
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
    /// +----+--------------+------------+
    /// | 0  | ------- : -- | owner : ^/ |
    /// +----+--------------+------------+
    /// | 1  | visitor : ^/ | owner : >< |
    /// +----+--------------+------------+
    /// | 2+ | visitor : >< | owner : ^/ |
    /// +----+--------------+------------+
    /// ```
    waker: UnsafeCell<MaybeUninit<Waker>>,
    /// The stored value that is being borrowed.
    ///
    /// May be modified based on counter state:
    ///
    /// ```no_run
    /// +----+--------------+------------+
    /// | 0  | ------- : -- | owner : ^/ |
    /// +----+--------------+------------+
    /// | 1  | visitor : >< | owner : >< |
    /// +----+--------------+------------+
    /// | 2+ | visitor : ^/ | owner : >< |
    /// +----+--------------+------------+
    /// ```
    val: UnsafeCell<MaybeUninit<T>>,
    _shared: PhantomPinned,
}

impl<T> Contract<T> {
    pub fn new(val: T) -> Self {
        Contract {
            counter: AtomicUsize::new(0),
            waker: UnsafeCell::new(MaybeUninit::uninit()),
            _shared: PhantomPinned,
            val: UnsafeCell::new(MaybeUninit::new(val)),
        }
    }

    /// Increment the count assuming there is a non-dropping visitor currently allocated.
    ///
    /// **only visitors may safely call.**
    pub unsafe fn increment_unchecked(&self) -> usize {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Increment the count to aquire unique access and set the waker.
    /// 
    /// **owner must be unique.**
    pub unsafe fn try_set_waker(&self, cx: &mut std::task::Context<'_>) -> Option<()> {
        loop {
            let n = self.counter.load(Ordering::Relaxed);
            if n == 0 {
                // no visitors
                return None;
            }
            if n == 1 {
                // already aquired
                continue;
            }
            if let Err(_) = self.counter.compare_exchange(n, n + 1, Ordering::Acquire, Ordering::Relaxed) {
                // exchange failed
                continue;
            }
            // gaurded against drop
            unsafe {
                // go go sanchez skii shoes
                *self.waker.get().as_mut().unwrap_unchecked().assume_init_mut() = cx.waker().clone();
            }
            // release the counter
            self.counter.store(n, Ordering::Release);
            break Some(());
        }
    }

    /// Initlize the contract with a visitor and a noop waker.
    ///
    /// *will fail to drop waker if the counter is not 0.*
    pub fn init_visitor(&mut self) {
        self.counter = 2.into();
        *self.waker.get_mut() = MaybeUninit::new(futures::task::noop_waker());
    }

    pub unsafe fn as_ref(&self) -> &T {
        unsafe { self.val.get().as_ref().unwrap_unchecked().assume_init_ref() }
    }

    pub unsafe fn as_mut(&self) -> &mut T {
        unsafe { self.val.get().as_mut().unwrap_unchecked().assume_init_mut() }
    }

    #[allow(unused)]
    pub fn increment(&self) -> usize {
        loop {
            let n = self.counter.load(Ordering::Relaxed);
            if n == 1 {
                continue;
            }
            if n == 0 {
                match self
                    .counter
                    .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                {
                    Ok(_) => {
                        unsafe {
                                self.waker.get().as_mut().unwrap_unchecked().write(futures::task::noop_waker());
                        }
                        self.counter.store(2, Ordering::Release);
                        break 0;
                    },
                    Err(_) => continue,
                }
            } else {
                match self
                    .counter
                    .compare_exchange(n, n + 1, Ordering::Relaxed, Ordering::Relaxed)
                {
                    Ok(_) => break n,
                    Err(_) => continue,
                }
            };
            #[allow(unreachable_code)]
            {
                unreachable!()
            }
        }
    }

    pub fn decrement(&self) -> usize {
        loop {
            let n = self.counter.load(Ordering::Relaxed);
            if n == 1 {
                continue;
            }
            if n == 2 {
                match self
                    .counter
                    .compare_exchange(2, 1, Ordering::Acquire, Ordering::Relaxed)
                {
                    Ok(_) => {
                        unsafe {
                            std::mem::replace(
                                self.waker.get().as_mut().unwrap_unchecked(),
                                MaybeUninit::uninit(),
                            )
                            .assume_init()
                            .wake();
                        };
                        self.counter.store(0, Ordering::Release);
                        break n;
                    },
                    Err(_) => continue,
                }
            } else {
                match self
                    .counter
                    .compare_exchange(n, n - 1, Ordering::Release, Ordering::Relaxed)
                {
                    Ok(_) => break n,
                    Err(_) => continue,
                }
            };
            #[allow(unreachable_code)]
            {
                unreachable!()
            }
        }
    }

    pub unsafe fn take_val(&self) -> T {
        std::mem::replace(self.val.get().as_mut().unwrap_unchecked(), MaybeUninit::uninit()).assume_init()
    }
}

/// A lifetimeless reference to a value managed by an async cotract.
pub struct Ref</* Out */ T> {
    visitor: VisitorView<T>,
    _co_varience: PhantomData<*const /* Out */ T>,
}

unsafe impl<T: Sync> Sync for Ref<T> {}

unsafe impl<T: Sync> Send for Ref<T> {}


impl<T> Deref for Ref<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self
            .visitor
            .as_ref()
    }
}

impl<T> Clone for Ref<T> {
    fn clone(&self) -> Self {
        Ref { visitor: self.visitor.clone(), _co_varience: PhantomData }
    }
}

/// A lifetimeless upgradable reference to a value managed by an async cotract.
/// 
/// Requires `T: Send` to be able to be sent between threads to allow for constraint free upgrading.
pub struct UpgRef</* Mix */ T> {
    visitor: VisitorView<T>,
    _mixed_varience: PhantomData<*mut /* Mix */ T>,
}

unsafe impl<T: Sync> Sync for UpgRef<T> {}

unsafe impl<T: Sync + Send> Send for UpgRef<T> {}

impl<T> Deref for UpgRef<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self
            .visitor
            .as_ref()
    }
}

impl<T> UpgRef<T> {
    /// Tries to upgrade to a mutable reference.
    /// 
    /// Fails if other references exist.
    pub fn upgrade(self) -> Result<RefMut<T>, Self> {
        match unsafe { self
            .visitor
            .contract
            .as_ref()
            .unwrap_unchecked()
            .counter
            .compare_exchange(2, 2, Ordering::Acquire, Ordering::Relaxed)
        } {
            Ok(_) => Ok(RefMut { visitor: self.visitor, _mixed_varience: PhantomData }),
            Err(_) => Err(self),
        }
    }

    /// Trades upgradable access for easier sending.
    pub fn as_ref(self) -> Ref<T> {
        Ref { visitor: self.visitor, _co_varience: PhantomData }
    }
}

impl<T> Clone for UpgRef<T> {
    fn clone(&self) -> Self {
        UpgRef { visitor: self.visitor.clone(), _mixed_varience: PhantomData }
    }
}

/// A lifetimeless mutable reference to a value managed by an async cotract..
pub struct RefMut</* Mix */ T> {
    visitor: VisitorView<T>,
    _mixed_varience: PhantomData<*mut /* Mix */ T>,
}

unsafe impl<T: Sync> Sync for RefMut<T> {}

unsafe impl<T: Send> Send for RefMut<T> {}

impl<T> Deref for RefMut<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self
            .visitor
            .as_ref()
    }
}

impl<T> DerefMut for RefMut<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self
            .visitor
            .as_mut()
        }
    }
}

impl<T> RefMut<T> {
    /// Downgrades the reference to an upgradable reference.
    /// 
    /// Most suitable for `Send` types.
    pub fn downgrade(self) -> UpgRef<T> {
        UpgRef { visitor: self.visitor, _mixed_varience: PhantomData }
    }

    /// Trades mutable access for cloning.
    pub fn as_ref(self) -> Ref<T> {
        Ref { visitor: self.visitor, _co_varience: PhantomData }
    }
}

impl<T> From<RefMut<T>> for Ref<T> {
    fn from(value: RefMut<T>) -> Self {
        value.as_ref()
    }
}

impl<T> From<UpgRef<T>> for Ref<T> {
    fn from(value: UpgRef<T>) -> Self {
        value.as_ref()
    }
}

pub struct Share</* Out */ 'owner, /* Mix */ T> {
    owner: OwnerView<'owner, T>,
    _mixed_varience: PhantomData<*mut /* Mix */ T>
}

impl<'owner, T> Share<'owner, T> {
    pub async fn aquire(self) -> Borrow<'owner, T> {
        Borrow { unique: self.owner.aquire().await, _mixed_varience: PhantomData }
    }

    pub(crate) unsafe fn aquire_unchecked(self) -> Borrow<'owner, T> {
        Borrow { unique: self.owner.aquire_unchecked(), _mixed_varience: PhantomData }
    }

    pub fn from_unique(unique: Borrow<'owner, T>) -> Self {
        Share { owner: unique.unique.as_owner(), _mixed_varience: PhantomData }
    }
}

impl<'owner, T> From<Borrow<'owner, T>> for Share<'owner, T> {
    fn from(unique: Borrow<'owner, T>) -> Self {
        Self::from_unique(unique)
    }
}

pub struct Borrow</* Out */ 'owner, /* Mix */ T> {
    unique: UniqueView<'owner, T>,
    _mixed_varience: PhantomData<*mut /* Mix */ T>
}

impl<'owner, T> Borrow<'owner, T> {
    pub fn borrow_ref(self) -> (Share<'owner, T>, Ref<T>) {
        let (owner, visitor) = self.unique.share();
        (Share { owner, _mixed_varience: PhantomData }, Ref { visitor, _co_varience: PhantomData })
    }

    pub fn borrow_mut(self) -> (Share<'owner, T>, RefMut<T>) {
        let (owner, visitor) = self.unique.share();
        (Share { owner, _mixed_varience: PhantomData }, RefMut { visitor, _mixed_varience: PhantomData })
    }

    pub fn as_ref(self) -> Ref<T> {
        self.borrow_ref().1
    }

    pub fn as_mut(self) -> RefMut<T> {
        self.borrow_mut().1
    }
}

impl<'owner, T> Deref for Borrow<'owner, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.unique.as_ref()
    }
}

impl<'owner, T> DerefMut for Borrow<'owner, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.unique.as_mut()
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
