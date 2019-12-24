#![deny(rust_2018_idioms)]
use conch_runtime;

use conch_runtime::spawn::{if_cmd, GuardBodyPair};

#[macro_use]
mod support;
pub use self::support::*;

macro_rules! run_env {
    ($future:expr) => {{
        let env = new_env();
        Compat01As03::new($future.pin_env(env).flatten()).await
    }};
}

#[tokio::test]
async fn should_run_body_of_successful_guard() {
    let should_not_run = mock_panic("must not run");
    let exit = ExitStatus::Code(42);

    let cmd = if_cmd(
        vec![
            GuardBodyPair {
                guard: vec![mock_status(EXIT_ERROR)],
                body: vec![should_not_run.clone()],
            },
            GuardBodyPair {
                guard: vec![mock_error(false)],
                body: vec![should_not_run.clone()],
            },
            GuardBodyPair {
                guard: vec![mock_status(EXIT_SUCCESS)],
                body: vec![mock_status(exit)],
            },
        ],
        Some(vec![should_not_run.clone()]),
    );
    assert_eq!(run_env!(cmd), Ok(exit));
}

#[tokio::test]
async fn should_run_else_branch_if_present_and_no_successful_guards() {
    let should_not_run = mock_panic("must not run");
    let exit = ExitStatus::Code(42);

    let cmd = if_cmd(
        vec![GuardBodyPair {
            guard: vec![mock_status(EXIT_ERROR)],
            body: vec![should_not_run.clone()],
        }],
        Some(vec![mock_status(exit)]),
    );
    assert_eq!(run_env!(cmd), Ok(exit));

    let cmd = if_cmd(
        vec![GuardBodyPair {
            guard: vec![mock_status(EXIT_ERROR)],
            body: vec![should_not_run.clone()],
        }],
        None,
    );
    assert_eq!(run_env!(cmd), Ok(EXIT_SUCCESS));

    let cmd = if_cmd(vec![], Some(vec![mock_status(exit)]));
    assert_eq!(run_env!(cmd), Ok(exit));

    let cmd = if_cmd(Vec::<GuardBodyPair<Vec<MockCmd>>>::new(), None);
    assert_eq!(run_env!(cmd), Ok(EXIT_SUCCESS));
}

#[tokio::test]
async fn should_propagate_fatal_errors() {
    let should_not_run = mock_panic("must not run");

    let cmd = if_cmd(
        vec![GuardBodyPair {
            guard: vec![mock_error(true), should_not_run.clone()],
            body: vec![should_not_run.clone()],
        }],
        Some(vec![should_not_run.clone()]),
    );
    assert_eq!(run_env!(cmd), Err(MockErr::Fatal(true)));

    let cmd = if_cmd(vec![], Some(vec![mock_error(true)]));
    assert_eq!(run_env!(cmd), Err(MockErr::Fatal(true)));
}

#[tokio::test]
async fn should_propagate_cancel() {
    let mut env = new_env();

    let should_not_run = mock_panic("must not run");

    let cmd = if_cmd(
        vec![
            GuardBodyPair {
                guard: vec![mock_must_cancel()],
                body: vec![should_not_run.clone()],
            },
            GuardBodyPair {
                guard: vec![should_not_run.clone()],
                body: vec![should_not_run.clone()],
            },
        ],
        Some(vec![should_not_run.clone()]),
    );
    test_cancel!(cmd, env);

    let cmd = if_cmd(
        vec![
            GuardBodyPair {
                guard: vec![mock_status(EXIT_SUCCESS)],
                body: vec![mock_must_cancel()],
            },
            GuardBodyPair {
                guard: vec![should_not_run.clone()],
                body: vec![should_not_run.clone()],
            },
        ],
        Some(vec![should_not_run.clone()]),
    );
    test_cancel!(cmd, env);

    let cmd = if_cmd(vec![], Some(vec![mock_must_cancel()]));
    test_cancel!(cmd, env);
}