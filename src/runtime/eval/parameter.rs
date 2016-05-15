//! A module that defines evaluating parameters and parameter subsitutions.

use glob;
use libc;

use runtime::{Environment, ExpansionError, ExitStatus, EXIT_ERROR, Result, Run, RuntimeError};
use runtime::eval::{Fields, TildeExpansion, WordEval, WordEvalConfig};
use std::io;
use std::rc::Rc;
use std::result;
use syntax::ast::{Arithmetic, Parameter, ParameterSubstitution};

const EXIT_SIGNAL_OFFSET: u32 = 128;
const IFS_DEFAULT: &'static str = " \t\n";

impl Parameter {
    /// Evaluates a parameter in the context of some environment,
    /// optionally splitting fields.
    ///
    /// A `None` value indicates that the parameter is unset.
    pub fn eval<E: Environment>(&self, split_fields_further: bool, env: &E) -> Option<Fields> {
        let get_args = || {
            let args = env.args();
            if args.is_empty() {
                None
            } else {
                Some(args.iter().cloned().collect())
            }
        };

        let ret = match *self {
            Parameter::At   => Some(get_args().map_or(Fields::Zero, Fields::At)),
            Parameter::Star => Some(get_args().map_or(Fields::Zero, Fields::Star)),

            Parameter::Pound  => Some(Fields::Single(Rc::new(env.args_len().to_string()))),
            Parameter::Dollar => Some(Fields::Single(Rc::new(unsafe { libc::getpid() }.to_string()))),
            Parameter::Dash   |        // FIXME: implement properly
            Parameter::Bang   => None, // FIXME: eventual job control would be nice

            Parameter::Question => Some(Fields::Single(Rc::new(match env.last_status() {
                ExitStatus::Code(c)   => c as u32,
                ExitStatus::Signal(c) => c as u32 + EXIT_SIGNAL_OFFSET,
            }.to_string()))),

            Parameter::Positional(0) => Some(Fields::Single(env.name().clone())),
            Parameter::Positional(p) => env.arg(p as usize).cloned().map(Fields::Single),
            Parameter::Var(ref var)  => env.var(var).cloned().map(Fields::Single),
        };

        ret.map(|f| {
            if split_fields_further {
                split_fields(f, env)
            } else {
                f
            }
        })
    }
}

impl<W, C> ParameterSubstitution<W, C> {
    /// Evaluates a parameter subsitution in the context of some environment,
    /// optionally splitting fields.
    ///
    /// Note: even if the caller specifies no splitting should be done,
    /// multiple fields can occur if `$@` or `$*` is evaluated.
    pub fn eval<E>(&self, env: &mut E, cfg: WordEvalConfig) -> Result<Fields>
        where E: Environment,
              W: WordEval<E>,
              C: Run<E>,
    {
        self.eval_inner(env, cfg.tilde_expansion).map(|f| {
            if cfg.split_fields_further {
                split_fields(f, env)
            } else {
                f
            }
        })
    }

    /// Evaluates a paarameter substitution without splitting fields further.
    fn eval_inner<E>(&self, env: &mut E, tilde_expansion: TildeExpansion) -> Result<Fields>
        where E: Environment,
              W: WordEval<E>,
              C: Run<E>,
    {
        use syntax::ast::ParameterSubstitution::*;

        const EMPTY_FIELD: Fields = Fields::Zero;

        fn remove_pattern<E, W, F>(param: &Parameter,
                                   pat: &Option<W>,
                                   env: &mut E,
                                   remove: F) -> Result<Option<Fields>>
            where E: Environment,
                  W: WordEval<E>,
                  F: Fn(Rc<String>, &glob::Pattern) -> Rc<String>
        {
            let map = |v: Vec<Rc<String>>, p| v.into_iter().map(|f| remove(f, &p)).collect();
            let param = param.eval(false, env);

            match *pat {
                None => Ok(param),
                Some(ref pat) => {
                    let pat = try!(pat.eval_as_pattern(env));
                    Ok(param.map(|p| match p {
                        Fields::Zero      => Fields::Zero,
                        Fields::Single(s) => Fields::Single(remove(s, &pat)),
                        Fields::At(v)    => Fields::At(map(v, pat)),
                        Fields::Star(v)  => Fields::Star(map(v, pat)),
                        Fields::Split(v) => Fields::Split(map(v, pat)),
                    }))
                },
            }
        }

        // Since we will do field splitting in the outer, public method,
        // we don't want internal word evaluations to also do field splitting
        // otherwise we will doubly split and potentially lose some fields.
        let cfg = WordEvalConfig {
            tilde_expansion: tilde_expansion,
            split_fields_further: false,
        };

        let null_str   = Rc::new(String::new());
        let match_opts = glob::MatchOptions {
            case_sensitive: true,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        // A macro that evaluates a parameter in some environment and immediately
        // returns the result as long as there is at least one non-empty field inside.
        // If all fields from the evaluated result are empty and the evaluation is
        // considered NON-strict, an empty vector is returned to the caller.
        macro_rules! check_param_subst {
            ($param:expr, $env:expr, $strict:expr) => {{
                if let Some(fields) = $param.eval(false, $env) {
                    if !fields.is_null() {
                        return Ok(fields)
                    } else if !$strict {
                        return Ok(EMPTY_FIELD)
                    }
                }
            }}
        }

        let ret = match *self {
            Command(ref body) => {
                try!(run_cmd_subst(body, env).map_err(|e| RuntimeError::Io(e, None))).into()
            },

            // We won't do field splitting here because any field expansions
            // should be done on the result we are about to return, and not the
            // intermediate value.
            Len(ref p) => Fields::Single(Rc::new(match p.eval(false, env) {
                None |
                Some(Fields::Zero) => String::from("0"),

                Some(Fields::Single(s)) => s.len().to_string(),

                Some(Fields::At(v))   |
                Some(Fields::Star(v)) => v.len().to_string(),

                // Since we should have specified NO field splitting above,
                // this variant should never occur.
                Some(Fields::Split(_)) => unreachable!(),
            })),

            Arith(ref a) => Fields::Single(Rc::new(match *a {
                Some(ref a) => try!(a.eval(env)).to_string(),
                None => String::from("0"),
            })),

            Default(strict, ref p, ref default) => {
                check_param_subst!(p, env, strict);
                match *default {
                    Some(ref w) => try!(w.eval_with_config(env, cfg)),
                    None => EMPTY_FIELD,
                }
            },

            Assign(strict, ref p, ref assig) => {
                check_param_subst!(p, env, strict);
                match p {
                    p@&Parameter::At       |
                    p@&Parameter::Star     |
                    p@&Parameter::Pound    |
                    p@&Parameter::Question |
                    p@&Parameter::Dash     |
                    p@&Parameter::Dollar   |
                    p@&Parameter::Bang     |
                    p@&Parameter::Positional(_) => {
                        env.set_last_status(EXIT_ERROR);
                        return Err(ExpansionError::BadAssig(p.clone()).into());
                    },

                    &Parameter::Var(ref name) => {
                        let val = match *assig {
                            Some(ref w) => try!(w.eval_with_config(env, cfg)),
                            None => EMPTY_FIELD,
                        };

                        env.set_var(name.clone(), val.clone().join());
                        val
                    },
                }
            },

            Error(strict, ref p, ref msg) => {
                check_param_subst!(p, env, strict);
                let msg = match *msg {
                    None => String::from("parameter null or not set"),
                    Some(ref w) => {
                        let rc = try!(w.eval_with_config(env, cfg)).join();
                        Rc::try_unwrap(rc).unwrap_or_else(|rc| (*rc).clone())
                    },
                };

                env.set_last_status(EXIT_ERROR);
                return Err(ExpansionError::EmptyParameter(p.clone(), msg).into());
            },

            Alternative(strict, ref p, ref alt) => {
                let val = p.eval(false, env);
                if val.is_none() || (strict && val.unwrap().is_null()) {
                    return Ok(EMPTY_FIELD);
                }

                match *alt {
                    Some(ref w) => try!(w.eval_with_config(env, cfg)),
                    None => EMPTY_FIELD,
                }
            },

            RemoveSmallestSuffix(ref p, ref pat) => try!(remove_pattern(p, pat, env, |s, pat| {
                let len = s.len();
                for idx in 0..len {
                    let idx = len - idx - 1;
                    if pat.matches_with(&s[idx..], &match_opts) {
                        return Rc::new(String::from(&s[0..idx]));
                    }
                }
                s
            })).unwrap_or(EMPTY_FIELD),

            RemoveLargestSuffix(ref p, ref pat) => try!(remove_pattern(p, pat, env, |s, pat| {
                let mut longest_start = None;
                let len = s.len();
                for idx in 0..len {
                    let idx = len - idx - 1;
                    if pat.matches_with(&s[idx..], &match_opts) {
                        longest_start = Some(idx);
                    }
                }

                match longest_start {
                    None => s,
                    Some(idx) => Rc::new(String::from(&s[0..idx])),
                }
            })).unwrap_or(EMPTY_FIELD),

            RemoveSmallestPrefix(ref p, ref pat) => try!(remove_pattern(p, pat, env, |s, pat| {
                for idx in 0..s.len() {
                    if pat.matches_with(&s[0..idx], &match_opts) {
                        return Rc::new(String::from(&s[idx..]));
                    }
                }

                // Don't forget to check the entire string for a match
                if pat.matches_with(&s, &match_opts) {
                    null_str.clone()
                } else {
                    s
                }
            })).unwrap_or(EMPTY_FIELD),

            RemoveLargestPrefix(ref p, ref pat) => try!(remove_pattern(p, pat, env, |s, pat| {
                if pat.matches_with(&s, &match_opts) {
                    return null_str.clone();
                }

                let mut longest_end = None;
                for idx in 0..s.len() {
                    if pat.matches_with(&s[0..idx], &match_opts) {
                        longest_end = Some(idx);
                    }
                }

                match longest_end {
                    None => s,
                    Some(idx) => Rc::new(String::from(&s[idx..])),
                }
            })).unwrap_or(EMPTY_FIELD),
        };

        Ok(match ret {
            Fields::Single(ref s) if s.is_empty() => EMPTY_FIELD,
            field => field,
        })
    }
}

/// Runs a collection of `Run`able commands as a command substitution.
/// The output of the commands will be captured, and trailing newlines trimmed.
fn run_cmd_subst<I, E>(body: I, env: &E) -> io::Result<String>
    where I: IntoIterator,
          I::Item: Run<E>,
          E: Environment,
{
    use runtime::{run_as_subshell, STDOUT_FILENO};
    use runtime::io::{Permissions, Pipe};
    use std::thread;

    let Pipe { reader: mut cmd_output, writer: cmd_stdout_fd } = try!(Pipe::new());

    let child_thread = try!(thread::Builder::new().spawn(move || {
        let mut buf = Vec::new();
        try!(io::copy(&mut cmd_output, &mut buf));
        Ok(buf)
    }));

    {
        let mut env = env.sub_env();
        let cmd_stdout_fd = Rc::new(cmd_stdout_fd);
        env.set_file_desc(STDOUT_FILENO, cmd_stdout_fd.clone(), Permissions::Write);
        let _ = run_as_subshell(body, &env);

        // Make sure that we drop env, and thus the writer end of the pipe here,
        // otherwise we could deadlock while waiting on a read on the pipe.
        drop(env);
        let cmd_stdout_fd = try!(Rc::try_unwrap(cmd_stdout_fd).map_err(|_| {
            io::Error::new(io::ErrorKind::WouldBlock, "writer end of pipe to command substitution \
                           was not closed, and would have caused a deadlock")
        }));
        drop(cmd_stdout_fd);
    }

    let mut buf = match child_thread.join() {
        Ok(Ok(buf)) => buf,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(
            io::Error::new(io::ErrorKind::Other, "thread capturing command output panicked")
        ),
    };

    // Trim the trailing newlines as per POSIX spec
    while Some(&b'\n') == buf.last() {
        buf.pop();
    }

    String::from_utf8(buf).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "command substitution output is not valid UTF-8")
    })
}

