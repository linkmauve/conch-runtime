#![deny(rust_2018_idioms)]

use conch_parser::ast;
use conch_parser::ast::ComplexWord::*;

mod support;
pub use self::support::*;

type ComplexWord = ast::ComplexWord<MockWord>;

async fn assert_eval_equals_single<T: Into<String>>(complex: ComplexWord, expected: T) {
    assert_eval_equals_fields(complex, Fields::Single(expected.into())).await;
}

async fn assert_eval_equals_fields(complex: ComplexWord, fields: Fields<String>) {
    let cfg = WordEvalConfig {
        tilde_expansion: TildeExpansion::All,
        split_fields_further: true,
    };

    let mut env = VarEnv::<String, String>::new();
    let future = complex
        .eval_with_config(&mut env, cfg)
        .await
        .expect("eval failed");
    drop(env);

    assert_eq!(fields, future.await);
}

#[tokio::test]
async fn test_single() {
    let fields = Fields::Single("foo bar".to_owned());
    assert_eval_equals_fields(Single(mock_word_fields(fields.clone())), fields).await;

    let cfg = WordEvalConfig {
        tilde_expansion: TildeExpansion::All,
        split_fields_further: true,
    };

    let mut env = VarEnv::<String, String>::new();
    assert_eq!(
        Some(MockErr::Fatal(false)),
        Single(mock_word_error(false))
            .eval_with_config(&mut env, cfg)
            .await
            .err()
    );
}

#[tokio::test]
async fn test_concat_error() {
    let concat = Concat(vec![
        mock_word_error(false),
        mock_word_fields(Fields::Single("foo".to_owned())),
    ]);

    let cfg = WordEvalConfig {
        tilde_expansion: TildeExpansion::All,
        split_fields_further: true,
    };
    let mut env = VarEnv::<String, String>::new();
    assert_eq!(
        Some(MockErr::Fatal(false)),
        concat.eval_with_config(&mut env, cfg).await.err()
    );
}

#[tokio::test]
async fn test_concat_joins_all_inner_words() {
    let concat = Concat(vec![mock_word_fields(Fields::Single("hello".to_owned()))]);
    assert_eval_equals_single(concat, "hello").await;

    let concat = Concat(vec![
        mock_word_fields(Fields::Single("hello".to_owned())),
        mock_word_fields(Fields::Single("foobar".to_owned())),
        mock_word_fields(Fields::Single("world".to_owned())),
    ]);

    assert_eval_equals_single(concat, "hellofoobarworld").await;
}

#[tokio::test]
async fn test_concat_expands_to_many_fields_and_joins_with_those_before_and_after() {
    let concat = Concat(vec![
        mock_word_fields(Fields::Single("hello".to_owned())),
        mock_word_fields(Fields::Split(vec![
            "foo".to_owned(),
            "bar".to_owned(),
            "baz".to_owned(),
        ])),
        mock_word_fields(Fields::Star(vec!["qux".to_owned(), "quux".to_owned()])),
        mock_word_fields(Fields::Single("world".to_owned())),
    ]);

    assert_eval_equals_fields(
        concat,
        Fields::Split(vec![
            "hellofoo".to_owned(),
            "bar".to_owned(),
            "bazqux".to_owned(),
            "quuxworld".to_owned(),
        ]),
    )
    .await;
}

#[tokio::test]
async fn test_concat_should_not_expand_tilde_which_is_not_at_start() {
    let concat = Concat(vec![
        mock_word_assert_cfg(WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        }),
        mock_word_fields(Fields::Single("foo".to_owned())),
        mock_word_assert_cfg(WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: true,
        }),
        mock_word_fields(Fields::Single("bar".to_owned())),
    ]);
    assert_eval_equals_single(concat, "foobar").await;
}

// FIXME: test_concat_should_expand_tilde_after_colon

#[tokio::test]
async fn test_concat_empty_words_results_in_zero_field() {
    assert_eval_equals_fields(Concat(vec![]), Fields::Zero).await;

    let concat = Concat(vec![
        mock_word_fields(Fields::Zero),
        mock_word_fields(Fields::Zero),
        mock_word_fields(Fields::Zero),
    ]);
    assert_eval_equals_fields(concat, Fields::Zero).await;
}

#[tokio::test]
async fn test_concat_param_at_expands_when_args_set_and_concats_with_rest() {
    let concat = Concat(vec![
        mock_word_fields(Fields::Single("foo".to_owned())),
        mock_word_fields(Fields::At(vec![
            "one".to_owned(),
            "two".to_owned(),
            "three four".to_owned(),
        ])),
        mock_word_fields(Fields::Single("bar".to_owned())),
    ]);

    assert_eval_equals_fields(
        concat,
        Fields::Split(vec![
            "fooone".to_owned(),
            "two".to_owned(),
            "three fourbar".to_owned(),
        ]),
    )
    .await;
}

#[tokio::test]
async fn test_concat_param_at_expands_to_nothing_when_args_not_set_and_concats_with_rest() {
    let concat = Concat(vec![
        mock_word_fields(Fields::Single("foo".to_owned())),
        mock_word_fields(Fields::At(vec![])),
        mock_word_fields(Fields::Single("bar".to_owned())),
    ]);
    assert_eval_equals_single(concat, "foobar").await;
}
