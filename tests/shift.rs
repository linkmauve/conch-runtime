extern crate futures;

use futures::future::poll_fn;
use std::rc::Rc;

mod support;
pub use self::support::spawn::builtin::shift;
pub use self::support::*;

fn run_shift(
    env_args_starting: &[&str],
    shift_args: &[&str],
    env_args_expected: &[&str],
    expected_status: ExitStatus,
) {
    // NB: Suppress usage dumping errors to console
    let (mut lp, mut env) = new_env_with_no_fds();
    env.set_args(Rc::new(
        env_args_starting
            .iter()
            .map(|&s| s.to_owned())
            .map(Rc::new)
            .collect::<Vec<_>>(),
    ));

    let shift = shift(shift_args.iter().map(|&s| s.to_owned()));

    let mut shift = shift.spawn(&env);
    let exit = lp
        .run(poll_fn(|| shift.poll(&mut env)).flatten())
        .expect("command failed");

    assert_eq!(exit, expected_status);

    let env_args_expected = env_args_expected
        .iter()
        .map(|&s| s.to_owned())
        .map(Rc::new)
        .collect::<Vec<_>>();
    assert_eq!(env.args(), env_args_expected);
}

#[test]
fn shift_with_args() {
    let args = &["a", "b", "d", "e", "f"];
    run_shift(args, &["3"], &args[3..], EXIT_SUCCESS);
}

#[test]
fn shift_no_args_shifts_by_one() {
    let args = &["a", "b"];
    run_shift(args, &[], &args[1..], EXIT_SUCCESS);
}

#[test]
fn shift_negative_arg_does_nothing_and_exit_with_error() {
    let args = &["a", "b"];
    run_shift(args, &["-5"], args, EXIT_ERROR);
}

#[test]
fn shift_large_arg_does_nothing_and_exit_with_error() {
    let args = &["a", "b"];
    run_shift(args, &["3"], args, EXIT_ERROR);
}

#[test]
fn shift_non_numeric_arg_does_nothing_and_exit_with_error() {
    let args = &["a", "b"];
    run_shift(args, &["foobar"], args, EXIT_ERROR);
}

#[test]
fn shift_multiple_arg_does_nothing_and_exit_with_error() {
    let args = &["a", "b"];
    run_shift(args, &["1", "2"], args, EXIT_ERROR);
}

#[test]
#[should_panic]
fn polling_canceled_shift_panics() {
    let (_, mut env) = new_env_with_no_fds();
    let mut shift = shift(Vec::<Rc<String>>::new()).spawn(&env);

    shift.cancel(&mut env);
    let _ = shift.poll(&mut env);
}
