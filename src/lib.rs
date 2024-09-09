use std::{
    ops::{Deref, DerefMut},
    sync::mpsc,
};

use futures::channel::oneshot;

pub trait Sender<T> {
    fn fire_and_forget(self, t: T);
}

impl<T> Sender<T> for oneshot::Sender<T> {
    fn fire_and_forget(self, t: T) {
        self.send(t).unwrap_or_else(|t| drop(t))
    }
}

impl<T> Sender<T> for mpsc::Sender<T> {
    fn fire_and_forget(self, t: T) {
        self.send(t).unwrap_or_else(|mpsc::SendError(t)| drop(t))
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Final;

impl<T> Sender<T> for Final {
    fn fire_and_forget(self, t: T) {
        drop(t)
    }
}

/// By-value memory storage without by-value memory access.
///
/// *our princess is in another castle!*
#[derive(Debug, Clone)]
pub struct Guest<T, S: Sender<T>> {
    value: std::mem::ManuallyDrop<T>,
    sender: std::mem::ManuallyDrop<S>,
}

impl<T, S: Sender<T>> Guest<T, S> {
    /// Creates a new guest and runs the callback on drop.
    pub fn new(value: T, sender: S) -> Self {
        Guest {
            value: std::mem::ManuallyDrop::new(value),
            sender: std::mem::ManuallyDrop::new(sender),
        }
    }
}

impl<T, S: Sender<T>> Drop for Guest<T, S> {
    fn drop(&mut self) {
        let value = unsafe { std::mem::ManuallyDrop::take(&mut self.value) };
        (unsafe { std::mem::ManuallyDrop::take(&mut self.sender) }).fire_and_forget(value)
    }
}

impl<T, S: Sender<T>> Deref for Guest<T, S> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &*self.value
    }
}

impl<T, S: Sender<T>> DerefMut for Guest<T, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.value
    }
}

pub type OneshotGuest<T> = Guest<T, oneshot::Sender<T>>;

pub type MultiGuest<T> = Guest<T, mpsc::Sender<T>>;