/// Splits a vector of fields further based on the contents of the `IFS`
/// variable (i.e. as long as it is non-empty). Any empty fields, original
/// or otherwise created will be discarded.
fn split_fields<E: Environment>(fields: Fields, env: &E) -> Fields {
    match fields {
        Fields::Zero      => Fields::Zero,
        Fields::Single(f) => split_fields_internal(vec!(f), env).into(),
        Fields::At(fs)    => Fields::At(split_fields_internal(fs, env)),
        Fields::Star(fs)  => Fields::Star(split_fields_internal(fs, env)),
        Fields::Split(fs) => Fields::Split(split_fields_internal(fs, env)),
    }
}

/// Actual implementation of `split_fields`.
fn split_fields_internal<E: Environment>(words: Vec<Rc<String>>, env: &E) -> Vec<Rc<String>> {
    // If IFS is set but null, there is nothing left to split
    let ifs = env.var("IFS").map_or(IFS_DEFAULT, |s| s);
    if ifs.is_empty() {
        return words;
    }

    let whitespace: Vec<char> = ifs.chars().filter(|c| c.is_whitespace()).collect();

    let mut fields = Vec::with_capacity(words.len());
    'word: for word in words {
        if word.is_empty() {
            continue;
        }

        let mut iter = word.chars().enumerate().peekable();
        loop {
            let start;
            loop {
                match iter.next() {
                    // If we are still skipping leading whitespace, and we hit the
                    // end of the word there are no fields to create, even empty ones.
                    None => continue 'word,
                    Some((idx, c)) => {
                        if whitespace.contains(&c) {
                            continue;
                        } else if ifs.contains(c) {
                            // If we hit an IFS char here then we have encountered an
                            // empty field, since the last iteration of this loop either
                            // had just consumed an IFS char, or its the start of the word.
                            // In either case the result should be the same.
                            fields.push(Rc::new(String::new()));
                        } else {
                            // Must have found a regular field character
                            start = idx;
                            break;
                        }
                    },
                }
            }

            let end;
            loop {
                match iter.next() {
                    None => {
                        end = None;
                        break;
                    },
                    Some((idx, c)) => if ifs.contains(c) {
                        end = Some(idx);
                        break;
                    },
                }
            }

            let field = match end {
                Some(end) => &word[start..end],
                None      => &word[start..],
            };

            fields.push(Rc::new(String::from(field)));

            // Since now we've hit an IFS character, we need to also skip past
            // any adjacent IFS whitespace as well. This also conveniently
            // ignores any trailing IFS whitespace in the input as well.
            loop {
                match iter.peek() {
                    Some(&(_, c)) if whitespace.contains(&c) => {
                        iter.next();
                    },
                    Some(_) |
                    None => break,
                }
            }
        }
    }

    fields.shrink_to_fit();
    fields
}

impl Arithmetic {
    /// Evaluates an arithmetic expression in the context of an environment.
    /// A mutable reference to the environment is needed since an arithmetic
    /// expression could mutate environment variables.
    #[cfg_attr(feature = "clippy", allow(if_not_else))]
    pub fn eval<E: Environment>(&self, env: &mut E) -> result::Result<isize, ExpansionError> {
        use syntax::ast::Arithmetic::*;

        let get_var = |env: &E, var| env.var(var).and_then(|s| s.parse().ok()).unwrap_or(0);

        let ret = match *self {
            Literal(lit) => lit,
            Var(ref var) => get_var(env, var),

            PostIncr(ref var) => {
                let value = get_var(env, var);
                env.set_var(var.clone(), Rc::new((value + 1).to_string()));
                value
            },

            PostDecr(ref var) => {
                let value = get_var(env, var);
                env.set_var(var.clone(), Rc::new((value - 1).to_string()));
                value
            },

            PreIncr(ref var) => {
                let value = get_var(env, var) + 1;
                env.set_var(var.clone(), Rc::new(value.to_string()));
                value
            },

            PreDecr(ref var) => {
                let value = get_var(env, var) - 1;
                env.set_var(var.clone(), Rc::new(value.to_string()));
                value
            },

            UnaryPlus(ref expr)  => try!(expr.eval(env)).abs(),
            UnaryMinus(ref expr) => -try!(expr.eval(env)),
            BitwiseNot(ref expr) => try!(expr.eval(env)) ^ !0,
            LogicalNot(ref expr) => if try!(expr.eval(env)) == 0 { 1 } else { 0 },

            Less(ref left, ref right)    => if try!(left.eval(env)) <  try!(right.eval(env)) { 1 } else { 0 },
            LessEq(ref left, ref right)  => if try!(left.eval(env)) <= try!(right.eval(env)) { 1 } else { 0 },
            Great(ref left, ref right)   => if try!(left.eval(env)) >  try!(right.eval(env)) { 1 } else { 0 },
            GreatEq(ref left, ref right) => if try!(left.eval(env)) >= try!(right.eval(env)) { 1 } else { 0 },
            Eq(ref left, ref right)      => if try!(left.eval(env)) == try!(right.eval(env)) { 1 } else { 0 },
            NotEq(ref left, ref right)   => if try!(left.eval(env)) != try!(right.eval(env)) { 1 } else { 0 },

            Pow(ref left, ref right) => {
                let right = try!(right.eval(env));
                if right.is_negative() {
                    env.set_last_status(EXIT_ERROR);
                    return Err(ExpansionError::NegativeExponent);
                } else {
                    try!(left.eval(env)).pow(right as u32)
                }
            },

            Div(ref left, ref right) => {
                let right = try!(right.eval(env));
                if right == 0 {
                    env.set_last_status(EXIT_ERROR);
                    return Err(ExpansionError::DivideByZero);
                } else {
                    try!(left.eval(env)) / right
                }
            },

            Modulo(ref left, ref right) => {
                let right = try!(right.eval(env));
                if right == 0 {
                    env.set_last_status(EXIT_ERROR);
                    return Err(ExpansionError::DivideByZero);
                } else {
                    try!(left.eval(env)) % right
                }
            },

            Mult(ref left, ref right)       => try!(left.eval(env)) *  try!(right.eval(env)),
            Add(ref left, ref right)        => try!(left.eval(env)) +  try!(right.eval(env)),
            Sub(ref left, ref right)        => try!(left.eval(env)) -  try!(right.eval(env)),
            ShiftLeft(ref left, ref right)  => try!(left.eval(env)) << try!(right.eval(env)),
            ShiftRight(ref left, ref right) => try!(left.eval(env)) >> try!(right.eval(env)),
            BitwiseAnd(ref left, ref right) => try!(left.eval(env)) &  try!(right.eval(env)),
            BitwiseXor(ref left, ref right) => try!(left.eval(env)) ^  try!(right.eval(env)),
            BitwiseOr(ref left, ref right)  => try!(left.eval(env)) |  try!(right.eval(env)),

            LogicalAnd(ref left, ref right) => if try!(left.eval(env)) != 0 {
                if try!(right.eval(env)) != 0 { 1 } else { 0 }
            } else {
                0
            },

            LogicalOr(ref left, ref right) => if try!(left.eval(env)) == 0 {
                if try!(right.eval(env)) != 0 { 1 } else { 0 }
            } else {
                1
            },

            Ternary(ref guard, ref thn, ref els) => if try!(guard.eval(env)) != 0 {
                try!(thn.eval(env))
            } else {
                try!(els.eval(env))
            },

            Assign(ref var, ref value) => {
                let value = try!(value.eval(env));
                env.set_var(var.clone(), Rc::new(value.to_string()));
                value
            },

            Sequence(ref exprs) => {
                let mut last = 0;
                for e in exprs.iter() {
                    last = try!(e.eval(env));
                }
                last
            },
        };

        Ok(ret)
    }
}

#[cfg(test)]
#[cfg_attr(feature = "clippy", allow(similar_names))]
mod tests {
    use glob;
    use runtime::{Env, EnvConfig, Environment, ExitStatus, EXIT_SUCCESS, EXIT_ERROR, ExpansionError,
                  Result, Run, RuntimeError};
    use runtime::eval::{Fields, TildeExpansion, WordEval, WordEvalConfig};
    use std::rc::Rc;
    use syntax::ast::{Arithmetic, Parameter, ParameterSubstitution, TopLevelWord};
    use syntax::parse::test::cmd_args;

    #[derive(Copy, Clone, Debug)]
    struct MockCmd;
    impl<E: Environment> Run<E> for MockCmd {
        fn run(&self, _: &mut E) -> Result<ExitStatus> {
            Ok(EXIT_SUCCESS)
        }
    }

