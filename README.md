# cosync

![docs.rs](https://img.shields.io/docsrs/cosync)
![Crates.io](https://img.shields.io/crates/v/cosync)
![Crates.io](https://img.shields.io/crates/l/cosync)

This crate provides a single-threaded, parameterized async runtime. In other words, this creates *coroutines*, specifically targeting video game logic, though `cosync` is suitable for creating any sequences of directions which take time.

## Quick Start

To install, add the following to your Cargo.toml:

```toml
cosync = { git = "https://github.com/sanbox-irl/cosync" }
```

or run

```sh
cargo add cosync
```

You can make and use a `Cosync` as follows:

```rust
use cosync::{Cosync, CosyncInput};

fn main() {
    // the type parameter is the *value* which other functions will get.
    let mut cosync: Cosync<i32> = Cosync::new();
    let example_move = 20;

    // there are a few ways to queue tasks, but here's a simple one:
    cosync.queue(move |mut input: CosyncInput<i32>| async move {
        // set our input to `example_move`...
        *input.get() = example_move;
    });

    let mut value = 0;
    cosync.run(&mut value);

    // okay, we ran our future, and since it has no awaits, we know
    // it will have completed!
    assert_eq!(value, example_move);
}
```

Additionally, `Cosync` can handle unsized Ts, including dynamic dispatch:

```rust
use cosync::Cosync;

// unsized type
let mut cosync: Cosync<str> = Cosync::new();
cosync.queue(|mut input| async move {
    let input_guard = input.get();
    let inner_str: &str = &input_guard;
    println!("inner str = {}", inner_str);
});

// dynamic dispatch
trait DynDispatch {
    fn test(&self);
}
let mut cosync_dyn: Cosync<dyn DynDispatch> = Cosync::new();
cosync_dyn.queue(|mut input| async move {
    let inner: &mut dyn DynDispatch = &mut *input.get();
});
```

## Overview

`Cosync` is **not** multithreaded or parallel -- it works fundamentally sequentially, though if provided more than one task, it will try to make progress on all of them. In this library, we refer to that idea as "concurrent" tasks, though once again, they are fundamentally single-threaded, and therefore, sequential.

A `Cosync` is a useful way of expressing code that is multistaged, that takes *time* to complete, or that you want to do *later*. Moving cameras, staging actors, performing animations, or reacting to button presses often work well with `Cosync`. Loading asset files, doing mathematical computations, or doing IO should be done by more easily multithreaded runtimes such as [switchyard](https://github.com/BVE-Reborn/switchyard).

This crate exposes two methods for driving the runtime: `run` and `run_blocking`. You generally want `run`, which attempts process as much of the queue as it can, until it cannot (ie, a future returns `Poll::Pending`), at which point control is returned to the caller.

There are three ways to make new tasks. First, the `Cosync` struct itself has a `queue` method on it. Secondly, each task gets a `CosyncInput<T>` as a parameter, which has `get` (to get access to your `&mut T`) and `queue` to queue another task (which is at the end of the queue, not necessarily after the task which added it). Lastly, you can create a `CosyncQueueHandle` with `Cosync::create_queue_handle` which is `Send` and can be given to other threads to create new tasks for the `Cosync`.

## Waiting Around

`Cosync` includes the utility function `sleep_ticks`. This function returns a future which you can `.await`, which will continue the task after the specified number of ticks. A `tick` occurs every time `run` or `run_blocking` is called.

Right now, it is implemented simply, but in the future, it will become better. This can allow for more efficient code in the future.

## Waking Tasks

Tasks which return a `Poll::Pending` must also wake the waker, with `cx.waker().wake_by_ref()` at some point in the future. This is temporary in the library -- likely in the future, all tasks will always assume they are awake.

## Handling Multiple Tasks

If two or more tasks are queued, the next time `run` is called, progress will be made on both of them. Cosync will *always* do work on the first queued task, and then, whether or not the first task completes (ie, returns `Poll::Result(())`), do work on the second task.

## `SerialCosync`

In the early days of this library, this wasn't the case. Instead, `Cosync` would do only *one* task at a time, and if, and only if, a task returned `Poll::Result(())` would `Cosync` move onto the next task. This is extremely useful in certain contexts, so a modified `Cosync` with that behavior is available as `SerialCosync`. It has the same API, though slightly modified to reflect that only task is running at a time. See `examples/serial.rs` for an example of it in practice.

This crate depends on only `std`. It is in an early state of development, but is in production ready state right now.
