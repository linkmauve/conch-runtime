#![deny(rust_2018_idioms)]
use conch_runtime;
use futures;

use conch_runtime::spawn::{for_args, for_loop, for_with_args};
use conch_runtime::RefCounted;
use futures::future::{ok, FutureResult};
use std::sync::Arc;

#[macro_use]
mod support;
pub use self::support::*;

macro_rules! run_env {
    ($future:expr, $env:expr) => {{
        Compat01As03::new($future.pin_env($env.sub_env()).flatten()).await
    }};
}

const MOCK_EXIT: ExitStatus = ExitStatus::Code(42);
const VAR: &str = "var name";
const RESULT_VAR: &str = "resulting var name";

#[derive(Debug, Clone)]
struct MockCmd2;

impl<'a> Spawn<DefaultEnvArc> for &'a MockCmd2 {
    type Error = MockErr;
    type EnvFuture = Self;
    type Future = FutureResult<ExitStatus, Self::Error>;

    fn spawn(self, _: &DefaultEnvArc) -> Self::EnvFuture {
        self
    }
}

impl<'a> EnvFuture<DefaultEnvArc> for &'a MockCmd2 {
    type Item = FutureResult<ExitStatus, Self::Error>;
    type Error = MockErr;

    fn poll(&mut self, env: &mut DefaultEnvArc) -> Poll<Self::Item, Self::Error> {
        let result_var = RESULT_VAR.to_owned();
        let mut result_val = env
            .var(&result_var)
            .cloned()
            .unwrap_or_else(|| Arc::new(String::new()));

        if let Some(val) = env.var(&VAR.to_owned()) {
            result_val.make_mut().push_str(&**val);
        }

        env.set_var(Arc::new(result_var), result_val);
        Ok(Async::Ready(ok(MOCK_EXIT)))
    }

    fn cancel(&mut self, _env: &mut DefaultEnvArc) {
        unimplemented!()
    }
}

#[tokio::test]
async fn should_run_with_appropriate_args() {
    should_run_with_appropriate_args_impl().await
}

async fn should_run_with_appropriate_args_impl() {
    let mut env = new_env();
    env.set_args(Arc::new(vec![
        Arc::new("arg_foo".to_owned()),
        Arc::new("arg_bar".to_owned()),
    ]));

    let result_var = Arc::new(RESULT_VAR.to_owned());
    let name = Arc::new(VAR.to_owned());
    let vars_raw = vec!["raw_foo".to_owned(), "raw_bar".to_owned()];
    let vars = mock_word_fields(Fields::Split(vars_raw.clone()));
    let cmd = MockCmd2;

    macro_rules! run_env_and_assert_var {
        ($future:expr, $env:expr, $value:expr) => {{
            let mut env = $env;
            env.unset_var(&result_var);

            let mut future = $future;
            let mut next_future = future.poll(&mut env);
            while let Ok(Async::NotReady) = next_future {
                next_future = future.poll(&mut env);
            }

            let next_future = match next_future.expect("did not resolve successfully") {
                Async::Ready(n) => n,
                Async::NotReady => unreachable!(),
            };

            let ret = Compat01As03::new(next_future).await;
            assert_eq!(ret, Ok(MOCK_EXIT));
            assert_eq!(&**env.var(&result_var).unwrap(), $value);
        }};
    }

    {
        let env = env.sub_env();

        let for_cmd = for_loop(name.clone(), Some(vec![vars.clone()]), vec![&cmd], &env);
        run_env_and_assert_var!(for_cmd, env, "raw_fooraw_bar");
    }

    {
        let env = env.sub_env();

        let no_word: Option<Vec<MockWord>> = None;
        let for_cmd = for_loop(name.clone(), no_word, vec![&cmd], &env);
        run_env_and_assert_var!(for_cmd, env, "arg_fooarg_bar");
    }

    {
        let env = env.sub_env();
        let for_cmd = for_args(name.clone(), vec![&cmd], &env);
        run_env_and_assert_var!(for_cmd, env, "arg_fooarg_bar");
    }

    {
        let vars_raw = vars_raw.into_iter().map(Arc::new);
        let for_cmd = for_with_args(name.clone(), vars_raw, vec![&cmd]);
        run_env_and_assert_var!(for_cmd, env, "raw_fooraw_bar");
    }
}

#[tokio::test]
async fn should_swallow_non_fatal_errors_in_body() {
    let mut env = new_env();
    env.set_args(Arc::new(vec![
        Arc::new("arg_foo".to_owned()),
        Arc::new("arg_bar".to_owned()),
    ]));

    let name = Arc::new("name".to_owned());
    let vars = mock_word_fields(Fields::Single((*name).clone()));

    let non_fatal = mock_error(false);
    let cmd = mock_status(MOCK_EXIT);

    let for_cmd = for_loop(
        name.clone(),
        Some(vec![vars.clone()]),
        vec![&non_fatal, &cmd],
        &env,
    );
    assert_eq!(run_env!(for_cmd, env), Ok(MOCK_EXIT));

    let no_word: Option<Vec<MockWord>> = None;
    let for_cmd = for_loop(name.clone(), no_word, vec![&non_fatal, &cmd], &env);
    assert_eq!(run_env!(for_cmd, env), Ok(MOCK_EXIT));

    let for_cmd = for_args(name.clone(), vec![&non_fatal, &cmd], &env);
    assert_eq!(run_env!(for_cmd, env), Ok(MOCK_EXIT));

    let for_cmd = for_with_args(name.clone(), vec![name.clone()], vec![&non_fatal, &cmd]);
    assert_eq!(run_env!(for_cmd, env), Ok(MOCK_EXIT));
}

