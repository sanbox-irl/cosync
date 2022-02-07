# cosync

This crate provides a single threaded, sequential, parameterized async runtime.
In other words, this creates *coroutines*, specifically targetting video game logic,
though it's suitable for creating any sequences of directions which take time.

Here's a basic `Cosync` example:

```rust
# use cosync::{Cosync, CosyncInput};
// the type parameter is the *value* which other functions will get.
let mut cosync: Cosync<i32> = Cosync::new();
let example_move = 20;
// there are a few ways to queue tasks, but here's a simple one:
cosync.queue(move |mut input: CosyncInput<i32>| async move {
    // set our input to `example_move`...
    *input.get() = example_move;
});
let mut value = 0;
cosync.run_until_stall(&mut value);
assert_eq!(value, example_move);
```

`Cosync` is **not** multithreaded, nor parallel -- it works entirely sequentially. Think of it as a useful way of expressing code that is multistaged and takes *time* to complete. Moving cameras, staging actors, and performing animations often work well with `Cosync`. Loading asset files, doing mathematical computations, or doing IO should be done by more easily multithreaded runtimes.

This crate exposes two methods for driving the runtime: `run_until_stall`
and `run_blocking`. You generally want `run_until_stall`, which attempts to do
the task given, until it cannot (ie, is waiting), at which point is returns
control to the caller.

Finally, this crate supports sending tasks from other threads. There are three ways
to make new tasks. First, the `Cosync` struct itself has a `queue` method on it. Secondly,
each task gets a `CosyncInput<T>` as a parameter, which has `get` (to get access to your `&mut
T`) and `queue` to queue another task (which is at the end of the queue, not necessarily after
the task which added it). Lastly, you can create a `CosyncQueueHandle` with
`Cosync::create_queue_handle` which is `Send` and can be given to other threads to create new
tasks for the `Cosync`.

This crate depends on only `std`. It is in an early state of development, but is in
production ready state right now.
