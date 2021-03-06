#![deny(rust_2018_idioms)]

use conch_runtime::io::{FileDesc, Permissions};
use std::sync::Arc;

mod support;
pub use self::support::*;

type MockRedirectOrVarAssig =
    RedirectOrVarAssig<MockRedirect<Arc<FileDesc>>, Arc<String>, MockWord>;

async fn eval(
    vars: Vec<MockRedirectOrVarAssig>,
    export_vars: Option<bool>,
    env: &mut DefaultEnvArc,
) -> Result<EnvRestorer<'_, DefaultEnvArc>, EvalRedirectOrVarAssigError<MockErr, MockErr>> {
    let mut restorer = EnvRestorer::new(env);
    eval_redirects_or_var_assignments_with_restorer(export_vars, vars.into_iter(), &mut restorer)
        .await?;
    Ok(restorer)
}

#[tokio::test]
async fn smoke() {
    let mut env = new_env_with_no_fds();

    let key = Arc::new("key".to_owned());
    let key_empty = Arc::new("key_empty".to_owned());
    let key_empty2 = Arc::new("key_empty2".to_owned());
    let key_split = Arc::new("key_split".to_owned());
    let val = "val".to_owned();

    let all_keys = vec![
        key.clone(),
        key_empty.clone(),
        key_empty2.clone(),
        key_split.clone(),
    ];

    let assert_empty_vars = |env: &DefaultEnvArc| {
        for var in &all_keys {
            assert_eq!(env.var(var), None);
        }
    };

    {
        let mut env = env.sub_env();
        eval(vec![], None, &mut env).await.unwrap();
        assert_empty_vars(&env);
    }

    assert_eq!(env.file_desc(1), None);
    assert_empty_vars(&env);

    let fdes = dev_null(&mut env);
    let future = eval(
        vec![
            RedirectOrVarAssig::Redirect(mock_redirect(RedirectAction::Open(
                1,
                fdes.clone(),
                Permissions::Write,
            ))),
            RedirectOrVarAssig::VarAssig(
                key.clone(),
                Some(mock_word_fields(Fields::Single(val.clone()))),
            ),
            RedirectOrVarAssig::VarAssig(
                key_split.clone(),
                Some(mock_word_fields(Fields::Split(vec![
                    "foo".to_owned(),
                    "bar".to_owned(),
                ]))),
            ),
            RedirectOrVarAssig::VarAssig(key_empty.clone(), None),
            RedirectOrVarAssig::VarAssig(key_empty2.clone(), Some(mock_word_fields(Fields::Zero))),
        ],
        None,
        &mut env,
    );
    let mut restorer = future.await.unwrap();

    assert_eq!(restorer.get().var(&key), Some(&Arc::new(val)));
    assert_eq!(
        restorer.get().var(&key_empty),
        Some(&Arc::new(String::new()))
    );
    assert_eq!(
        restorer.get().var(&key_empty2),
        Some(&Arc::new(String::new()))
    );
    assert_eq!(
        restorer.get().var(&key_split),
        Some(&Arc::new("foo bar".to_owned()))
    );

    restorer.restore_vars();
    assert_empty_vars(restorer.get());

    assert_eq!(
        restorer.get().file_desc(1),
        Some((&fdes, Permissions::Write))
    );
    restorer.restore_redirects();
    drop(restorer);

    assert_eq!(env.file_desc(1), None);
}

#[tokio::test]
async fn should_honor_export_vars_config() {
    let mut env = new_env_with_no_fds();

    let key = Arc::new("key".to_owned());
    let key_existing = Arc::new("key_existing".to_owned());
    let key_existing_exported = Arc::new("key_existing_exported".to_owned());

    let val_existing = Arc::new("val_existing".to_owned());
    let val_existing_exported = Arc::new("val_existing_exported".to_owned());
    let val = Arc::new("val".to_owned());
    let val_new = Arc::new("val_new".to_owned());
    let val_new_alt = Arc::new("val_new_alt".to_owned());

    env.set_exported_var(key_existing.clone(), val_existing.clone(), false);
    env.set_exported_var(
        key_existing_exported.clone(),
        val_existing_exported.clone(),
        true,
    );

    let cases = vec![
        (Some(true), true, true, true),
        (Some(false), false, false, false),
        (None, false, false, true),
    ];

    for (case, new, existing, existing_exported) in cases {
        let mut env = env.sub_env();
        let future = eval(
            vec![
                RedirectOrVarAssig::VarAssig(
                    key.clone(),
                    Some(mock_word_fields(Fields::Single((*val).clone()))),
                ),
                RedirectOrVarAssig::VarAssig(
                    key_existing.clone(),
                    Some(mock_word_fields(Fields::Single((*val_new).clone()))),
                ),
                RedirectOrVarAssig::VarAssig(
                    key_existing_exported.clone(),
                    Some(mock_word_fields(Fields::Single((*val_new_alt).clone()))),
                ),
            ],
            case,
            &mut env,
        );

        let var_restorer = future.await.unwrap();

        assert_eq!(var_restorer.get().exported_var(&key), Some((&val, new)));
        assert_eq!(
            var_restorer.get().exported_var(&key_existing),
            Some((&val_new, existing))
        );
        assert_eq!(
            var_restorer.get().exported_var(&key_existing_exported),
            Some((&val_new_alt, existing_exported))
        );
    }
}

#[tokio::test]
async fn should_propagate_errors_and_restore_redirects_and_vars() {
    let mut env = new_env_with_no_fds();

    let key = Arc::new("key".to_owned());

    {
        assert_eq!(env.file_desc(1), None);

        let future = eval(
            vec![
                RedirectOrVarAssig::Redirect(mock_redirect(RedirectAction::Open(
                    1,
                    dev_null(&mut env),
                    Permissions::Write,
                ))),
                RedirectOrVarAssig::VarAssig(
                    key.clone(),
                    Some(mock_word_fields(Fields::Single("val".to_owned()))),
                ),
                RedirectOrVarAssig::VarAssig(key.clone(), Some(mock_word_error(false))),
                RedirectOrVarAssig::VarAssig(key.clone(), Some(mock_word_panic("should not run"))),
            ],
            None,
            &mut env,
        );

        assert_eq!(
            future.await.unwrap_err(),
            EvalRedirectOrVarAssigError::VarAssig(MockErr::Fatal(false))
        );
        assert_eq!(env.file_desc(1), None);
        assert_eq!(env.var(&key), None);
    }

    {
        assert_eq!(env.file_desc(1), None);

        let future = eval(
            vec![
                RedirectOrVarAssig::Redirect(mock_redirect(RedirectAction::Open(
                    1,
                    dev_null(&mut env),
                    Permissions::Write,
                ))),
                RedirectOrVarAssig::VarAssig(
                    key.clone(),
                    Some(mock_word_fields(Fields::Single("val".to_owned()))),
                ),
                RedirectOrVarAssig::Redirect(mock_redirect_error(false)),
                RedirectOrVarAssig::VarAssig(key.clone(), Some(mock_word_panic("should not run"))),
            ],
            None,
            &mut env,
        );

        assert_eq!(
            future.await.unwrap_err(),
            EvalRedirectOrVarAssigError::Redirect(MockErr::Fatal(false))
        );
        assert_eq!(env.file_desc(1), None);
        assert_eq!(env.var(&key), None);
    }
}
