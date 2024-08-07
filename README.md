# Guest

By-value memory storage without by-value memory access.

## Usage

Guests allow one to have reading and writing access whilst garunteeing that the memory is handed on to the next owner after usage.

Some example types are as follows:

- Read access: `Arc<Guest<T>>`
- Write access: `Arc<Mutex<Guest<T>>>`
- Read and Write access: `Arc<RwLock<Guest<T>>>`

Some example callbacks could be:

- Just dropping: `std::mem::drop`
- Sending through a channel: `move |last| sender.send(last).unwrap()`
- Setting a value: `|last| capture = last`
