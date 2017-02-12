//! A module which defines evaluating any kind of word.

use io::FileDescWrapper;
use runtime::{HOME, Result, Run};
use runtime::env::{ArgumentsEnvironment, FileDescEnvironment, FunctionExecutorEnvironment,
                   IsInteractiveEnvironment, LastStatusEnvironment,
                   StringWrapper, SubEnvironment, VariableEnvironment};
use runtime::eval::{Fields, ParamEval, TildeExpansion, WordEval, WordEvalConfig};
use std::borrow::Borrow;
use std::convert::{From, Into};
use std::iter::{IntoIterator, Iterator};
use std::rc::Rc;
use syntax::ast::{ComplexWord, SimpleWord, TopLevelWord, Word};

impl<T, P, S, E: ?Sized> WordEval<E> for SimpleWord<T, P, S>
    where T: StringWrapper,
          P: ParamEval<E, EvalResult = T>,
          S: WordEval<E, EvalResult = T>,
          E: VariableEnvironment<Var = T>,
          E::VarName: Borrow<String>,
{
    type EvalResult = T;

    fn eval_with_config(&self, env: &mut E, cfg: WordEvalConfig) -> Result<Fields<Self::EvalResult>>
    {
        let ret = match *self {
            SimpleWord::Literal(ref s) |
            SimpleWord::Escaped(ref s) => Fields::Single(s.clone()),

            SimpleWord::Star        => Fields::Single(String::from("*").into()),
            SimpleWord::Question    => Fields::Single(String::from("?").into()),
            SimpleWord::SquareOpen  => Fields::Single(String::from("[").into()),
            SimpleWord::SquareClose => Fields::Single(String::from("]").into()),
            SimpleWord::Colon       => Fields::Single(String::from(":").into()),

            SimpleWord::Tilde => match cfg.tilde_expansion {
                TildeExpansion::None => Fields::Single(String::from("~").into()),
                TildeExpansion::All |
                TildeExpansion::First => {
                    // Note: even though we are expanding the equivalent of `$HOME`, a tilde
                    // expansion is NOT considered a parameter expansion, and therefore
                    // should not be subjected to field splitting.
                    env.var(&HOME).map_or(Fields::Zero, |f| Fields::Single(f.clone()))
                },
            },

            SimpleWord::Subst(ref s) => try!(s.eval_with_config(env, cfg)),
            SimpleWord::Param(ref p) => p.eval(cfg.split_fields_further, env).unwrap_or(Fields::Zero),
        };

        Ok(ret)
    }
}

impl<T, W, E: ?Sized> WordEval<E> for Word<T, W>
    where T: StringWrapper,
          W: WordEval<E, EvalResult = T>,
          E: VariableEnvironment<Var = T>,
          E::VarName: Borrow<String>,
{
    type EvalResult = T;

    fn eval_with_config(&self, env: &mut E, cfg: WordEvalConfig) -> Result<Fields<Self::EvalResult>>
    {
        let ret = match *self {
            Word::Simple(ref s) => try!(s.eval_with_config(env, cfg)),
            Word::SingleQuoted(ref s) => Fields::Single(s.clone().into()),
            Word::DoubleQuoted(ref v) => {
                // Make sure we are NOT doing any tilde expanions for further field splitting
                let cfg = WordEvalConfig {
                    tilde_expansion: TildeExpansion::None,
                    split_fields_further: false,
                };

                let mut fields = Vec::new();
                let mut cur_field: Option<String> = None;

                macro_rules! append_to_cur_field {
                    ($wrapper:expr) => {
                        match cur_field {
                            Some(ref mut cur_field) => cur_field.push_str($wrapper.as_str()),
                            None => cur_field = Some($wrapper.into_owned()),
                        }
                    }
                };

                for w in v.iter() {
                    match try!(w.eval_with_config(env, cfg)) {
                        Fields::Zero => continue,
                        Fields::Single(s) => append_to_cur_field!(s),

                        // Since we should have indicated we do NOT want field splitting,
                        // we should never encounter a Split variant, however, since we
                        // cannot control external implementations, we'll fallback
                        // somewhat gracefully rather than panicking.
                        f@Fields::Split(_) |
                        f@Fields::Star(_) => append_to_cur_field!(f.join_with_ifs(env)),

                        // Any fields generated by $@ must be maintained, however, the first and last
                        // fields of $@ should be concatenated to whatever comes before/after them.
                        Fields::At(v) => {
                            // According to the POSIX spec, if $@ is empty it should generate NO fields
                            // even when within double quotes.
                            if !v.is_empty() {
                                let mut iter = v.into_iter();
                                if let Some(first) = iter.next() {
                                    append_to_cur_field!(first);
                                }

                                cur_field.take().map(|s| fields.push(s.into()));

                                let mut last = None;
                                for next in iter {
                                    fields.extend(last.take());
                                    last = Some(next);
                                }

                                last.map(|rc| append_to_cur_field!(rc));
                            }
                        },
                    }
                }

                cur_field.map(|s| fields.push(s.into()));
                fields.into()
            }
        };

        Ok(ret)
    }
}

