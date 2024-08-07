use std::ops::{Deref, DerefMut};

/// By-value memory storage without by-value memory access.
///
/// *our princess is in another castle!*
#[derive(Debug, Clone, Hash)]
pub struct Guest<T, F: FnOnce(T) = Box<dyn FnOnce(T)>> {
    value: std::mem::ManuallyDrop<T>,
    consumer: std::mem::ManuallyDrop<F>,
}

/// Pull value from and then write back to an option.
pub fn take_back<'a, T>(owner: &'a mut Option<T>) -> Guest<T, impl FnOnce(T) + 'a> {
    Guest::new(owner.take().unwrap(), |last| *owner = Some(last))
}

impl<T, F: FnOnce(T)> Guest<T, F> {
    /// Creates a new guest and runs the callback on drop.
    pub fn new(value: T, callback: F) -> Self {
        Guest {
            value: std::mem::ManuallyDrop::new(value),
            consumer: std::mem::ManuallyDrop::new(callback),
        }
    }

    /// Maps the memory with an accompanying map back for after the journey is complete.
    ///
    /// *"There and Back Again"*
    pub fn extend<U, G: FnOnce(U) -> T>(
        mut self,
        extension: impl FnOnce(T) -> U,
        retraction: G,
    ) -> Guest<U, impl FnOnce(U)> {
        let extended = (extension)(unsafe { std::mem::ManuallyDrop::take(&mut self.value) });
        let consumer = unsafe { std::mem::ManuallyDrop::take(&mut self.consumer) };
        std::mem::forget(self);
        Guest {
            value: std::mem::ManuallyDrop::new(extended),
            consumer: std::mem::ManuallyDrop::new(move |last| (consumer)((retraction)(last))),
        }
    }

    /// Converts to a polymorphic form.
    pub fn dynamic<'a>(mut self) -> Guest<T, Box<dyn FnOnce(T) + 'a>>
    where
        F: 'a,
    {
        let value = unsafe { std::mem::ManuallyDrop::take(&mut self.value) };
        let consumer = unsafe { std::mem::ManuallyDrop::take(&mut self.consumer) };
        std::mem::forget(self);
        Guest {
            value: std::mem::ManuallyDrop::new(value),
            consumer: std::mem::ManuallyDrop::new(Box::new(consumer)),
        }
    }

    /// Swap owners.
    pub fn shuftle(x: &mut Self, y: &mut Self) {
        std::mem::swap(&mut x.consumer, &mut y.consumer)
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
        &mut *self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_text() {
        let nums = std::sync::Mutex::new(vec![]);
        {
            let mut x = Guest::new(vec![], |mut x| nums.lock().unwrap().append(&mut x));
            let mut y = Guest::new(vec![], |mut y| nums.lock().unwrap().append(&mut y));
            let mut z = Guest::new(vec![], |mut z| nums.lock().unwrap().append(&mut z));
            // x
            x.push(0);
            x.push(1);
            x.push(2);
            x.push(3);
            x.push(4);
            drop(x);
            // y
            y.push(0);
            {
                let mut yy = Guest::new(vec![], |yy| {
                    y.append(&mut yy.into_iter().map(|(a, b)| a + b).collect())
                });
                yy.push((0, 1));
                yy.push((1, 1));
                yy.push((1, 2));
            }
            y.push(4);
            drop(y);
            // z
            z.push(0);
            let mut zz = z.extend(
                |zs| zs.into_iter().map(|a| (a, 0)).collect(),
                |zzs: Vec<(i32, i32)>| zzs.into_iter().map(|(a, b)| a + b).collect(),
            );
            zz.push((0, 1));
            zz.push((1, 1));
            zz.push((1, 2));
            zz.push((2, 2));
            drop(zz)
        }
        assert_eq!(
            nums.into_inner().unwrap(),
            vec![0, 1, 2, 3, 4, 0, 1, 2, 3, 4, 0, 1, 2, 3, 4]
        )
    }
}