#[tokio::test]
async fn should_not_run_body_args_are_empty() {
    let mut env = new_env();
    env.set_args(Arc::new(vec![]));

    let should_not_run = mock_panic("must not run");
    let name = Arc::new("name".to_owned());
    let vars = mock_word_fields(Fields::Zero);

    let for_cmd = for_loop(
        name.clone(),
        Some(vec![vars.clone()]),
        vec![&should_not_run],
        &env,
    );
    assert_eq!(run_env!(for_cmd, env), Ok(EXIT_SUCCESS));

    let no_word: Option<Vec<MockWord>> = None;
    let for_cmd = for_loop(name.clone(), no_word, vec![&should_not_run], &env);
    assert_eq!(run_env!(for_cmd, env), Ok(EXIT_SUCCESS));

    let for_cmd = for_args(name.clone(), vec![&should_not_run], &env);
    assert_eq!(run_env!(for_cmd, env), Ok(EXIT_SUCCESS));

    let for_cmd = for_with_args(name.clone(), vec![], vec![&should_not_run]);
    assert_eq!(run_env!(for_cmd, env), Ok(EXIT_SUCCESS));
}

#[tokio::test]
async fn should_propagate_all_word_errors() {
    let env = new_env();

    let should_not_run = mock_panic("must not run");
    let name = Arc::new("name".to_owned());

    let for_cmd = for_loop(
        name.clone(),
        Some(vec![mock_word_error(true)]),
        vec![&should_not_run],
        &env,
    );
    assert_eq!(run_env!(for_cmd, env), Err(MockErr::Fatal(true)));

    let for_cmd = for_loop(
        name.clone(),
        Some(vec![mock_word_error(false)]),
        vec![&should_not_run],
        &env,
    );
    assert_eq!(run_env!(for_cmd, env), Err(MockErr::Fatal(false)));
}

#[tokio::test]
async fn should_propagate_fatal_errors_in_body() {
    let mut env = new_env();
    env.set_args(Arc::new(vec![
        Arc::new("foo".to_owned()),
        Arc::new("bar".to_owned()),
    ]));

    let name = Arc::new("name".to_owned());
    let vars_raw = vec!["foo".to_owned(), "bar".to_owned()];
    let vars = mock_word_fields(Fields::Split(vars_raw.clone()));
    let fatal = mock_error(true);

    let for_cmd = for_loop(name.clone(), Some(vec![vars.clone()]), vec![&fatal], &env);
    assert_eq!(run_env!(for_cmd, env), Err(MockErr::Fatal(true)));

    let no_word: Option<Vec<MockWord>> = None;
    let for_cmd = for_loop(name.clone(), no_word, vec![&fatal], &env);
    assert_eq!(run_env!(for_cmd, env), Err(MockErr::Fatal(true)));

    let for_cmd = for_args(name.clone(), vec![&fatal], &env);
    assert_eq!(run_env!(for_cmd, env), Err(MockErr::Fatal(true)));

    let vars_raw = vars_raw.into_iter().map(Arc::new);
    let for_cmd = for_with_args(name.clone(), vars_raw, vec![&fatal]);
    assert_eq!(run_env!(for_cmd, env), Err(MockErr::Fatal(true)));
}

#[tokio::test]
async fn should_propagate_cancel() {
    let mut env = new_env();
    env.set_args(Arc::new(vec![
        Arc::new("foo".to_owned()),
        Arc::new("bar".to_owned()),
    ]));

    let name = Arc::new("name".to_owned());
    let vars_raw = vec!["foo".to_owned(), "bar".to_owned()];
    let vars = mock_word_fields(Fields::Split(vars_raw.clone()));
    let should_not_run = mock_panic("must not run");
    let must_cancel = mock_must_cancel();

    let for_cmd = for_loop(
        name.clone(),
        Some(vec![mock_word_must_cancel()]),
        vec![&should_not_run],
        &env,
    );
    test_cancel!(for_cmd, env);

    let for_cmd = for_loop(
        name.clone(),
        Some(vec![vars.clone()]),
        vec![&must_cancel],
        &env,
    );
    test_cancel!(for_cmd, env);

    let for_cmd = for_args(name.clone(), vec![&must_cancel], &env);
    test_cancel!(for_cmd, env);

    let vars_raw = vars_raw.into_iter().map(Arc::new);
    let for_cmd = for_with_args(name.clone(), vars_raw, vec![&must_cancel]);
    test_cancel!(for_cmd, env);
}