impl<W, E: ?Sized> WordEval<E> for ComplexWord<W>
    where W: WordEval<E>,
{
    type EvalResult = W::EvalResult;

    fn eval_with_config(&self, env: &mut E, cfg: WordEvalConfig) -> Result<Fields<Self::EvalResult>>
    {
        let ret = match *self {
            ComplexWord::Single(ref w) => try!(w.eval_with_config(env, cfg)),

            ComplexWord::Concat(ref v) => {
                let cfg = WordEvalConfig {
                    tilde_expansion: TildeExpansion::None,
                    split_fields_further: cfg.split_fields_further,
                };

                let mut fields: Vec<W::EvalResult> = Vec::new();
                for w in v.iter() {
                    let mut iter = try!(w.eval_with_config(env, cfg)).into_iter();
                    match (fields.pop(), iter.next()) {
                       (Some(last), Some(next)) => {
                           let mut new = last.into_owned();
                           new.push_str(next.as_str());
                           fields.push(new.into());
                       },
                       (Some(last), None) => fields.push(last),
                       (None, Some(next)) => fields.push(next),
                       (None, None)       => continue,
                    }

                    fields.extend(iter);
                }

                fields.into()
            },
        };

        Ok(ret)
    }
}

impl<T, E> WordEval<E> for TopLevelWord<T>
    where T: 'static + StringWrapper + ::std::fmt::Display,
          E: ArgumentsEnvironment<Arg = T>
            + FileDescEnvironment
            + FunctionExecutorEnvironment<FnName = T>
            + IsInteractiveEnvironment
            + LastStatusEnvironment
            + SubEnvironment
            + VariableEnvironment<VarName = T, Var = T>,
          E::FileHandle: FileDescWrapper,
          E::Fn: From<Rc<Run<E>>>,
{
    type EvalResult = T;

    fn eval_with_config(&self, env: &mut E, cfg: WordEvalConfig) -> Result<Fields<Self::EvalResult>>
    {
        self.0.eval_with_config(env, cfg)
    }
}

#[cfg(test)]
mod tests {
    use error::RuntimeError;
    use error::ExpansionError::DivideByZero;
    use runtime::Result;
    use runtime::env::{ArgsEnv, Env, VariableEnvironment};
    use runtime::eval::{Fields, TildeExpansion, WordEval, WordEvalConfig};
    use runtime::tests::{DefaultEnv, DefaultEnvConfig};
    use syntax::ast::{Parameter, ParameterSubstitution, TopLevelWord};
    use syntax::ast::{DefaultComplexWord, DefaultWord, DefaultSimpleWord};
    use syntax::ast::ComplexWord::*;
    use syntax::ast::SimpleWord::*;
    use syntax::ast::Word::*;

    fn lit(s: &str) -> DefaultWord {
        Simple(Literal(String::from(s)))
    }

    #[derive(Copy, Clone, Debug)]
    struct MockCmd;
    impl<E: ?Sized> ::runtime::Run<E> for MockCmd {
        fn run(&self, _: &mut E) -> Result<::runtime::ExitStatus> {
            Ok(::runtime::EXIT_SUCCESS)
        }
    }

    #[test]
    fn test_simple_word_literal_eval() {
        // Should have no effect
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::<String>::new_test_env();
        let value = "foobar".to_owned();
        let simple: DefaultSimpleWord = Literal(value.clone());
        assert_eq!(simple.eval_with_config(&mut env, cfg), Ok(Fields::Single(value)));
    }

    #[test]
    fn test_simple_word_escaped_eval() {
        // Should have no effect
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::new_test_env();
        let value = "&& $@".to_owned();
        let simple: DefaultSimpleWord = Literal(value.clone());
        assert_eq!(simple.eval_with_config(&mut env, cfg), Ok(Fields::Single(value)));
    }