    #[derive(Copy, Clone, Debug)]
    struct MockSubstWord(&'static str);

    impl<E: Environment> WordEval<E> for MockSubstWord {
        fn eval_with_config(&self, _: &mut E, cfg: WordEvalConfig) -> Result<Fields>
        {
            // Patterns and other words part of substitutions should never be split
            // while the substitution is evaluating them. Any splitting should be done
            // before returning the substitution result to the caller.
            assert_eq!(cfg.split_fields_further, false);
            Ok(self.0.to_owned().into())
        }

        fn eval_as_pattern(&self, _: &mut E) -> Result<glob::Pattern> {
            Ok(glob::Pattern::new(self.0).unwrap())
        }
    }

    type ParamSubst = ParameterSubstitution<MockSubstWord, MockCmd>;

    #[test]
    fn test_eval_parameter_with_set_vars() {
        let var1 = Rc::new("var1_value".to_owned());
        let var2 = Rc::new("var2_value".to_owned());
        let var3 = Rc::new("".to_owned()); // var3 is set to the empty string

        let arg1 = Rc::new("arg1_value".to_owned());
        let arg2 = Rc::new("arg2_value".to_owned());
        let arg3 = Rc::new("arg3_value".to_owned());

        let args = vec!(
            arg1.clone(),
            arg2.clone(),
            arg3.clone(),
        );

        let env_args = args.iter().map(|rc| (**rc).clone()).collect();
        let mut env = Env::with_config(EnvConfig {
            args: Some(env_args),
            .. Default::default()
        }).unwrap();

        env.set_var("var1".to_owned(), var1.clone());
        env.set_var("var2".to_owned(), var2.clone());
        env.set_var("var3".to_owned(), var3.clone());

        assert_eq!(Parameter::At.eval(false, &env), Some(Fields::At(args.clone())));
        assert_eq!(Parameter::Star.eval(false, &env), Some(Fields::Star(args.clone())));

        assert_eq!(Parameter::Dollar.eval(false, &env), Some(Fields::Single(unsafe {
            ::libc::getpid().to_string().into()
        })));

        // FIXME: test these
        //assert_eq!(Parameter::Dash.eval(false, &env), ...);
        //assert_eq!(Parameter::Bang.eval(false, &env), ...);

        // Before anything is run it should be considered a success
        assert_eq!(Parameter::Question.eval(false, &env), Some(Fields::Single("0".to_owned().into())));
        env.set_last_status(ExitStatus::Code(3));
        assert_eq!(Parameter::Question.eval(false, &env), Some(Fields::Single("3".to_owned().into())));
        // Signals should have 128 added to them
        env.set_last_status(ExitStatus::Signal(5));
        assert_eq!(Parameter::Question.eval(false, &env), Some(Fields::Single("133".to_owned().into())));

        assert_eq!(Parameter::Positional(0).eval(false, &env), Some(Fields::Single(env.name().clone())));
        assert_eq!(Parameter::Positional(1).eval(false, &env), Some(Fields::Single(arg1)));
        assert_eq!(Parameter::Positional(2).eval(false, &env), Some(Fields::Single(arg2)));
        assert_eq!(Parameter::Positional(3).eval(false, &env), Some(Fields::Single(arg3)));

        assert_eq!(Parameter::Var("var1".to_owned()).eval(false, &env), Some(Fields::Single(var1)));
        assert_eq!(Parameter::Var("var2".to_owned()).eval(false, &env), Some(Fields::Single(var2)));
        assert_eq!(Parameter::Var("var3".to_owned()).eval(false, &env), Some(Fields::Single(var3)));

        assert_eq!(Parameter::Pound.eval(false, &env), Some(Fields::Single("3".to_owned().into())));
    }

    #[test]
    fn test_eval_parameter_with_unset_vars() {
        let env = Env::with_config(EnvConfig {
            args: Some(vec!()),
            .. Default::default()
        }).unwrap();

        assert_eq!(Parameter::At.eval(false, &env), Some(Fields::Zero));
        assert_eq!(Parameter::Star.eval(false, &env), Some(Fields::Zero));

        // FIXME: test these
        //assert_eq!(Parameter::Dash.eval(false, &env), ...);
        //assert_eq!(Parameter::Bang.eval(false, &env), ...);

        assert_eq!(Parameter::Positional(0).eval(false, &env), Some(Fields::Single(env.name().clone())));
        assert_eq!(Parameter::Positional(1).eval(false, &env), None);
        assert_eq!(Parameter::Positional(2).eval(false, &env), None);

        assert_eq!(Parameter::Var("var1".to_owned()).eval(false, &env), None);
        assert_eq!(Parameter::Var("var2".to_owned()).eval(false, &env), None);

        assert_eq!(Parameter::Pound.eval(false, &env), Some(Fields::Single("0".to_owned().into())));
    }

    #[test]
    fn test_eval_parameter_splitting_with_default_ifs() {
        let val1 = Rc::new(" \t\nfoo\n\n\nbar \t\n".to_owned());
        let val2 = Rc::new("".to_owned());

        let args = vec!(
            val1.clone(),
            val2.clone(),
        );

        let env_args = args.iter().map(|rc| (**rc).clone()).collect();
        let mut env = Env::with_config(EnvConfig {
            args: Some(env_args),
            .. Default::default()
        }).unwrap();

        env.set_var("var1".to_owned(), val1.clone());
        env.set_var("var2".to_owned(), val2.clone());

        // Splitting should NOT keep any IFS whitespace fields
        let fields_args = vec!("foo".to_owned().into(), "bar".to_owned().into());

        // With splitting
        assert_eq!(Parameter::At.eval(true, &env), Some(Fields::At(fields_args.clone())));
        assert_eq!(Parameter::Star.eval(true, &env), Some(Fields::Star(fields_args.clone())));

        let fields_foo_bar = Fields::Split(fields_args.clone());

        assert_eq!(Parameter::Positional(1).eval(true, &env), Some(fields_foo_bar.clone()));
        assert_eq!(Parameter::Positional(2).eval(true, &env), Some(Fields::Zero));

        assert_eq!(Parameter::Var("var1".to_owned()).eval(true, &env), Some(fields_foo_bar.clone()));
        assert_eq!(Parameter::Var("var2".to_owned()).eval(true, &env), Some(Fields::Zero));

        // Without splitting
        assert_eq!(Parameter::At.eval(false, &env), Some(Fields::At(args.clone())));
        assert_eq!(Parameter::Star.eval(false, &env), Some(Fields::Star(args.clone())));

        assert_eq!(Parameter::Positional(1).eval(false, &env), Some(Fields::Single(val1.clone())));
        assert_eq!(Parameter::Positional(2).eval(false, &env), Some(Fields::Single(val2.clone())));

        assert_eq!(Parameter::Var("var1".to_owned()).eval(false, &env), Some(Fields::Single(val1)));
        assert_eq!(Parameter::Var("var2".to_owned()).eval(false, &env), Some(Fields::Single(val2)));
    }

    #[test]
    fn test_eval_parameter_splitting_with_custom_ifs() {
        let val1 = Rc::new("   foo000bar   ".to_owned());
        let val2 = Rc::new("  00 0 00  0 ".to_owned());
        let val3 = Rc::new("".to_owned());

        let args = vec!(
            val1.clone(),
            val2.clone(),
            val3.clone(),
        );

        let env_args = args.iter().map(|rc| (**rc).clone()).collect();
        let mut env = Env::with_config(EnvConfig {
            args: Some(env_args),
            .. Default::default()
        }).unwrap();

        env.set_var("IFS".to_owned(), "0 ".to_owned().into());

        env.set_var("var1".to_owned(), val1.clone());
        env.set_var("var2".to_owned(), val2.clone());
        env.set_var("var3".to_owned(), val3.clone());

        // Splitting SHOULD keep empty fields between IFS chars which are NOT whitespace
        let fields_args = vec!(
            "foo".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
            "bar".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
            // Already empty word should result in Zero fields
        );

        // With splitting
        assert_eq!(Parameter::At.eval(true, &env), Some(Fields::At(fields_args.clone())));
        assert_eq!(Parameter::Star.eval(true, &env), Some(Fields::Star(fields_args.clone())));

        let fields_foo_bar = Fields::Split(vec!(
            "foo".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
            "bar".to_owned().into(),
        ));

        let fields_all_blanks = Fields::Split(vec!(
            "".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
        ));

        assert_eq!(Parameter::Positional(1).eval(true, &env), Some(fields_foo_bar.clone()));
        assert_eq!(Parameter::Positional(2).eval(true, &env), Some(fields_all_blanks.clone()));
        assert_eq!(Parameter::Positional(3).eval(true, &env), Some(Fields::Zero));

        assert_eq!(Parameter::Var("var1".to_owned()).eval(true, &env), Some(fields_foo_bar));
        assert_eq!(Parameter::Var("var2".to_owned()).eval(true, &env), Some(fields_all_blanks));
        assert_eq!(Parameter::Var("var3".to_owned()).eval(true, &env), Some(Fields::Zero));

        // FIXME: test these
        //assert_eq!(Parameter::Dash.eval(false, &env), ...);
        //assert_eq!(Parameter::Bang.eval(false, &env), ...);

        env.set_last_status(EXIT_SUCCESS);
        assert_eq!(Parameter::Question.eval(true, &env), Some(Fields::Single("".to_owned().into())));

        // Without splitting
        assert_eq!(Parameter::At.eval(false, &env), Some(Fields::At(args.clone())));
        assert_eq!(Parameter::Star.eval(false, &env), Some(Fields::Star(args.clone())));

        assert_eq!(Parameter::Positional(1).eval(false, &env), Some(Fields::Single(val1.clone())));
        assert_eq!(Parameter::Positional(2).eval(false, &env), Some(Fields::Single(val2.clone())));
        assert_eq!(Parameter::Positional(3).eval(false, &env), Some(Fields::Single(val3.clone())));

        assert_eq!(Parameter::Var("var1".to_owned()).eval(false, &env), Some(Fields::Single(val1)));
        assert_eq!(Parameter::Var("var2".to_owned()).eval(false, &env), Some(Fields::Single(val2)));
        assert_eq!(Parameter::Var("var3".to_owned()).eval(false, &env), Some(Fields::Single(val3)));

        // FIXME: test these
        //assert_eq!(Parameter::Dash.eval(false, &env), ...);
        //assert_eq!(Parameter::Bang.eval(false, &env), ...);

        env.set_last_status(EXIT_SUCCESS);
        assert_eq!(Parameter::Question.eval(false, &env), Some(Fields::Single("0".to_owned().into())));
    }

    #[test]
    fn test_eval_parameter_splitting_with_empty_ifs() {
        let val1 = Rc::new(" \t\nfoo\n\n\nbar \t\n".to_owned());
        let val2 = Rc::new("".to_owned());

        let args = vec!(
            val1.clone(),
            val2.clone(),
        );

        let env_args = args.iter().map(|rc| (**rc).clone()).collect();
        let mut env = Env::with_config(EnvConfig {
            args: Some(env_args),
            .. Default::default()
        }).unwrap();

        env.set_var("IFS".to_owned(), "".to_owned().into());
        env.set_var("var1".to_owned(), val1.clone());
        env.set_var("var2".to_owned(), val2.clone());

        // Splitting with empty IFS should keep fields as they are
        let field_args = args;
        let field1 = Fields::Single(val1);
        let field2 = Fields::Single(val2);

        // With splitting
        assert_eq!(Parameter::At.eval(true, &env), Some(Fields::At(field_args.clone())));
        assert_eq!(Parameter::Star.eval(true, &env), Some(Fields::Star(field_args.clone())));

        assert_eq!(Parameter::Positional(1).eval(true, &env), Some(field1.clone()));
        assert_eq!(Parameter::Positional(2).eval(true, &env), Some(field2.clone()));

        assert_eq!(Parameter::Var("var1".to_owned()).eval(true, &env), Some(field1.clone()));
        assert_eq!(Parameter::Var("var2".to_owned()).eval(true, &env), Some(field2.clone()));

        // Without splitting
        assert_eq!(Parameter::At.eval(false, &env), Some(Fields::At(field_args.clone())));
        assert_eq!(Parameter::Star.eval(false, &env), Some(Fields::Star(field_args.clone())));

        assert_eq!(Parameter::Positional(1).eval(false, &env), Some(field1.clone()));
        assert_eq!(Parameter::Positional(2).eval(false, &env), Some(field2.clone()));

        assert_eq!(Parameter::Var("var1".to_owned()).eval(false, &env), Some(field1.clone()));
        assert_eq!(Parameter::Var("var2".to_owned()).eval(false, &env), Some(field2.clone()));
    }

    #[test]
    fn test_eval_arith() {
        use ::std::isize::MAX;
        use syntax::ast::Arithmetic::*;

        fn lit(i: isize) -> Box<Arithmetic> {
            Box::new(Literal(i))
        }

        let mut env = Env::new().unwrap();
        let env = &mut env;
        let var = "var name".to_owned();
        let var_value = 10;
        let var_string = "var string".to_owned();
        let var_string_value = "asdf";

        env.set_var(var.clone(),        var_value.to_string().into());
        env.set_var(var_string.clone(), var_string_value.to_owned().into());

        assert_eq!(Literal(5).eval(env), Ok(5));

        assert_eq!(Var(var.clone()).eval(env), Ok(var_value));
        assert_eq!(Var(var_string.clone()).eval(env), Ok(0));
        assert_eq!(Var("missing var".to_owned()).eval(env), Ok(0));

        assert_eq!(PostIncr(var.clone()).eval(env), Ok(var_value));
        assert_eq!(&**env.var(&var).unwrap(), &*(var_value + 1).to_string());
        assert_eq!(PostDecr(var.clone()).eval(env), Ok(var_value + 1));
        assert_eq!(&**env.var(&var).unwrap(), &*var_value.to_string());

        assert_eq!(PreIncr(var.clone()).eval(env), Ok(var_value + 1));
        assert_eq!(&**env.var(&var).unwrap(), &*(var_value + 1).to_string());
        assert_eq!(PreDecr(var.clone()).eval(env), Ok(var_value));
        assert_eq!(&**env.var(&var).unwrap(), &*var_value.to_string());

        assert_eq!(UnaryPlus(lit(5)).eval(env), Ok(5));
        assert_eq!(UnaryPlus(lit(-5)).eval(env), Ok(5));

        assert_eq!(UnaryMinus(lit(5)).eval(env), Ok(-5));
        assert_eq!(UnaryMinus(lit(-5)).eval(env), Ok(5));

        assert_eq!(BitwiseNot(lit(5)).eval(env), Ok(!5));
        assert_eq!(BitwiseNot(lit(0)).eval(env), Ok(!0));

        assert_eq!(LogicalNot(lit(5)).eval(env), Ok(0));
        assert_eq!(LogicalNot(lit(0)).eval(env), Ok(1));

        assert_eq!(Less(lit(1), lit(1)).eval(env), Ok(0));
        assert_eq!(Less(lit(1), lit(0)).eval(env), Ok(0));
        assert_eq!(Less(lit(0), lit(1)).eval(env), Ok(1));

        assert_eq!(LessEq(lit(1), lit(1)).eval(env), Ok(1));
        assert_eq!(LessEq(lit(1), lit(0)).eval(env), Ok(0));
        assert_eq!(LessEq(lit(0), lit(1)).eval(env), Ok(1));

        assert_eq!(Great(lit(1), lit(1)).eval(env), Ok(0));
        assert_eq!(Great(lit(1), lit(0)).eval(env), Ok(1));
        assert_eq!(Great(lit(0), lit(1)).eval(env), Ok(0));

        assert_eq!(GreatEq(lit(1), lit(1)).eval(env), Ok(1));
        assert_eq!(GreatEq(lit(1), lit(0)).eval(env), Ok(1));
        assert_eq!(GreatEq(lit(0), lit(1)).eval(env), Ok(0));

        assert_eq!(Eq(lit(0), lit(1)).eval(env), Ok(0));
        assert_eq!(Eq(lit(1), lit(1)).eval(env), Ok(1));

        assert_eq!(NotEq(lit(0), lit(1)).eval(env), Ok(1));
        assert_eq!(NotEq(lit(1), lit(1)).eval(env), Ok(0));

        assert_eq!(Pow(lit(4), lit(3)).eval(env), Ok(64));
        assert_eq!(Pow(lit(4), lit(0)).eval(env), Ok(1));
        assert_eq!(Pow(lit(4), lit(-2)).eval(env), Err(ExpansionError::NegativeExponent));
        assert_eq!(env.last_status(), EXIT_ERROR);
        env.set_last_status(EXIT_SUCCESS);

        assert_eq!(Div(lit(6), lit(2)).eval(env), Ok(3));
        assert_eq!(Div(lit(1), lit(0)).eval(env), Err(ExpansionError::DivideByZero));
        assert_eq!(env.last_status(), EXIT_ERROR);
        env.set_last_status(EXIT_SUCCESS);

        assert_eq!(Modulo(lit(6), lit(5)).eval(env), Ok(1));
        assert_eq!(Modulo(lit(1), lit(0)).eval(env), Err(ExpansionError::DivideByZero));
        assert_eq!(env.last_status(), EXIT_ERROR);
        env.set_last_status(EXIT_SUCCESS);

        assert_eq!(Mult(lit(3), lit(2)).eval(env), Ok(6));
        assert_eq!(Mult(lit(1), lit(0)).eval(env), Ok(0));

        assert_eq!(Add(lit(3), lit(2)).eval(env), Ok(5));
        assert_eq!(Add(lit(1), lit(0)).eval(env), Ok(1));

        assert_eq!(Sub(lit(3), lit(2)).eval(env), Ok(1));
        assert_eq!(Sub(lit(0), lit(1)).eval(env), Ok(-1));

        assert_eq!(ShiftLeft(lit(4), lit(3)).eval(env), Ok(32));

        assert_eq!(ShiftRight(lit(32), lit(2)).eval(env), Ok(8));

        assert_eq!(BitwiseAnd(lit(135), lit(97)).eval(env), Ok(1));
        assert_eq!(BitwiseAnd(lit(135), lit(0)).eval(env), Ok(0));
        assert_eq!(BitwiseAnd(lit(135), lit(MAX)).eval(env), Ok(135));

        assert_eq!(BitwiseXor(lit(135), lit(150)).eval(env), Ok(17));
        assert_eq!(BitwiseXor(lit(135), lit(0)).eval(env), Ok(135));
        assert_eq!(BitwiseXor(lit(135), lit(MAX)).eval(env), Ok(135 ^ MAX));

        assert_eq!(BitwiseOr(lit(135), lit(97)).eval(env), Ok(231));
        assert_eq!(BitwiseOr(lit(135), lit(0)).eval(env), Ok(135));
        assert_eq!(BitwiseOr(lit(135), lit(MAX)).eval(env), Ok(MAX));

        assert_eq!(LogicalAnd(lit(135), lit(97)).eval(env), Ok(1));
        assert_eq!(LogicalAnd(lit(135), lit(0)).eval(env), Ok(0));
        assert_eq!(LogicalAnd(lit(0), lit(0)).eval(env), Ok(0));

        assert_eq!(LogicalOr(lit(135), lit(97)).eval(env), Ok(1));
        assert_eq!(LogicalOr(lit(135), lit(0)).eval(env), Ok(1));
        assert_eq!(LogicalOr(lit(0), lit(0)).eval(env), Ok(0));

        assert_eq!(Ternary(lit(2), lit(4), lit(5)).eval(env), Ok(4));
        assert_eq!(Ternary(lit(0), lit(4), lit(5)).eval(env), Ok(5));

        assert_eq!(&**env.var(&var).unwrap(), &*(var_value).to_string());
        assert_eq!(Assign(var.clone(), lit(42)).eval(env), Ok(42));
        assert_eq!(&**env.var(&var).unwrap(), "42");

        assert_eq!(Sequence(vec!(
            Assign("x".to_owned(), lit(5)),
            Assign("y".to_owned(), lit(10)),
            Add(Box::new(PreIncr("x".to_owned())), Box::new(PostDecr("y".to_owned()))),
        )).eval(env), Ok(16));

        assert_eq!(&**env.var("x").unwrap(), "6");
        assert_eq!(&**env.var("y").unwrap(), "9");
    }

    #[test]
    fn test_eval_parameter_substitution_command() {
        use syntax::ast::ParameterSubstitution::Command;

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: false,
        };

        let mut env = Env::new().unwrap();
        let cmd_subst: ParameterSubstitution<TopLevelWord, _>
            = Command(vec!(cmd_args("echo", &["hello\n\n\n ~ * world\n\n\n\n"])));
        assert_eq!(cmd_subst.eval(&mut env, cfg), Ok("hello\n\n\n ~ * world".to_owned().into()));

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::All,
            split_fields_further: true,
        };
        assert_eq!(cmd_subst.eval(&mut env, cfg), Ok(Fields::Split(vec!(
            "hello".to_owned().into(),
            "~".to_owned().into(),
            "*".to_owned().into(),
            "world".to_owned().into(),
        ))));

