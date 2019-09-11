#![deny(rust_2018_idioms)]

use conch_runtime::future::EnvFuture;
use conch_runtime::ExitStatus;
use futures::Future;

#[test]
fn smoke_env_future() {
    let env = ();
    let exit = ExitStatus::Code(42);
    let future = exit.pin_env(env);
    assert_eq!(future.wait(), Ok(exit));
}

#[test]
fn smoke_future() {
    let exit = ExitStatus::Code(42);
    assert_eq!(exit.wait(), Ok(exit));
}
