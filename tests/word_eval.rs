extern crate conch_runtime;
extern crate futures;

use conch_runtime::new_eval::{Fields, TildeExpansion, WordEval, WordEvalConfig};
use conch_runtime::future::{Async, EnvFuture, Poll};

#[macro_use]
mod support;
pub use self::support::*;

#[derive(Debug, Clone)]
struct MockWordCfg {
    cfg: WordEvalConfig,
    fields: Fields<String>,
}

impl<E: ?Sized> WordEval<E> for MockWordCfg {
    type EvalResult = String;
    type Error = ();
    type EvalFuture = Self;

    fn eval_with_config(self, _: &mut E, cfg: WordEvalConfig) -> Self::EvalFuture {
        assert_eq!(cfg, self.cfg);
        self
    }
}

impl<E: ?Sized> EnvFuture<E> for MockWordCfg {
    type Item = Fields<String>;
    type Error = ();

    fn poll(&mut self, _: &mut E) -> Poll<Self::Item, Self::Error> {
        Ok(Async::Ready(self.fields.clone()))
    }

    fn cancel(&mut self, _: &mut E) {
        unimplemented!()
    }
}

#[test]
fn test_eval_expands_first_tilde_and_splits_words() {
    let word = MockWordCfg {
        cfg: WordEvalConfig {
            tilde_expansion: TildeExpansion::First,
            split_fields_further: true,
        },
        fields: Fields::Zero,
    };

    let mut env = ();
    assert_eq!(word.eval(&mut env).pin_env(env).wait(), Ok(Fields::Zero));
}

#[test]
fn test_eval_as_assignment_expands_all_tilde_and_does_not_split_words() {
    use conch_runtime::env::{VariableEnvironment, VarEnv};

    let cfg = WordEvalConfig {
        tilde_expansion: TildeExpansion::All,
        split_fields_further: false,
    };

    let mut env = VarEnv::new();
    env.set_var("IFS".to_owned(), "!".to_owned());

    {
        let word = MockWordCfg { cfg: cfg, fields: Fields::Zero };
        let mut env = env.clone();
        assert_eq!(word.eval_as_assignment(&mut env).pin_env(env).wait(), Ok("".to_owned()));
    }

    {
        let msg = "foo".to_owned();
        let word = MockWordCfg { cfg: cfg, fields: Fields::Single(msg.clone()) };
        let mut env = env.clone();
        assert_eq!(word.eval_as_assignment(&mut env).pin_env(env).wait(), Ok(msg));
    }

    {
        let word = MockWordCfg {
            cfg: cfg,
            fields: Fields::At(vec!(
                "foo".to_owned(),
                "bar".to_owned(),
            )),
        };

        let mut env = env.clone();
        assert_eq!(word.eval_as_assignment(&mut env).pin_env(env).wait(), Ok("foo bar".to_owned()));
    }

    {
        let word = MockWordCfg {
            cfg: cfg,
            fields: Fields::Split(vec!(
                "foo".to_owned(),
                "bar".to_owned(),
            )),
        };

        let mut env = env.clone();
        assert_eq!(word.eval_as_assignment(&mut env).pin_env(env).wait(), Ok("foo bar".to_owned()));
    }

    {
        let word = MockWordCfg {
            cfg: cfg,
            fields: Fields::Star(vec!(
                "foo".to_owned(),
                "bar".to_owned(),
            )),
        };

        let mut env = env.clone();
        assert_eq!(word.eval_as_assignment(&mut env).pin_env(env).wait(), Ok("foo!bar".to_owned()));
    }
}

#[test]
fn test_eval_as_pattern_expands_first_tilde_and_does_not_split_words_and_joins_fields() {
    let word = MockWordCfg {
        cfg: WordEvalConfig {
            tilde_expansion: TildeExpansion::First,
            split_fields_further: false,
        },
        fields: Fields::Split(vec!(
            "foo".to_owned(),
            "*?".to_owned(),
            "bar".to_owned(),
        )),
    };

    let mut env = ();
    let pat = word.eval_as_pattern(&mut env).pin_env(env).wait().unwrap();
    assert_eq!(pat.as_str(), "foo [*][?] bar"); // FIXME: update once patterns implemented
    //assert_eq!(pat.as_str(), "foo *? bar");
}

#[test]
fn test_assignment_cancel() {
    use conch_runtime::env::VarEnv;

    let mut env = VarEnv::<String, String>::new();
    let future = mock_word_must_cancel().eval_as_assignment(&mut env);
    test_cancel!(future, env);
}

#[test]
fn test_pattern_cancel() {
    let mut env = ();
    let future = mock_word_must_cancel().eval_as_pattern(&mut env);
    test_cancel!(future, env);
}
