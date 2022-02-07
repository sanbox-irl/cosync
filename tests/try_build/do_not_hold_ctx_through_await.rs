use cosync::Cosync;

pub fn main() {
    let mut executor = Cosync::new();

    executor.queue(move |mut input| async move {
        let mut inner_input = input.get();
        let inner_inner_input: &mut i32 = &mut inner_input;

        let sleep = cosync::sleep_ticks(1).await;

        let input = inner_inner_input;

        assert_eq!(*input, 100);
    });

    let mut one_value = 10;
    executor.run_until_stalled(&mut one_value);
    let mut two_value = 100;
    executor.run_until_stalled(&mut two_value);
}
