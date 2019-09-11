#![deny(rust_2018_idioms)]
mod support;
pub use self::support::spawn::builtin::colon;
pub use self::support::*;

#[test]
fn colon_smoke() {
    let (mut lp, mut env) = new_env();

    let exit = lp.run(colon().spawn(&env).pin_env(&mut env).flatten());

    assert_eq!(exit, Ok(EXIT_SUCCESS));
}
