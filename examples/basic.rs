use cosync::{Cosync, CosyncQueueHandle};

struct Game {
    cosync: Cosync<World>,
    world: World,
}

struct World {
    player_position: f32,
    monster_position: f32,
    /// you'll use this in game to queue tasks, assuming you don't want to pass around `Game` and
    /// just pass around `World`. You can also use this in `cosync.queue` to queue stuff, but this
    /// is fine too.
    cosync_handle: CosyncQueueHandle<World>,
    cancel_game: bool,
}

fn main() {
    let cosync = Cosync::new();
    let world = World {
        player_position: 0.0,
        monster_position: 20.0,
        cosync_handle: cosync.create_queue_handle(),
        cancel_game: false,
    };

    let mut game = Game { cosync, world };

    // notice the `move` for the closure to get `pos` to be owned,
    // and then the `async move` moves the `world` param into the async block.
    // In a world where `async closures` exist, you'd theoretically not need as many of these (just a
    // since `async move` before the parameters), but we don't live in that world yet.
    let pos = 2.0;
    game.world
        .cosync_handle
        .queue(move |mut world| async move {
            // get sleepy!
            cosync::sleep_ticks(10).await;
            world.get().player_position = pos;
        })
        .unwrap(); // you don't really need to unwrap here -- this only is `None` is `Cosync` was dropped, which is rare.

    game.cosync.queue(move |mut world| async move {
        world.get().monster_position = pos;

        // it takes 100 ticks to defeat the monster
        cosync::sleep_ticks(100).await;

        world.get().cancel_game = true;
    });

    // this is your main loop
    loop {
        // note that both of our tasks are actually running at the same time.
        // since the monster task takes 100 ticks to complete, it will complete later than
        // the first task.
        game.cosync.run(&mut game.world);

        if game.world.cancel_game {
            break;
        }
    }

    assert_eq!(game.world.player_position, 2.0);
    assert_eq!(game.world.monster_position, 2.0);
}