        env.set_var("IFS".to_owned(), "o".to_owned().into());
        assert_eq!(cmd_subst.eval(&mut env, cfg), Ok(Fields::Split(vec!(
            "hell".to_owned().into(),
            "\n\n\n ~ * w".to_owned().into(),
            "rld".to_owned().into(),
        ))));
    }

    #[test]
    fn test_eval_parameter_substitution_len() {
        use syntax::ast::ParameterSubstitution::Len;

        let name  = "shell name".to_owned();
        let var   = "var".to_owned();
        let value = "foo bar".to_owned();

        let mut env = Env::with_config(EnvConfig {
            name: Some(name.clone()),
            args: Some(vec!(
                "one".to_owned(),
                "two".to_owned(),
                "three".to_owned(),
            )),
            .. Default::default()
        }).unwrap();

        env.set_var(var.clone(), Rc::new(value.clone()));

        let cases = vec!(
            (Parameter::At,    3),
            (Parameter::Star,  3),
            (Parameter::Pound, 1),
            (Parameter::Dollar, unsafe { ::libc::getpid().to_string().len() }),

            // FIXME: test these as well
            //Parameter::Dash,
            //Parameter::Bang,

            (Parameter::Positional(0), name.len()),
            (Parameter::Positional(3), 5),
            (Parameter::Positional(5), 0),
            (Parameter::Var(var), value.len()),
            (Parameter::Var("missing".to_owned()), 0),
        );

        for tilde in vec!(TildeExpansion::None, TildeExpansion::First, TildeExpansion::All) {
            for split in vec!(false, true) {
                // Field splitting and tilde expansion should not affect calculating Len
                let cfg = WordEvalConfig {
                    tilde_expansion: tilde,
                    split_fields_further: split,
                };

                for (param, result) in cases.clone() {
                    let len: ParamSubst = Len(param);
                    assert_eq!(len.eval(&mut env, cfg), Ok(Fields::Single(Rc::new(result.to_string()))));
                }

                env.set_last_status(EXIT_SUCCESS);
                let len: ParamSubst = Len(Parameter::Question);
                assert_eq!(len.eval(&mut env, cfg), Ok(Fields::Single("1".to_owned().into())));
                env.set_last_status(ExitStatus::Signal(5)); // Signals have an offset
                assert_eq!(len.eval(&mut env, cfg), Ok(Fields::Single("3".to_owned().into())));
            }
        }
    }

    #[test]
    fn test_eval_parameter_substitution_arith() {
        use syntax::ast::ParameterSubstitution::Arith;

        let mut env = Env::new().unwrap();

        for tilde in vec!(TildeExpansion::None, TildeExpansion::First, TildeExpansion::All) {
            for split in vec!(false, true) {
                // Field splitting and tilde expansion should not affect calculating Arith
                let cfg = WordEvalConfig {
                    tilde_expansion: tilde,
                    split_fields_further: split,
                };

                let arith: ParamSubst = Arith(None);
                assert_eq!(arith.eval(&mut env, cfg), Ok(Fields::Single("0".to_owned().into())));

                let arith: ParamSubst = Arith(Some(Arithmetic::Literal(5)));
                assert_eq!(arith.eval(&mut env, cfg), Ok(Fields::Single("5".to_owned().into())));

                let arith: ParamSubst = Arith(Some(
                    Arithmetic::Div(Box::new(Arithmetic::Literal(1)), Box::new(Arithmetic::Literal(0)))
                ));
                assert!(arith.eval(&mut env, cfg).is_err());
                assert_eq!(env.last_status(), EXIT_ERROR);
            }
        }
    }

    #[test]
    fn test_eval_parameter_substitution_default() {
        use syntax::ast::ParameterSubstitution::Default;

        const DEFAULT_VALUE: &'static str = "some default value";

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: false,
        };

        let var       = "non_empty_var".to_owned();
        let var_null  = "var with empty value".to_owned();
        let var_unset = "var_not_set_in_env".to_owned();

        let var_value = Rc::new("foobar".to_owned());
        let null      = Rc::new("".to_owned());

        let mut env = Env::new().unwrap();
        env.set_var(var.clone(),      var_value.clone());
        env.set_var(var_null.clone(), null.clone());

        let default_value = Fields::Single(Rc::new(DEFAULT_VALUE.to_owned()));
        let var_value     = Fields::Single(var_value);

        let default = MockSubstWord(DEFAULT_VALUE);

        // Strict with default
        let subst: ParamSubst = Default(true, Parameter::Var(var.clone()), Some(default));
        assert_eq!(subst.eval(&mut env, cfg), Ok(var_value.clone()));
        let subst: ParamSubst = Default(true, Parameter::Var(var_null.clone()), Some(default));
        assert_eq!(subst.eval(&mut env, cfg), Ok(default_value.clone()));
        let subst: ParamSubst = Default(true, Parameter::Var(var_unset.clone()), Some(default));
        assert_eq!(subst.eval(&mut env, cfg), Ok(default_value.clone()));

        // Strict without default
        let subst: ParamSubst = Default(true, Parameter::Var(var.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(var_value.clone()));
        let subst: ParamSubst = Default(true, Parameter::Var(var_null.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        let subst: ParamSubst = Default(true, Parameter::Var(var_unset.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));


        // Non-strict with default
        let subst: ParamSubst = Default(false, Parameter::Var(var.clone()), Some(default));
        assert_eq!(subst.eval(&mut env, cfg), Ok(var_value.clone()));
        let subst: ParamSubst = Default(false, Parameter::Var(var_null.clone()), Some(default));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        let subst: ParamSubst = Default(false, Parameter::Var(var_unset.clone()), Some(default));
        assert_eq!(subst.eval(&mut env, cfg), Ok(default_value.clone()));

        // Non-strict without default
        let subst: ParamSubst = Default(false, Parameter::Var(var.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(var_value.clone()));
        let subst: ParamSubst = Default(false, Parameter::Var(var_null.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        let subst: ParamSubst = Default(false, Parameter::Var(var_unset.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        // Args have one non-null argument
        {
            let args = vec!(
                "".to_owned(),
                "foo".to_owned(),
                "".to_owned(),
                "".to_owned(),
            );

            let args_value = args.iter().cloned().map(Rc::new).collect::<Vec<_>>();
            let mut env = Env::with_config(EnvConfig {
                args: Some(args),
                .. ::std::default::Default::default()
            }).unwrap();

            let subst: ParamSubst = Default(true, Parameter::At, Some(default));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::At(args_value.clone())));
            let subst: ParamSubst = Default(true, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::At(args_value.clone())));
            let subst: ParamSubst = Default(true, Parameter::Star, Some(default));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Star(args_value.clone())));
            let subst: ParamSubst = Default(true, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Star(args_value.clone())));

            let subst: ParamSubst = Default(false, Parameter::At, Some(default));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::At(args_value.clone())));
            let subst: ParamSubst = Default(false, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::At(args_value.clone())));
            let subst: ParamSubst = Default(false, Parameter::Star, Some(default));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Star(args_value.clone())));
            let subst: ParamSubst = Default(false, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Star(args_value.clone())));
        }

        // Args all null
        {
            let mut env = Env::with_config(EnvConfig {
                args: Some(vec!(
                    "".to_owned(),
                    "".to_owned(),
                    "".to_owned(),
                )),
                .. ::std::default::Default::default()
            }).unwrap();

            let subst: ParamSubst = Default(true, Parameter::At, Some(default));
            assert_eq!(subst.eval(&mut env, cfg), Ok(default_value.clone()));
            let subst: ParamSubst = Default(true, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Default(true, Parameter::Star, Some(default));
            assert_eq!(subst.eval(&mut env, cfg), Ok(default_value.clone()));
            let subst: ParamSubst = Default(true, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

            let subst: ParamSubst = Default(false, Parameter::At, Some(default));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Default(false, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Default(false, Parameter::Star, Some(default));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Default(false, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        }

        // Args not set
        {
            let mut env = Env::new().unwrap();

            let subst: ParamSubst = Default(true, Parameter::At, Some(default));
            assert_eq!(subst.eval(&mut env, cfg), Ok(default_value.clone()));
            let subst: ParamSubst = Default(true, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Default(true, Parameter::Star, Some(default));
            assert_eq!(subst.eval(&mut env, cfg), Ok(default_value.clone()));
            let subst: ParamSubst = Default(true, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

            let subst: ParamSubst = Default(false, Parameter::At, Some(default));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Default(false, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Default(false, Parameter::Star, Some(default));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Default(false, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        }
    }

    #[test]
    fn test_eval_parameter_substitution_assign() {
        use syntax::ast::ParameterSubstitution::Assign;

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: false,
        };

        let var         = "non_empty_var".to_owned();
        let var_value   = "foobar".to_owned();
        let var_null    = "var with empty value".to_owned();
        let var_unset   = "var_not_set_in_env".to_owned();

        let null = Rc::new(String::new());
        let assig = MockSubstWord("assigned value");

        let mut env = Env::new().unwrap();
        env.set_var(var.clone(), Rc::new(var_value.clone()));

        let assig_var_value = Rc::new(assig.0.to_owned());
        let var_value       = Fields::Single(Rc::new(var_value));
        let assig_value     = Fields::Single(assig_var_value.clone());

        // Variable set and non-null
        let subst: ParamSubst = Assign(true, Parameter::Var(var.clone()), Some(assig));
        assert_eq!(subst.eval(&mut env, cfg), Ok(var_value.clone()));
        let subst: ParamSubst = Assign(true, Parameter::Var(var.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(var_value.clone()));
        let subst: ParamSubst = Assign(false, Parameter::Var(var.clone()), Some(assig));
        assert_eq!(subst.eval(&mut env, cfg), Ok(var_value.clone()));
        let subst: ParamSubst = Assign(false, Parameter::Var(var.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(var_value.clone()));


        // Variable set but null
        env.set_var(var_null.clone(), null.clone());
        let subst: ParamSubst = Assign(true, Parameter::Var(var_null.clone()), Some(assig));
        assert_eq!(subst.eval(&mut env, cfg), Ok(assig_value.clone()));
        assert_eq!(env.var(&var_null), Some(&assig_var_value));

        env.set_var(var_null.clone(), null.clone());
        let subst: ParamSubst = Assign(true, Parameter::Var(var_null.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        assert_eq!(env.var(&var_null), Some(&null));

        env.set_var(var_null.clone(), null.clone());
        let subst: ParamSubst = Assign(false, Parameter::Var(var_null.clone()), Some(assig));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        assert_eq!(env.var(&var_null), Some(&null));

        env.set_var(var_null.clone(), null.clone());
        let subst: ParamSubst = Assign(false, Parameter::Var(var_null.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        assert_eq!(env.var(&var_null), Some(&null));


        // Variable unset
        {
            let mut env = env.sub_env();
            let subst: ParamSubst = Assign(true, Parameter::Var(var_unset.clone()), Some(assig));
            assert_eq!(subst.eval(&mut env, cfg), Ok(assig_value.clone()));
            assert_eq!(env.var(&var_unset), Some(&assig_var_value));
        }

        {
            let mut env = env.sub_env();
            let subst: ParamSubst = Assign(true, Parameter::Var(var_unset.clone()), None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            assert_eq!(env.var(&var_unset), Some(&null));
        }

        {
            let mut env = env.sub_env();
            let subst: ParamSubst = Assign(false, Parameter::Var(var_unset.clone()), Some(assig));
            assert_eq!(subst.eval(&mut env, cfg), Ok(assig_value.clone()));
            assert_eq!(env.var(&var_unset), Some(&assig_var_value));
        }

        {
            let mut env = env.sub_env();
            let subst: ParamSubst = Assign(false, Parameter::Var(var_unset.clone()), None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            assert_eq!(env.var(&var_unset), Some(&null));
        }

        let unassignable_params = vec!(
            Parameter::At,
            Parameter::Star,
            Parameter::Dash,
            Parameter::Bang,

            // These parameters can't ever really be null or undefined,
            // so we won't test for them.
            //Parameter::Pound,
            //Parameter::Question,
            //Parameter::Dollar,
        );

        for param in unassignable_params {
            let err = ExpansionError::BadAssig(param.clone());
            let subst: ParamSubst = Assign(true, param.clone(), Some(assig));
            assert_eq!(subst.eval(&mut env, cfg), Err(RuntimeError::Expansion(err.clone())));
            assert_eq!(env.last_status(), EXIT_ERROR);
        }
    }

    #[test]
    fn test_eval_parameter_substitution_error() {
        use syntax::ast::ParameterSubstitution::Error;

        const ERR_MSG: &'static str = "this variable is not set!";

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: false,
        };

        let var       = "non_empty_var".to_owned();
        let var_null  = "var with empty value".to_owned();
        let var_unset = "var_not_set_in_env".to_owned();

        let var_value = Rc::new("foobar".to_owned());
        let null      = Rc::new("".to_owned());
        let err_msg   = ERR_MSG.to_owned();

        let mut env = Env::new().unwrap();
        env.set_var(var.clone(), var_value.clone());
        env.set_var(var_null.clone(), null.clone());

        let var_value = Fields::Single(var_value);

        let err_null  = RuntimeError::Expansion(
            ExpansionError::EmptyParameter(Parameter::Var(var_null.clone()),  err_msg.clone()));
        let err_unset = RuntimeError::Expansion(
            ExpansionError::EmptyParameter(Parameter::Var(var_unset.clone()), err_msg.clone()));
        let err_at    = RuntimeError::Expansion(
            ExpansionError::EmptyParameter(Parameter::At,                     err_msg.clone()));
        let err_star  = RuntimeError::Expansion(
            ExpansionError::EmptyParameter(Parameter::Star,                   err_msg.clone()));

        let err_msg = MockSubstWord(ERR_MSG);

        // Strict with error message
        let subst: ParamSubst = Error(true, Parameter::Var(var.clone()), Some(err_msg));
        assert_eq!(subst.eval(&mut env, cfg), Ok(var_value.clone()));

        env.set_last_status(EXIT_SUCCESS);
        let subst: ParamSubst = Error(true, Parameter::Var(var_null.clone()), Some(err_msg));
        assert_eq!(subst.eval(&mut env, cfg).as_ref(), Err(&err_null));
        assert_eq!(env.last_status(), EXIT_ERROR);

        env.set_last_status(EXIT_SUCCESS);
        let subst: ParamSubst = Error(true, Parameter::Var(var_unset.clone()), Some(err_msg));
        assert_eq!(subst.eval(&mut env, cfg).as_ref(), Err(&err_unset));
        assert_eq!(env.last_status(), EXIT_ERROR);


        // Strict without error message
        let subst: ParamSubst = Error(true, Parameter::Var(var.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(var_value.clone()));

        env.set_last_status(EXIT_SUCCESS);
        let subst: ParamSubst = Error(true, Parameter::Var(var_null.clone()), None);
        let eval = subst.eval(&mut env, cfg);
        if let Err(RuntimeError::Expansion(ExpansionError::EmptyParameter(param, _))) = eval {
            assert_eq!(param, Parameter::Var(var_null.clone()));
            assert_eq!(env.last_status(), EXIT_ERROR);
        } else {
            panic!("Unexpected evaluation: {:?}", eval);
        }

        env.set_last_status(EXIT_SUCCESS);
        let subst: ParamSubst = Error(true, Parameter::Var(var_unset.clone()), None);
        let eval = subst.eval(&mut env, cfg);
        if let Err(RuntimeError::Expansion(ExpansionError::EmptyParameter(param, _))) = eval {
            assert_eq!(param, Parameter::Var(var_unset.clone()));
            assert_eq!(env.last_status(), EXIT_ERROR);
        } else {
            panic!("Unexpected evaluation: {:?}", eval);
        }


        // Non-strict with error message
        let subst: ParamSubst = Error(false, Parameter::Var(var.clone()), Some(err_msg));
        assert_eq!(subst.eval(&mut env, cfg), Ok(var_value.clone()));

        let subst: ParamSubst = Error(false, Parameter::Var(var_null.clone()), Some(err_msg));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        env.set_last_status(EXIT_SUCCESS);
        let subst: ParamSubst = Error(false, Parameter::Var(var_unset.clone()), Some(err_msg));
        assert_eq!(subst.eval(&mut env, cfg).as_ref(), Err(&err_unset));
        assert_eq!(env.last_status(), EXIT_ERROR);


        // Non-strict without error message
        let subst: ParamSubst = Error(false, Parameter::Var(var.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(var_value.clone()));

        let subst: ParamSubst = Error(false, Parameter::Var(var_null.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        env.set_last_status(EXIT_SUCCESS);
        let subst: ParamSubst = Error(false, Parameter::Var(var_unset.clone()), None);
        let eval = subst.eval(&mut env, cfg);
        if let Err(RuntimeError::Expansion(ExpansionError::EmptyParameter(param, _))) = eval {
            assert_eq!(param, Parameter::Var(var_unset.clone()));
            assert_eq!(env.last_status(), EXIT_ERROR);
        } else {
            panic!("Unexpected evaluation: {:?}", eval);
        }


        // Args have one non-null argument
        {
            let args = vec!(
                "".to_owned(),
                "foo".to_owned(),
                "".to_owned(),
                "".to_owned(),
            );

            let args_value = args.iter().cloned().map(Rc::new).collect::<Vec<_>>();
            let mut env = Env::with_config(EnvConfig {
                args: Some(args),
                .. Default::default()
            }).unwrap();

            let subst: ParamSubst = Error(true, Parameter::At, Some(err_msg));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::At(args_value.clone())));
            let subst: ParamSubst = Error(true, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::At(args_value.clone())));
            let subst: ParamSubst = Error(true, Parameter::Star, Some(err_msg));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Star(args_value.clone())));
            let subst: ParamSubst = Error(true, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Star(args_value.clone())));

            let subst: ParamSubst = Error(false, Parameter::At, Some(err_msg));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::At(args_value.clone())));
            let subst: ParamSubst = Error(false, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::At(args_value.clone())));
            let subst: ParamSubst = Error(false, Parameter::Star, Some(err_msg));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Star(args_value.clone())));
            let subst: ParamSubst = Error(false, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Star(args_value.clone())));
        }

        // Args all null
        {
            let mut env = Env::with_config(EnvConfig {
                args: Some(vec!(
                    "".to_owned(),
                    "".to_owned(),
                    "".to_owned(),
                )),
                .. Default::default()
            }).unwrap();

            env.set_last_status(EXIT_SUCCESS);
            let subst: ParamSubst = Error(true, Parameter::At, Some(err_msg));
            assert_eq!(subst.eval(&mut env, cfg).as_ref(), Err(&err_at));
            assert_eq!(env.last_status(), EXIT_ERROR);

            env.set_last_status(EXIT_SUCCESS);
            let subst: ParamSubst = Error(true, Parameter::At, None);
            let eval = subst.eval(&mut env, cfg);
            if let Err(RuntimeError::Expansion(ExpansionError::EmptyParameter(Parameter::At, _))) = eval {
                assert_eq!(env.last_status(), EXIT_ERROR);
            } else {
                panic!("Unexpected evaluation: {:?}", eval);
            }

            env.set_last_status(EXIT_SUCCESS);
            let subst: ParamSubst = Error(true, Parameter::Star, Some(err_msg));
            assert_eq!(subst.eval(&mut env, cfg).as_ref(), Err(&err_star));
            assert_eq!(env.last_status(), EXIT_ERROR);

            env.set_last_status(EXIT_SUCCESS);
            let subst: ParamSubst = Error(true, Parameter::Star, None);
            let eval = subst.eval(&mut env, cfg);
            if let Err(RuntimeError::Expansion(ExpansionError::EmptyParameter(Parameter::Star, _))) = eval {
                assert_eq!(env.last_status(), EXIT_ERROR);
            } else {
                panic!("Unexpected evaluation: {:?}", eval);
            }


            let subst: ParamSubst = Error(false, Parameter::At, Some(err_msg));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Error(false, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Error(false, Parameter::Star, Some(err_msg));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Error(false, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        }

        // Args not set
        {
            let mut env = Env::new().unwrap();

            env.set_last_status(EXIT_SUCCESS);
            let subst: ParamSubst = Error(true, Parameter::At, Some(err_msg));
            assert_eq!(subst.eval(&mut env, cfg).as_ref(), Err(&err_at));
            assert_eq!(env.last_status(), EXIT_ERROR);

            env.set_last_status(EXIT_SUCCESS);
            let subst: ParamSubst = Error(true, Parameter::At, None);
            let eval = subst.eval(&mut env, cfg);
            if let Err(RuntimeError::Expansion(ExpansionError::EmptyParameter(Parameter::At, _))) = eval {
                assert_eq!(env.last_status(), EXIT_ERROR);
            } else {
                panic!("Unexpected evaluation: {:?}", eval);
            }

            env.set_last_status(EXIT_SUCCESS);
            let subst: ParamSubst = Error(true, Parameter::Star, Some(err_msg));
            assert_eq!(subst.eval(&mut env, cfg).as_ref(), Err(&err_star));
            assert_eq!(env.last_status(), EXIT_ERROR);

            env.set_last_status(EXIT_SUCCESS);
            let subst: ParamSubst = Error(true, Parameter::Star, None);
            let eval = subst.eval(&mut env, cfg);
            if let Err(RuntimeError::Expansion(ExpansionError::EmptyParameter(Parameter::Star, _))) = eval {
                assert_eq!(env.last_status(), EXIT_ERROR);
            } else {
                panic!("Unexpected evaluation: {:?}", eval);
            }

            let subst: ParamSubst = Error(false, Parameter::At, Some(err_msg));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Error(false, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Error(false, Parameter::Star, Some(err_msg));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Error(false, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        }
    }

    #[test]
    fn test_eval_parameter_substitution_alternative() {
        use syntax::ast::ParameterSubstitution::Alternative;

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: false,
        };

        let var       = "non_empty_var".to_owned();
        let var_value = "foobar".to_owned();
        let var_null  = "var with empty value".to_owned();
        let null      = "".to_owned();
        let var_unset = "var_not_set_in_env".to_owned();

        let alt_value = "some alternative value";
        let alternative = MockSubstWord(alt_value);

        let mut env = Env::new().unwrap();
        env.set_var(var.clone(),      Rc::new(var_value.clone()));
        env.set_var(var_null.clone(), Rc::new(null.clone()));

        let alt_value = Fields::Single(Rc::new(alt_value.to_owned()));

        // Strict with alternative
        let subst: ParamSubst = Alternative(true, Parameter::Var(var.clone()), Some(alternative));
        assert_eq!(subst.eval(&mut env, cfg), Ok(alt_value.clone()));
        let subst: ParamSubst = Alternative(true, Parameter::Var(var_null.clone()), Some(alternative));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        let subst: ParamSubst = Alternative(true, Parameter::Var(var_unset.clone()), Some(alternative));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        // Strict without alternative
        let subst: ParamSubst = Alternative(true, Parameter::Var(var.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        let subst: ParamSubst = Alternative(true, Parameter::Var(var_null.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        let subst: ParamSubst = Alternative(true, Parameter::Var(var_unset.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));


        // Non-strict with alternative
        let subst: ParamSubst = Alternative(false, Parameter::Var(var.clone()), Some(alternative));
        assert_eq!(subst.eval(&mut env, cfg), Ok(alt_value.clone()));
        let subst: ParamSubst = Alternative(false, Parameter::Var(var_null.clone()), Some(alternative));
        assert_eq!(subst.eval(&mut env, cfg), Ok(alt_value.clone()));
        let subst: ParamSubst = Alternative(false, Parameter::Var(var_unset.clone()), Some(alternative));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        // Non-strict without alternative
        let subst: ParamSubst = Alternative(false, Parameter::Var(var.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        let subst: ParamSubst = Alternative(false, Parameter::Var(var_null.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        let subst: ParamSubst = Alternative(false, Parameter::Var(var_unset.clone()), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));


        // Args have one non-null argument
        {
            let args = vec!(
                "".to_owned(),
                "foo".to_owned(),
                "".to_owned(),
                "".to_owned(),
            );

            let mut env = Env::with_config(EnvConfig {
                args: Some(args),
                .. Default::default()
            }).unwrap();

            let subst: ParamSubst = Alternative(true, Parameter::At, Some(alternative));
            assert_eq!(subst.eval(&mut env, cfg), Ok(alt_value.clone()));
            let subst: ParamSubst = Alternative(true, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Alternative(true, Parameter::Star, Some(alternative));
            assert_eq!(subst.eval(&mut env, cfg), Ok(alt_value.clone()));
            let subst: ParamSubst = Alternative(true, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

            let subst: ParamSubst = Alternative(false, Parameter::At, Some(alternative));
            assert_eq!(subst.eval(&mut env, cfg), Ok(alt_value.clone()));
            let subst: ParamSubst = Alternative(false, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Alternative(false, Parameter::Star, Some(alternative));
            assert_eq!(subst.eval(&mut env, cfg), Ok(alt_value.clone()));
            let subst: ParamSubst = Alternative(false, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        }

        // Args all null
        {
            let mut env = Env::with_config(EnvConfig {
                args: Some(vec!(
                    "".to_owned(),
                    "".to_owned(),
                    "".to_owned(),
                )),
                .. Default::default()
            }).unwrap();

            let subst: ParamSubst = Alternative(true, Parameter::At, Some(alternative));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Alternative(true, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Alternative(true, Parameter::Star, Some(alternative));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Alternative(true, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

            let subst: ParamSubst = Alternative(false, Parameter::At, Some(alternative));
            assert_eq!(subst.eval(&mut env, cfg), Ok(alt_value.clone()));
            let subst: ParamSubst = Alternative(false, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Alternative(false, Parameter::Star, Some(alternative));
            assert_eq!(subst.eval(&mut env, cfg), Ok(alt_value.clone()));
            let subst: ParamSubst = Alternative(false, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        }

        // Args not set
        {
            let mut env = Env::new().unwrap();

            let subst: ParamSubst = Alternative(true, Parameter::At, Some(alternative));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Alternative(true, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Alternative(true, Parameter::Star, Some(alternative));
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Alternative(true, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

            let subst: ParamSubst = Alternative(false, Parameter::At, Some(alternative));
            assert_eq!(subst.eval(&mut env, cfg), Ok(alt_value.clone()));
            let subst: ParamSubst = Alternative(false, Parameter::At, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
            let subst: ParamSubst = Alternative(false, Parameter::Star, Some(alternative));
            assert_eq!(subst.eval(&mut env, cfg), Ok(alt_value.clone()));
            let subst: ParamSubst = Alternative(false, Parameter::Star, None);
            assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        }
    }

    #[test]
    fn test_eval_parameter_substitution_splitting_default_ifs() {
        use syntax::ast::ParameterSubstitution::*;

        let val1 = " \t\nfoo \t\nbar \t\n";
        let val2 = "";

        let mock1 = MockSubstWord(val1);
        let mock2 = MockSubstWord(val2);

        let val1 = val1.to_owned();
        let val2 = val2.to_owned();

        let args = vec!(
            val1.clone(),
            val2.clone(),
        );

        let val1 = Rc::new(val1);
        let val2 = Rc::new(val2);
        let var1 = Parameter::Var("var1".to_owned());
        let var2 = Parameter::Var("var2".to_owned());

        let var_null = var2.clone();

        let mut env = Env::with_config(EnvConfig {
            args: Some(args.clone()),
            .. ::std::default::Default::default()
        }).unwrap();
        env.set_var("var1".to_owned(), val1.clone());
        env.set_var("var2".to_owned(), val2.clone());

        // Splitting should NOT keep empty fields between IFS chars which ARE whitespace
        let fields_foo_bar = Fields::Split(vec!(
            "foo".to_owned().into(),
            "bar".to_owned().into(),
        ));

        // With splitting
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: true,
        };

        let subst: ParamSubst = Default(false, var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = Default(false, var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = Assign(false, var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = Assign(false, var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = Error(false, var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = Error(false, var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = Alternative(false, var_null.clone(), Some(mock1));
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = Alternative(false, var_null.clone(), Some(mock2));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = RemoveSmallestSuffix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = RemoveSmallestSuffix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = RemoveLargestSuffix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = RemoveLargestSuffix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = RemoveSmallestPrefix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = RemoveSmallestPrefix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = RemoveLargestPrefix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = RemoveLargestPrefix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        // Without splitting

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: false,
        };

        let val1 = Fields::Single(val1.clone());
        let val2 = Fields::Zero;

        let subst: ParamSubst = Default(false, var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = Default(false, var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));

        let subst: ParamSubst = Assign(false, var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = Assign(false, var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));

        let subst: ParamSubst = Error(false, var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = Error(false, var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));

        let subst: ParamSubst = Alternative(false, var_null.clone(), Some(mock1));
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = Alternative(false, var_null.clone(), Some(mock2));
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));

        let subst: ParamSubst = RemoveSmallestSuffix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = RemoveSmallestSuffix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));

        let subst: ParamSubst = RemoveLargestSuffix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = RemoveLargestSuffix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));

        let subst: ParamSubst = RemoveSmallestPrefix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = RemoveSmallestPrefix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));

        let subst: ParamSubst = RemoveLargestPrefix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = RemoveLargestPrefix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));
    }

    #[test]
    fn test_eval_parameter_substitution_splitting_with_custom_ifs() {
        use syntax::ast::ParameterSubstitution::*;

        let val1 = "   foo000bar   ";
        let val2 = "  00 0 00  0 ";
        let val3 = "";

        let mock1 = MockSubstWord(val1);
        let mock2 = MockSubstWord(val2);
        let mock3 = MockSubstWord(val3);

        let val1 = val1.to_owned();
        let val2 = val2.to_owned();
        let val3 = val3.to_owned();

        let args = vec!(
            val1.clone(),
            val2.clone(),
            val3.clone(),
        );

        let val1 = Rc::new(val1);
        let val2 = Rc::new(val2);
        let val3 = Rc::new(val3);
        let var1 = Parameter::Var("var1".to_owned());
        let var2 = Parameter::Var("var2".to_owned());
        let var3 = Parameter::Var("var3".to_owned());

        let var_null = var3.clone();
        let var_missing = Parameter::Var("missing".to_owned());

        let mut env = Env::with_config(EnvConfig {
            args: Some(args.clone()),
            .. ::std::default::Default::default()
        }).unwrap();
        env.set_var("IFS".to_owned(), Rc::new("0 ".to_owned()));

        env.set_var("var1".to_owned(), val1.clone());
        env.set_var("var2".to_owned(), val2.clone());
        env.set_var("var3".to_owned(), val3.clone());

        // Splitting SHOULD keep empty fields between IFS chars which are NOT whitespace
        let fields_foo_bar = Fields::Split(vec!(
            "foo".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
            "bar".to_owned().into(),
        ));

        let fields_all_blanks = Fields::Split(vec!(
            "".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
            "".to_owned().into(),
        ));

        // With splitting
        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: true,
        };

        let subst: ParamSubst = Len(var_missing.clone());
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Single("".to_owned().into())));

        let subst: ParamSubst = Arith(Some(Arithmetic::Add(
            Box::new(Arithmetic::Literal(100)),
            Box::new(Arithmetic::Literal(5)),
        )));
        assert_eq!(subst.eval(&mut env, cfg), Ok(
            Fields::Split(vec!(
                "1".to_owned().into(),
                "5".to_owned().into(),
            )))
        );

        let subst: ParamSubst = Default(false, var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = Default(false, var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_all_blanks.clone()));
        let subst: ParamSubst = Default(false, var3.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = Assign(false, var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = Assign(false, var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_all_blanks.clone()));
        let subst: ParamSubst = Assign(false, var3.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = Error(false, var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = Error(false, var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_all_blanks.clone()));
        let subst: ParamSubst = Error(false, var3.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = Alternative(false, var_null.clone(), Some(mock1));
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = Alternative(false, var_null.clone(), Some(mock2));
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_all_blanks.clone()));
        let subst: ParamSubst = Alternative(false, var_null.clone(), Some(mock3));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = RemoveSmallestSuffix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = RemoveSmallestSuffix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_all_blanks.clone()));
        let subst: ParamSubst = RemoveSmallestSuffix(var3.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = RemoveLargestSuffix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = RemoveLargestSuffix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_all_blanks.clone()));
        let subst: ParamSubst = RemoveLargestSuffix(var3.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = RemoveSmallestPrefix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = RemoveSmallestPrefix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_all_blanks.clone()));
        let subst: ParamSubst = RemoveSmallestPrefix(var3.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = RemoveLargestPrefix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_foo_bar.clone()));
        let subst: ParamSubst = RemoveLargestPrefix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(fields_all_blanks.clone()));
        let subst: ParamSubst = RemoveLargestPrefix(var3.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        // Without splitting

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: false,
        };

        let subst: ParamSubst = Len(var_missing.clone());
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Single("0".to_owned().into())));

        let subst: ParamSubst = Arith(Some(Arithmetic::Add(
            Box::new(Arithmetic::Literal(100)),
            Box::new(Arithmetic::Literal(5)),
        )));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Single("105".to_owned().into())));

        let val1 = Fields::Single(val1.clone());
        let val2 = Fields::Single(val2.clone());
        let val3 = Fields::Zero;

        let subst: ParamSubst = Default(false, var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = Default(false, var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));
        let subst: ParamSubst = Default(false, var3.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val3.clone()));

        let subst: ParamSubst = Assign(false, var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = Assign(false, var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));
        let subst: ParamSubst = Assign(false, var3.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val3.clone()));

        let subst: ParamSubst = Error(false, var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = Error(false, var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));
        let subst: ParamSubst = Error(false, var3.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val3.clone()));

        let subst: ParamSubst = Alternative(false, var_null.clone(), Some(mock1));
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = Alternative(false, var_null.clone(), Some(mock2));
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));
        let subst: ParamSubst = Alternative(false, var_null.clone(), Some(mock3));
        assert_eq!(subst.eval(&mut env, cfg), Ok(val3.clone()));

        let subst: ParamSubst = RemoveSmallestSuffix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = RemoveSmallestSuffix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));
        let subst: ParamSubst = RemoveSmallestSuffix(var3.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val3.clone()));

        let subst: ParamSubst = RemoveLargestSuffix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = RemoveLargestSuffix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));
        let subst: ParamSubst = RemoveLargestSuffix(var3.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val3.clone()));

        let subst: ParamSubst = RemoveSmallestPrefix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = RemoveSmallestPrefix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));
        let subst: ParamSubst = RemoveSmallestPrefix(var3.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val3.clone()));

        let subst: ParamSubst = RemoveLargestPrefix(var1.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val1.clone()));
        let subst: ParamSubst = RemoveLargestPrefix(var2.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val2.clone()));
        let subst: ParamSubst = RemoveLargestPrefix(var3.clone(), None);
        assert_eq!(subst.eval(&mut env, cfg), Ok(val3.clone()));
    }

    #[test]
    fn test_eval_parameter_substitution_remove_smallest_suffix() {
        use syntax::ast::ParameterSubstitution::RemoveSmallestSuffix;

        let args = vec!(
            "foobar".to_owned(),
            "foobaar".to_owned(),
            "bazbaar".to_owned(),
            "def".to_owned(),
            "".to_owned(),
        );

        let foobar  = Parameter::Positional(1);
        let null    = Parameter::Positional(5);
        let unset   = Parameter::Positional(6);

        let pat = MockSubstWord("a*");

        let fields_args = vec!(
            "foob".to_owned().into(),
            "fooba".to_owned().into(),
            "bazba".to_owned().into(),
            "def".to_owned().into(),
            "".to_owned().into(),
        );

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: false,
        };

        let mut env = Env::with_config(EnvConfig {
            args: Some(args),
            .. Default::default()
        }).unwrap();

        let subst: ParamSubst = RemoveSmallestSuffix(foobar, None);
        assert_eq!(subst.eval(&mut env, cfg), Ok("foobar".to_owned().into()));

        let subst: ParamSubst = RemoveSmallestSuffix(unset, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        let subst: ParamSubst = RemoveSmallestSuffix(null, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = RemoveSmallestSuffix(Parameter::At, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::At(fields_args.clone())));
        let subst: ParamSubst = RemoveSmallestSuffix(Parameter::Star, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Star(fields_args.clone())));
    }

    #[test]
    fn test_eval_parameter_substitution_remove_largest_suffix() {
        use syntax::ast::ParameterSubstitution::RemoveLargestSuffix;

        let args = vec!(
            "foobar".to_owned(),
            "foobaar".to_owned(),
            "bazbaar".to_owned(),
            "def".to_owned(),
            "".to_owned(),
        );

        let foobar  = Parameter::Positional(1);
        let null    = Parameter::Positional(5);
        let unset   = Parameter::Positional(6);

        let pat = MockSubstWord("a*");

        let fields_args = vec!(
            "foob".to_owned().into(),
            "foob".to_owned().into(),
            "b".to_owned().into(),
            "def".to_owned().into(),
            "".to_owned().into(),
        );

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: false,
        };

        let mut env = Env::with_config(EnvConfig {
            args: Some(args),
            .. Default::default()
        }).unwrap();

        let subst: ParamSubst = RemoveLargestSuffix(foobar, None);
        assert_eq!(subst.eval(&mut env, cfg), Ok("foobar".to_owned().into()));

        let subst: ParamSubst = RemoveLargestSuffix(unset, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        let subst: ParamSubst = RemoveLargestSuffix(null, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = RemoveLargestSuffix(Parameter::At, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::At(fields_args.clone())));
        let subst: ParamSubst = RemoveLargestSuffix(Parameter::Star, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Star(fields_args.clone())));
    }

    #[test]
    fn test_eval_parameter_substitution_remove_smallest_prefix() {
        use syntax::ast::ParameterSubstitution::RemoveSmallestPrefix;

        let args = vec!(
            "foobar".to_owned(),
            "foofoo".to_owned(),
            "bazfooqux".to_owned(),
            "abc".to_owned(),
            "".to_owned(),
        );

        let foobar  = Parameter::Positional(1);
        let abc     = Parameter::Positional(4);
        let null    = Parameter::Positional(5);
        let unset   = Parameter::Positional(6);

        let pat = MockSubstWord("*o");

        let fields_args = vec!(
            "obar".to_owned().into(),
            "ofoo".to_owned().into(),
            "oqux".to_owned().into(),
            "abc".to_owned().into(),
            "".to_owned().into(),
        );

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: false,
        };

        let mut env = Env::with_config(EnvConfig {
            args: Some(args),
            .. Default::default()
        }).unwrap();

        let subst: ParamSubst = RemoveSmallestPrefix(foobar, None);
        assert_eq!(subst.eval(&mut env, cfg), Ok("foobar".to_owned().into()));

        let subst: ParamSubst = RemoveSmallestPrefix(abc, Some(MockSubstWord("abc")));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = RemoveSmallestPrefix(unset, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        let subst: ParamSubst = RemoveSmallestPrefix(null, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = RemoveSmallestPrefix(Parameter::At, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::At(fields_args.clone())));
        let subst: ParamSubst = RemoveSmallestPrefix(Parameter::Star, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Star(fields_args.clone())));
    }

    #[test]
    fn test_eval_parameter_substitution_remove_largest_prefix() {
        use syntax::ast::ParameterSubstitution::RemoveLargestPrefix;

        let args = vec!(
            "foobar".to_owned(),
            "foofoo".to_owned(),
            "bazfooqux".to_owned(),
            "".to_owned(),
        );

        let foobar  = Parameter::Positional(1);
        let null    = Parameter::Positional(4);
        let unset   = Parameter::Positional(5);

        let pat = MockSubstWord("*o");

        let fields_args = vec!(
            "bar".to_owned().into(),
            "".to_owned().into(),
            "qux".to_owned().into(),
            "".to_owned().into(),
        );

        let cfg = WordEvalConfig {
            tilde_expansion: TildeExpansion::None,
            split_fields_further: false,
        };

        let mut env = Env::with_config(EnvConfig {
            args: Some(args),
            .. Default::default()
        }).unwrap();

        let subst: ParamSubst = RemoveLargestPrefix(foobar, None);
        assert_eq!(subst.eval(&mut env, cfg), Ok("foobar".to_owned().into()));

        let subst: ParamSubst = RemoveLargestPrefix(unset, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));
        let subst: ParamSubst = RemoveLargestPrefix(null, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Zero));

        let subst: ParamSubst = RemoveLargestPrefix(Parameter::At, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::At(fields_args.clone())));
        let subst: ParamSubst = RemoveLargestPrefix(Parameter::Star, Some(pat));
        assert_eq!(subst.eval(&mut env, cfg), Ok(Fields::Star(fields_args.clone())));
    }

    #[test]
    fn test_eval_parameter_substitution_forwards_tilde_expansion() {
        use syntax::ast::ParameterSubstitution::*;
        use runtime::Result;

        #[derive(Copy, Clone, Debug)]
        struct MockWord(TildeExpansion);

        impl<E: Environment> WordEval<E> for MockWord {
            fn eval_with_config(&self, _: &mut E, cfg: WordEvalConfig) -> Result<Fields>
            {
                assert_eq!(self.0, cfg.tilde_expansion);
                assert_eq!(cfg.split_fields_further, false);
                Ok(Fields::Zero)
            }
        }

        type ParamSubst = ParameterSubstitution<MockWord, MockCmd>;

        let name = "var";
        let var = Parameter::Var(name.to_owned());
        let mut env = Env::new().unwrap();

        let cases = vec!(TildeExpansion::None, TildeExpansion::First, TildeExpansion::All);
        for tilde_expansion in cases {
            let cfg = WordEvalConfig {
                tilde_expansion: tilde_expansion,
                split_fields_further: true, // Should not affect inner word
            };

            let mock = MockWord(tilde_expansion);

            env.unset_var(name);
            let subst: ParamSubst = Default(true, var.clone(), Some(mock));
            subst.eval(&mut env, cfg).unwrap();

            env.unset_var(name);
            let subst: ParamSubst = Assign(true, var.clone(), Some(mock));
            subst.eval(&mut env, cfg).unwrap();

            env.unset_var(name);
            let subst: ParamSubst = Error(true, var.clone(), Some(mock));
            subst.eval(&mut env, cfg).unwrap_err();

            env.set_var(name.to_owned(), "some value".to_owned().into());
            let subst: ParamSubst = Alternative(true, var.clone(), Some(mock));
            subst.eval(&mut env, cfg).unwrap();
        }
    }
}
