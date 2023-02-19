use cosync::SerialCosync;

struct Game {
    cosync: SerialCosync<World>,
    world: World,
}

struct World {
    main_actor_position: f32,
    supporting_actor_position: f32,
    cancel_game: bool,
}

fn main() {
    // this would live in a `Game` struct, or something to that effect
    let cosync = SerialCosync::new();

    // this is probably whatever inside `Game` holds your `Ecs`. Because `cosync`
    // requires its `T` to be `'static`, often you need to have something like `World`.
    let world = World {
        main_actor_position: 100.0,
        supporting_actor_position: 0.0,
        cancel_game: false,
    };

    let mut game = Game { cosync, world };

    let main_marks = vec![100.0, 200.0, 300.0];
    let support_marks = vec![0.0, 2.0, 3.0];

    // notice the `move` for the closure to get `pos` to be owned,
    // and then the `async move` moves the `world` param into the async block.
    // In a world where `async closures` exist, you'd theoretically not need as many of these (just a
    // since `async move` before the parameters), but we don't live in that world yet.

    // let's drag this fella around
    game.cosync.queue(move |mut world| async move {
        for pos in main_marks {
            world.get().main_actor_position = pos;

            // let's yield (imagine this as a call to wait till the actor has made it to the position)
            cosync::yield_now().await;
        }
    });

    // and then, ONLY AFTER the main actor has moved, will we move the supporting actor
    game.cosync.queue(move |mut world| async move {
        for pos in support_marks {
            world.get().supporting_actor_position = pos;

            // let's yield (imagine this as a call to wait till the actor has made it to the position)
            cosync::yield_now().await;
        }

        // and now let's stop the game!
        world.get().cancel_game = true;
    });

    // this is your main loop
    loop {
        game.cosync.run(&mut game.world);

        if game.world.cancel_game {
            break;
        }
    }

    assert_eq!(game.world.main_actor_position, 300.0);
    assert_eq!(game.world.supporting_actor_position, 3.0);
}
