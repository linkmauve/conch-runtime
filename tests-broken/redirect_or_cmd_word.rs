#![deny(rust_2018_idioms)]
use conch_runtime;
use futures;

use conch_runtime::env::FileDescEnvironment;
use conch_runtime::eval::RedirectAction;
use conch_runtime::io::Permissions;
use futures::future::poll_fn;

#[macro_use]
mod support;
pub use self::support::*;

#[tokio::test]
async fn smoke() {
    let mut env = new_env_with_no_fds();

    {
        let env = env.sub_env();
        let future = eval_redirects_or_cmd_words::<MockRedirect<_>, MockWord, _, _>(vec![], &env)
            .pin_env(env);
        let (_restorer, words) = Compat01As03::new(future).await.unwrap();
        assert!(words.is_empty());
    }

    assert_eq!(env.file_desc(1), None);
    let fdes = dev_null(&mut env);
    let mut future = eval_redirects_or_cmd_words(
        vec![
            RedirectOrCmdWord::Redirect(mock_redirect(RedirectAction::Open(
                1,
                fdes.clone(),
                Permissions::Write,
            ))),
            RedirectOrCmdWord::CmdWord(mock_word_fields(Fields::Zero)),
            RedirectOrCmdWord::CmdWord(mock_word_fields(Fields::Single("foo".to_owned()))),
            RedirectOrCmdWord::CmdWord(mock_word_fields(Fields::Split(vec![
                "bar".to_owned(),
                "baz".to_owned(),
            ]))),
        ],
        &env,
    );

    let (mut restorer, words) = Compat01As03::new(poll_fn(|| future.poll(&mut env)))
        .await
        .unwrap();

    assert_eq!(env.file_desc(1), Some((&fdes, Permissions::Write)));
    restorer.restore(&mut env);
    assert_eq!(env.file_desc(1), None);

    assert_eq!(
        words,
        vec!("foo".to_owned(), "bar".to_owned(), "baz".to_owned())
    );
}

#[tokio::test]
async fn should_propagate_errors_and_restore_redirects() {
    let mut env = new_env_with_no_fds();

    {
        assert_eq!(env.file_desc(1), None);

        let mut future = eval_redirects_or_cmd_words(
            vec![
                RedirectOrCmdWord::Redirect(mock_redirect(RedirectAction::Open(
                    1,
                    dev_null(&mut env),
                    Permissions::Write,
                ))),
                RedirectOrCmdWord::CmdWord(mock_word_error(false)),
                RedirectOrCmdWord::CmdWord(mock_word_panic("should not run")),
            ],
            &env,
        );

        let err = EvalRedirectOrCmdWordError::CmdWord(MockErr::Fatal(false));
        assert_eq!(
            Compat01As03::new(poll_fn(|| future.poll(&mut env))).await,
            Err(err)
        );
        assert_eq!(env.file_desc(1), None);
    }

    {
        assert_eq!(env.file_desc(1), None);

        let mut future = eval_redirects_or_cmd_words(
            vec![
                RedirectOrCmdWord::Redirect(mock_redirect(RedirectAction::Open(
                    1,
                    dev_null(&mut env),
                    Permissions::Write,
                ))),
                RedirectOrCmdWord::Redirect(mock_redirect_error(false)),
                RedirectOrCmdWord::CmdWord(mock_word_panic("should not run")),
            ],
            &env,
        );

        let err = EvalRedirectOrCmdWordError::Redirect(MockErr::Fatal(false));
        assert_eq!(
            Compat01As03::new(poll_fn(|| future.poll(&mut env))).await,
            Err(err)
        );
        assert_eq!(env.file_desc(1), None);
    }
}

#[tokio::test]
async fn should_propagate_cancel_and_restore_redirects() {
    let mut env = new_env_with_no_fds();

    test_cancel!(
        eval_redirects_or_cmd_words::<MockRedirect<_>, _, _, _>(
            vec!(RedirectOrCmdWord::CmdWord(mock_word_must_cancel())),
            &env,
        ),
        env
    );

    assert_eq!(env.file_desc(1), None);
    test_cancel!(
        eval_redirects_or_cmd_words(
            vec!(
                RedirectOrCmdWord::Redirect(mock_redirect(RedirectAction::Open(
                    1,
                    dev_null(&mut env),
                    Permissions::Write
                ))),
                RedirectOrCmdWord::Redirect(mock_redirect_must_cancel()),
                RedirectOrCmdWord::CmdWord(mock_word_panic("should not run")),
            ),
            &env,
        ),
        env
    );
    assert_eq!(env.file_desc(1), None);
}