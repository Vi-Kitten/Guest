use futures::{future::FusedFuture, FutureExt};

use super::*;

/// Contains sharable memory.
/// 
/// **will leak memory if not awaited.**
#[must_use = "will leak memory if not awaited"]
pub struct ShareBox<T> {
    contract: *const Contract<T>,
}

impl<T> ShareBox<T> {
    pub fn new(val: T) -> Self {
        Self { contract: Box::into_raw(Box::new(val)) as *const _ }
    }

    pub fn new_with<R>(val: T, f: impl for<'owner> FnOnce(Borrow<'owner, T>) -> R) -> (Self, R) {
        let mut owner = Self::new(val);
        let gaurd = unsafe { owner.share().aquire_unchecked() };
        let res = (f)(gaurd);
        (owner, res)
    }

    pub fn new_with_ref(val: T) -> (Self, Ref<T>) {
        Self::new_with(val, |borrow| borrow.as_ref())
    }

    pub fn new_with_mut(val: T) -> (Self, RefMut<T>) {
        Self::new_with(val, |borrow| borrow.as_mut())
    }

    pub fn share<'owner>(&'owner mut self) -> Share<'owner, T> {
        if self.contract.is_null() {
            panic!("cannot share null");
        }
        Share {
            owner: unsafe { OwnerView::from_raw(self.contract) }
        }
    }
}

impl<T> Future for ShareBox<T> {
    type Output = T;
    
    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        if self.contract.is_null() {
            panic!("cannot await null");
        }
        unsafe { OwnerView::from_raw(self.contract).poll_unpin(cx) }.map(|_| {
            let mem = unsafe { Box::from_raw(self.contract as *mut Contract<T>) };
            self.contract = std::ptr::null();
            let val = unsafe { mem.take_val() };
            drop(mem);
            val
        })
    }
}

impl<T> FusedFuture for ShareBox<T> {
    fn is_terminated(&self) -> bool {
        self.contract.is_null()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;
    use super::ShareBox;

    #[tokio::test]
    pub async fn vibe_check() {
        let (counter, reference) = ShareBox::<AtomicU32>::new_with_ref(0.into());
        {
            let this_reference = reference.clone();
            tokio::spawn(async move {
                this_reference.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
        }
        {
            let this_reference = reference.clone();
            tokio::spawn(async move {
                this_reference.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
        }
        {
            let this_reference = reference.clone();
            tokio::spawn(async move {
                this_reference.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
        }
        drop(reference);
        assert_eq!(counter.await.into_inner(), 3);
    }
}