use std::ops::{Deref, DerefMut};

/// By-value memory storage without by-value memory access.
/// 
/// *our princess is in another castle!*
pub struct Guest<T, F: FnOnce(T) = Box<dyn FnOnce(T)>> {
    value: std::mem::ManuallyDrop<T>,
    consumer: std::mem::ManuallyDrop<F>
}

pub fn take_back<'a, T>(owner: &'a mut Option<T>) -> Guest<T, impl FnOnce(T) + 'a> {
    Guest::new(owner.take().unwrap(), |last| *owner = Some(last))
}

impl<T, F: FnOnce(T)> Guest<T, F> {
    /// Creates a new guest and runs the callback on drop.
    pub fn new(value: T, callback: F) -> Self {
        Guest {
            value: std::mem::ManuallyDrop::new(value),
            consumer: std::mem::ManuallyDrop::new(callback)
        }
    }

    /// Maps the memory with an accompanying map back for after the journey is complete.
    /// 
    /// *"There and Back Again"*
    pub fn extend<U, G: FnOnce(U) -> T>(mut self, extension: impl FnOnce(T) -> U, retraction: G)
        -> Guest<U, impl FnOnce(U)>
    {
        let extended = (extension)(unsafe { std::mem::ManuallyDrop::take(&mut self.value) });
        let consumer = unsafe { std::mem::ManuallyDrop::take(&mut self.consumer) };
        std::mem::forget(self);
        Guest {
            value: std::mem::ManuallyDrop::new(extended),
            consumer: std::mem::ManuallyDrop::new(move |last| (consumer)((retraction)(last)))
        }
    }

    /// Converts to a polymorphic form.
    pub fn dynamic<'a>(mut self) -> Guest<T, Box<dyn FnOnce(T) + 'a>> where F: 'a {
        let value = unsafe { std::mem::ManuallyDrop::take(&mut self.value) };
        let consumer = unsafe { std::mem::ManuallyDrop::take(&mut self.consumer) };
        std::mem::forget(self);
        Guest {
            value: std::mem::ManuallyDrop::new(value),
            consumer: std::mem::ManuallyDrop::new(Box::new(consumer))
        }
    }
}

impl<T, F: FnOnce(T)> Drop for Guest<T, F> {
    fn drop(&mut self) {
        let value = unsafe { std::mem::ManuallyDrop::take(&mut self.value) };
        (unsafe { std::mem::ManuallyDrop::take(&mut self.consumer) })(value)
    }
}

impl<T, F: FnOnce(T)> Deref for Guest<T, F> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &*self.value
    }
}

impl<T, F: FnOnce(T)> DerefMut for Guest<T, F> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut*self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_back_test() {
        let mut owner = Some(vec![]);
        {
            let mut guest = take_back(&mut owner);
            let mut a = 0_u64;
            let mut b = 1_u64;
            guest.push(a);
            loop {
                guest.push(b);
                if b > 1<<63 { break }
                b = a + b;
                a = b - a;
            }
            assert_eq!(*guest.last().unwrap(), 12200160415121876738_u64)
        }
        assert_eq!(owner.as_ref().map(|fibs| fibs.len()), Some(94))
    }
}