    #[test]
    fn test_simple_word_special_literals_eval_properly() {
        // Should have no effect
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let cases: Vec<(DefaultSimpleWord, &'static str)> = vec!(
            (Star,        "*"),
            (Question,    "?"),
            (SquareOpen,  "["),
            (SquareClose, "]"),
            (Colon,       ":"),
        );

        let mut env = DefaultEnv::new_test_env();

        for (word, correct) in cases {
            let correct = Ok(Fields::Single(correct.to_owned()));
            assert_eq!(word.eval_with_config(&mut env, cfg), correct);
        }
    }

    #[test]
    fn test_word_lone_tilde_expansion() {
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::First,
            split_fields_further: true,
        };

        let home_value = "foo bar".to_owned();
        let mut env = DefaultEnv::new_test_env();
        env.set_var("HOME".to_owned(), home_value.clone());

        let word: DefaultWord = Simple(Tilde);
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(Fields::Single(home_value)));
    }

    #[test]
    fn test_simple_word_subst() {
        use syntax::ast::ParameterSubstitution;

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: false,
        };

        let var_name = "var".to_owned();
        let var_value = "foo".to_owned();

        let mut env = DefaultEnv::new_test_env();
        env.set_var(var_name.clone(), var_value.clone());

        let simple: DefaultSimpleWord =
            Subst(Box::new(ParameterSubstitution::Len(Parameter::Var(var_name))));
        let correct = Fields::Single("3".to_owned());
        assert_eq!(simple.eval_with_config(&mut env, cfg), Ok(correct));
    }

    #[test]
    fn test_simple_word_subst_error() {
        use runtime::RuntimeError;
        use syntax::ast::{Arithmetic, ParameterSubstitution};

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: false,
        };

        let var_name = "var".to_owned();
        let var_value = "foo".to_owned();

        let mut env = DefaultEnv::new_test_env();
        env.set_var(var_name.clone(), var_value.clone());

        let simple: DefaultSimpleWord = Subst(Box::new(ParameterSubstitution::Arith(Some(Arithmetic::Div(
            Box::new(Arithmetic::Literal(1)),
            Box::new(Arithmetic::Literal(0))
        )))));
        let correct = RuntimeError::Expansion(DivideByZero);
        assert_eq!(simple.eval_with_config(&mut env, cfg), Err(correct));
    }

    #[test]
    fn test_simple_word_param() {
        // Should have no effect
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let var_name = "var".to_owned();
        let var_value = "~/foo".to_owned();

        let mut env = DefaultEnv::new_test_env();
        env.set_var(var_name.clone(), var_value.clone());

        let simple: DefaultSimpleWord = Param(Parameter::Var(var_name));
        assert_eq!(simple.eval_with_config(&mut env, cfg), Ok(Fields::Single(var_value)));
    }

    #[test]
    fn test_simple_word_param_unset() {
        // Should have no effect
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::<String>::new_test_env();
        let simple: DefaultSimpleWord = Param(Parameter::Var("var".to_owned()));
        assert_eq!(simple.eval_with_config(&mut env, cfg), Ok(Fields::Zero));
    }

    #[test]
    fn test_simple_word_param_splitting() {
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All, // Should have no effect
            split_fields_further: true, // Should have effect
        };

        let var_name = "var".to_owned();
        let var_value = "~ foo".to_owned();

        let mut env = DefaultEnv::new_test_env();
        env.set_var(var_name.clone(), var_value);

        let simple: DefaultSimpleWord = Param(Parameter::Var(var_name));
        let correct = Fields::Split(vec!("~".to_owned(), "foo".to_owned()));
        assert_eq!(simple.eval_with_config(&mut env, cfg), Ok(correct));
    }

    #[test]
    fn test_word_simple() {
        use syntax::ast::Arithmetic;

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::new_test_env();
        let value = "foo".to_owned();
        let word = lit(&value);
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(Fields::Single(value)));

        let word: DefaultWord = Simple(Subst(Box::new(ParameterSubstitution::Arith(Some(
            Arithmetic::Div(
                Box::new(Arithmetic::Literal(1)),
                Box::new(Arithmetic::Literal(0))
            )
        )))));
        assert_eq!(word.eval_with_config(&mut env, cfg), Err(RuntimeError::Expansion(DivideByZero)));
    }

    #[test]
    fn test_word_single_quoted_should_not_split_fields_or_expand_anything() {
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::new_test_env();
        let value = "~/hello world\nfoo\tbar *".to_owned();
        let word: DefaultWord = SingleQuoted(value.clone());
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(Fields::Single(value)));
    }

    #[test]
    fn test_word_double_quoted_does_parameter_expansions_as_single_field() {
        // Should have no effect
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let var = "var".to_owned();
        let mut env = DefaultEnv::new_test_env();
        env.set_var(var.clone(), "hello world".to_owned());

        let word: DefaultWord = DoubleQuoted(vec!(
            Literal("foo".to_owned()),
            Param(Parameter::Var(var)),
            Literal("bar".to_owned()),
        ));
        let correct = Fields::Single("foohello worldbar".to_owned());
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(correct));
    }

    #[test]
    fn test_word_double_quoted_does_not_expand_tilde() {
        // Should have no effect
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::new_test_env();
        let word: DefaultWord = DoubleQuoted(vec!(Tilde));
        let correct = Fields::Single("~".to_owned());
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(correct));

        let word: DefaultWord = DoubleQuoted(vec!(Tilde, Literal("root".to_owned())));
        let correct = Fields::Single("~root".to_owned());
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(correct));

        let word: DefaultWord = DoubleQuoted(vec!(Tilde, Literal("/root".to_owned())));
        let correct = Fields::Single("~/root".to_owned());
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(correct));
    }

    #[test]
    fn test_word_double_quoted_param_star_unset_results_in_no_fields() {
        // Should have no effect
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::<String>::new_test_env();
        let word: DefaultWord = DoubleQuoted(vec!(Param(Parameter::Star)));
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(Fields::Zero));
    }

    #[test]
    fn test_word_double_quoted_param_at_expands_when_args_set_and_concats_with_rest() {
        // Should have no effect
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = Env::with_config(DefaultEnvConfig {
            args_env: ArgsEnv::with_name_and_args("shell".to_owned(), vec!(
                "one".to_owned(),
                "two".to_owned(),
                "three".to_owned(),
            )),
            .. Default::default()
        });

        let word: DefaultWord = DoubleQuoted(vec!(
            Literal("foo".to_owned()),
            Param(Parameter::At),
            Literal("bar".to_owned()),
        ));

        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(Fields::Split(vec!(
            "fooone".to_owned(),
            "two".to_owned(),
            "threebar".to_owned(),
        ))));
    }

    #[test]
    fn test_word_double_quoted_param_at_expands_to_nothing_when_args_not_set_and_concats_with_rest() {
        // Should have no effect
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::new_test_env();
        let word: DefaultWord = DoubleQuoted(vec!(Param(Parameter::At)));
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(Fields::Zero));

        let word: DefaultWord = DoubleQuoted(vec!(
            Literal("foo".to_owned()),
            Param(Parameter::At),
            Literal("bar".to_owned()),
        ));
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(Fields::Single("foobar".to_owned())));
    }

    #[test]
    fn test_word_double_quoted_param_star_expands_but_joined_by_ifs() {
        use runtime::env::UnsetVariableEnvironment;

        // Should have no effect
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = Env::with_config(DefaultEnvConfig {
            args_env: ArgsEnv::with_name_and_args("shell".to_owned(), vec!(
                "one".to_owned(),
                "two".to_owned(),
                "three".to_owned(),
            )),
            .. Default::default()
        });

        let word: DefaultWord = DoubleQuoted(vec!(
            Literal("foo".to_owned()),
            Param(Parameter::Star),
            Literal("bar".to_owned()),
        ));

        // IFS initialized by environment for us
        let correct = Fields::Single("fooone two threebar".to_owned());
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(correct));

        env.set_var("IFS".to_owned(), "!".to_owned());
        let correct = Fields::Single("fooone!two!threebar".to_owned());
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(correct));

        env.set_var("IFS".to_owned(), "".to_owned());
        let correct = Fields::Single("fooonetwothreebar".to_owned());
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(correct));

        env.unset_var("IFS");
        let correct = Fields::Single("fooone two threebar".to_owned());
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(correct));
    }

    #[test]
    fn test_word_double_quoted_param_at_zero_fields_if_no_args() {
        // Should have no effect
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::<String>::new_test_env();
        let word: DefaultWord = DoubleQuoted(vec!(Param(Parameter::At)));
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(Fields::Zero));
    }

    #[test]
    fn test_word_double_quoted_no_field_splitting() {
        // Should have no effect
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::new_test_env();
        env.set_var("var".to_owned(), "foo bar".to_owned());

        let var = Parameter::Var("var".to_owned());

        let word: DefaultWord = DoubleQuoted(vec!(Param(var.clone())));
        let correct = Fields::Single("foo bar".to_owned());
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(correct));

        let word: DefaultWord = DoubleQuoted(vec!(
            Subst(Box::new(ParameterSubstitution::Default(false, var, None)))
        ));
        let correct = Fields::Single("foo bar".to_owned());
        assert_eq!(word.eval_with_config(&mut env, cfg), Ok(correct));
    }

    #[test]
    fn test_complex_word_single() {
        use syntax::ast::Arithmetic;

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::new_test_env();
        let value = "foo".to_owned();
        let complex: DefaultComplexWord = Single(Simple(Literal(value.clone())));
        assert_eq!(complex.eval_with_config(&mut env, cfg), Ok(Fields::Single(value)));

        let complex: DefaultComplexWord = Single(Simple(Subst(Box::new(ParameterSubstitution::Arith(
            Some(Arithmetic::Div(
                Box::new(Arithmetic::Literal(1)),
                Box::new(Arithmetic::Literal(0))
            ))
        )))));
        assert_eq!(complex.eval_with_config(&mut env, cfg), Err(RuntimeError::Expansion(DivideByZero)));
    }

    #[test]
    fn test_complex_word_concat_error() {
        use syntax::ast::Arithmetic;

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::new_test_env();
        let value = "foo".to_owned();
        let complex: DefaultComplexWord = Single(Simple(Literal(value.clone())));
        assert_eq!(complex.eval_with_config(&mut env, cfg), Ok(Fields::Single(value)));

        let complex: DefaultComplexWord = Concat(vec!(
            Simple(Subst(Box::new(ParameterSubstitution::Arith(
                Some(Arithmetic::Div(
                    Box::new(Arithmetic::Literal(1)),
                    Box::new(Arithmetic::Literal(0))
                ))
            ))))
        ));
        assert_eq!(complex.eval_with_config(&mut env, cfg), Err(RuntimeError::Expansion(DivideByZero)));
    }

    #[test]
    fn test_complex_word_concat_joins_all_inner_words() {
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::new_test_env();
        env.set_var("var".to_owned(), "foobar".to_owned());

        let complex: DefaultComplexWord = Concat(vec!(lit("hello")));
        let correct = Fields::Single("hello".to_owned());
        assert_eq!(complex.eval_with_config(&mut env, cfg), Ok(correct));

        let complex: DefaultComplexWord = Concat(vec!(
            lit("hello"),
            Simple(Param(Parameter::Var("var".to_owned()))),
            lit("world"),
        ));
        let correct = Fields::Single("hellofoobarworld".to_owned());
        assert_eq!(complex.eval_with_config(&mut env, cfg), Ok(correct));
    }

    #[test]
    fn test_complex_word_concat_expands_to_many_fields_and_joins_with_those_before_and_after() {
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::new_test_env();
        env.set_var("var".to_owned(), "foo bar baz".to_owned());

        let complex: DefaultComplexWord = Concat(vec!(
            lit("hello"),
            Simple(Param(Parameter::Var("var".to_owned()))),
            lit("world"),
        ));

        assert_eq!(complex.eval_with_config(&mut env, cfg), Ok(Fields::Split(vec!(
            "hellofoo".to_owned(),
            "bar".to_owned(),
            "bazworld".to_owned(),
        ))));
    }

    #[test]
    fn test_complex_word_concat_should_not_expand_tilde_which_is_not_at_start() {
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All, // should have no effect
            split_fields_further: true,
        };

        let mut env = DefaultEnv::new_test_env();
        let complex: DefaultComplexWord = Concat(vec!(
            lit("foo"),
            Simple(Tilde),
            lit("bar"),
        ));
        let correct = Fields::Single("foo~bar".to_owned());
        assert_eq!(complex.eval_with_config(&mut env, cfg), Ok(correct));

        let complex: DefaultComplexWord = Concat(vec!(
            lit("foo"),
            Simple(Tilde),
        ));
        let correct = Fields::Single("foo~".to_owned());
        assert_eq!(complex.eval_with_config(&mut env, cfg), Ok(correct));
    }

    #[test]
    fn test_complex_word_concat_empty_words_results_in_zero_field() {
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All, // should have no effect
            split_fields_further: true,
        };

        let mut env = DefaultEnv::<String>::new_test_env();
        let complex: DefaultComplexWord = Concat(vec!());
        assert_eq!(complex.eval_with_config(&mut env, cfg), Ok(Fields::Zero));

        let var = Simple(Param(Parameter::Var("var".to_owned())));

        let complex: DefaultComplexWord = Concat(vec!(var.clone()));
        assert_eq!(complex.eval_with_config(&mut env, cfg), Ok(Fields::Zero));

        let complex: DefaultComplexWord = Concat(vec!(var.clone(), var.clone()));
        assert_eq!(complex.eval_with_config(&mut env, cfg), Ok(Fields::Zero));
    }

    #[test]
    fn test_complex_word_concat_param_at_expands_when_args_set() {
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All, // should have no effect
            split_fields_further: true,
        };

        let mut env = Env::with_config(DefaultEnvConfig {
            args_env: ArgsEnv::with_name_and_args("shell".to_owned(), vec!(
                "one".to_owned(),
                "two".to_owned(),
                "three four".to_owned(),
            )),
            .. Default::default()
        });

        let complex: DefaultComplexWord = Concat(vec!(Simple(Param(Parameter::At))));
        assert_eq!(complex.eval_with_config(&mut env, cfg), Ok(Fields::Split(vec!(
            "one".to_owned(),
            "two".to_owned(),
            "three".to_owned(),
            "four".to_owned(),
        ))));
    }

    #[test]
    fn test_complex_word_concat_param_at_expands_when_args_set_and_concats_with_rest() {
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All, // should have no effect
            split_fields_further: true,
        };

        let mut env = Env::with_config(DefaultEnvConfig {
            args_env: ArgsEnv::with_name_and_args("shell".to_owned(), vec!(
                "one".to_owned(),
                "two".to_owned(),
                "three four".to_owned(),
            )),
            .. Default::default()
        });

        let complex: DefaultComplexWord = Concat(vec!(
            lit("foo"),
            Simple(Param(Parameter::At)),
            lit("bar"),
        ));
        assert_eq!(complex.eval_with_config(&mut env, cfg), Ok(Fields::Split(vec!(
            "fooone".to_owned(),
            "two".to_owned(),
            "three".to_owned(),
            "fourbar".to_owned(),
        ))));
    }

    #[test]
    fn test_complex_word_concat_param_at_expands_to_nothing_when_args_not_set_and_concats_with_rest() {
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All, // should have no effect
            split_fields_further: true,
        };

        let mut env = DefaultEnv::new_test_env();
        let complex: DefaultComplexWord = Concat(vec!(
            lit("foo"),
            Simple(Param(Parameter::At)),
            lit("bar"),
        ));

        let correct = Fields::Single("foobar".to_owned());
        assert_eq!(complex.eval_with_config(&mut env, cfg), Ok(correct));
    }

    #[test]
    fn test_complex_word_tilde_in_middle_of_word_after_colon_does_not_expand() {
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All, // should have no effect
            split_fields_further: true,
        };

        let mut env = DefaultEnv::new_test_env();
        let complex: DefaultComplexWord = Concat(vec!(
            lit("foo"),
            Simple(Colon),
            Simple(Tilde),
            lit("bar"),
        ));

        let correct = Fields::Single("foo:~bar".to_owned());
        assert_eq!(complex.eval_with_config(&mut env, cfg), Ok(correct));
    }

    #[test]
    fn test_top_level_word() {
        use syntax::ast::Arithmetic;

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };

        let mut env = DefaultEnv::new_test_env();
        let value = "foo".to_owned();
        let top_level_word = TopLevelWord(Single(Simple(Literal(value.clone()))));
        assert_eq!(top_level_word.eval_with_config(&mut env, cfg), Ok(Fields::Single(value)));

        let top_level_word = TopLevelWord(Single(Simple(Subst(Box::new(
            ParameterSubstitution::Arith(Some(Arithmetic::Div(
                Box::new(Arithmetic::Literal(1)),
                Box::new(Arithmetic::Literal(0))
            )))
        )))));
        assert_eq!(top_level_word.eval_with_config(&mut env, cfg), Err(RuntimeError::Expansion(DivideByZero)));
    }
}
