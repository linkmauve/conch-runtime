use {CANCELLED_TWICE, POLLED_TWICE};
use conch_parser::ast;
use conch_parser::ast::ParameterSubstitution::*;
use env::{AsyncIoEnvironment, FileDescEnvironment, FileDescOpener, LastStatusEnvironment,
          ReportErrorEnvironment, StringWrapper, SubEnvironment, VariableEnvironment};
use error::{ExpansionError, IsFatalError};
use future::{Async, EnvFuture, Poll};
use futures::Future;
use eval::{Alternative, ArithEval, Assign, EvalDefault, Error, Fields, ParamEval,
           RemoveLargestPrefix, RemoveLargestSuffix, RemoveSmallestPrefix,
           RemoveSmallestSuffix, Split, WordEval, WordEvalConfig, alternative,
           assign, default, error, len, remove_largest_prefix, remove_largest_suffix,
           remove_smallest_prefix, remove_smallest_suffix, split};
use spawn::{substitution, Spawn, Substitution, SubstitutionEnvFuture};
use std::fmt;
use std::io::Error as IoError;
use std::slice;
use std::vec;
use tokio_io::AsyncRead;

impl<T, P, W, C, A, E> WordEval<E> for ast::ParameterSubstitution<P, W, C, A>
    where T: StringWrapper,
          P: ParamEval<E, EvalResult = T> + fmt::Display,
          W: WordEval<E, EvalResult = T>,
          W::Error: From<ExpansionError> + From<C::Error>,
          C: Spawn<E>,
          C::Error: IsFatalError + From<IoError>,
          A: ArithEval<E>,
          E: AsyncIoEnvironment
              + FileDescEnvironment
              + FileDescOpener
              + LastStatusEnvironment
              + ReportErrorEnvironment
              + SubEnvironment
              + VariableEnvironment<VarName = T, Var = T>,
          E::FileHandle: From<E::OpenedFileHandle>,
          E::IoHandle: From<E::OpenedFileHandle>,
          E::Read: AsyncRead,
{
    type EvalResult = T;
    type EvalFuture = ParameterSubstitution<T, W::EvalFuture, vec::IntoIter<C>, A, E, E::Read>;
    type Error = W::Error;

    /// Evaluates a parameter subsitution in the context of some environment,
    /// optionally splitting fields.
    ///
    /// Note: even if the caller specifies no splitting should be done,
    /// multiple fields can occur if `$@` or `$*` is evaluated.
    fn eval_with_config(self, env: &E, cfg: WordEvalConfig) -> Self::EvalFuture {
        let te = cfg.tilde_expansion;

        let inner = match self {
            Command(body) => Inner::CommandInit(substitution(body)),
            Len(ref p) => Inner::Len(Some(len(p, env))),
            Arith(a) => Inner::Arith(a),
            Default(strict, p, def) => Inner::Default(default(strict, &p, def, env, te)),
            Assign(strict, p, assig) => Inner::Assign(assign(strict, &p, assig, env, te)),
            Error(strict, p, msg) => Inner::Error(error(strict, &p, msg, env, te)),
            Alternative(strict, p, al) => Inner::Alternative(alternative(strict, &p, al, env, te)),
            RemoveSmallestSuffix(p, pat) =>
                Inner::RemoveSmallestSuffix(remove_smallest_suffix(&p, pat, env)),
            RemoveLargestSuffix(p, pat) =>
                Inner::RemoveLargestSuffix(remove_largest_suffix(&p, pat, env)),
            RemoveSmallestPrefix(p, pat) =>
                Inner::RemoveSmallestPrefix(remove_smallest_prefix(&p, pat, env)),
            RemoveLargestPrefix(p, pat) =>
                Inner::RemoveLargestPrefix(remove_largest_prefix(&p, pat, env)),
        };

        ParameterSubstitution {
            inner: split(cfg.split_fields_further, inner),
        }
    }
}

impl<'a, T, P, W, C, A, E> WordEval<E> for &'a ast::ParameterSubstitution<P, W, C, A>
    where T: StringWrapper,
          P: ParamEval<E, EvalResult = T> + fmt::Display,
          &'a W: WordEval<E, EvalResult = T>,
          <&'a W as WordEval<E>>::Error: From<ExpansionError> + From<<&'a C as Spawn<E>>::Error>,
          &'a C: Spawn<E>,
          <&'a C as Spawn<E>>::Error: IsFatalError + From<IoError>,
          A: ArithEval<E>,
          E: AsyncIoEnvironment
              + FileDescEnvironment
              + FileDescOpener
              + LastStatusEnvironment
              + ReportErrorEnvironment
              + SubEnvironment
              + VariableEnvironment<VarName = T, Var = T>,
          E::FileHandle: From<E::OpenedFileHandle>,
          E::IoHandle: From<E::OpenedFileHandle>,
          E::Read: AsyncRead,
{
    type EvalResult = T;
    #[cfg_attr(feature = "cargo-clippy", allow(type_complexity))]
    type EvalFuture = ParameterSubstitution<
        T,
        <&'a W as WordEval<E>>::EvalFuture,
        slice::Iter<'a, C>,
        &'a A,
        E,
        E::Read
    >;
    type Error = <&'a W as WordEval<E>>::Error;

    /// Evaluates a parameter subsitution in the context of some environment,
    /// optionally splitting fields.
    ///
    /// Note: even if the caller specifies no splitting should be done,
    /// multiple fields can occur if `$@` or `$*` is evaluated.
    fn eval_with_config(self, env: &E, cfg: WordEvalConfig) -> Self::EvalFuture {
        let te = cfg.tilde_expansion;

        let inner = match *self {
            Command(ref body) => Inner::CommandInit(substitution(body)),
            Len(ref p) => Inner::Len(Some(len(p, env))),
            Arith(ref a) => Inner::Arith(a.as_ref()),
            Default(strict, ref p, ref def) =>
                Inner::Default(default(strict, p, def.as_ref(), env, te)),
            Assign(strict, ref p, ref assig) =>
                Inner::Assign(assign(strict, p, assig.as_ref(), env, te)),
            Error(strict, ref p, ref msg) =>
                Inner::Error(error(strict, p, msg.as_ref(), env, te)),
            Alternative(strict, ref p, ref al) =>
                Inner::Alternative(alternative(strict, p, al.as_ref(), env, te)),
            RemoveSmallestSuffix(ref p, ref pat) =>
                Inner::RemoveSmallestSuffix(remove_smallest_suffix(p, pat.as_ref(), env)),
            RemoveLargestSuffix(ref p, ref pat) =>
                Inner::RemoveLargestSuffix(remove_largest_suffix(p, pat.as_ref(), env)),
            RemoveSmallestPrefix(ref p, ref pat) =>
                Inner::RemoveSmallestPrefix(remove_smallest_prefix(p, pat.as_ref(), env)),
            RemoveLargestPrefix(ref p, ref pat) =>
                Inner::RemoveLargestPrefix(remove_largest_prefix(p, pat.as_ref(), env)),
        };

        ParameterSubstitution {
            inner: split(cfg.split_fields_further, inner),
        }
    }
}

/// A future representing a `ParameterSubstitution` evaluation.
#[must_use = "futures do nothing unless polled"]
pub struct ParameterSubstitution<T, F, I, A, E, R>
    where I: Iterator,
          I::Item: Spawn<E>,
{
    inner: Split<Inner<T, F, I, A, E, R>>,
}

impl<T, I, A, E, R, S> fmt::Debug for ParameterSubstitution<T, S::Future, I, A, E, R>
    where T: fmt::Debug,
          I: Iterator<Item = S> + fmt::Debug,
          A: fmt::Debug,
          S: Spawn<E> + fmt::Debug,
          S::EnvFuture: fmt::Debug,
          S::Future: fmt::Debug,
          S::Error: fmt::Debug,
          E: fmt::Debug,
          R: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("ParameterSubstitution")
            .field("inner", &self.inner)
            .finish()
    }
}

#[must_use = "futures do nothing unless polled"]
enum Inner<T, F, I, A, E, R>
    where I: Iterator,
          I::Item: Spawn<E>,
{
    CommandInit(SubstitutionEnvFuture<I>),
    Command(Substitution<I, R, E>),
    Len(Option<T>),
    Arith(Option<A>),
    Default(EvalDefault<T, F>),
    Assign(Assign<T, F>),
    Error(Error<T, F>),
    Alternative(Alternative<F>),
    RemoveSmallestSuffix(RemoveSmallestSuffix<T, F>),
    RemoveLargestSuffix(RemoveLargestSuffix<T, F>),
    RemoveSmallestPrefix(RemoveSmallestPrefix<T, F>),
    RemoveLargestPrefix(RemoveLargestPrefix<T, F>),
    Gone,
}

impl<T, I, A, E, R, S> fmt::Debug for Inner<T, S::Future, I, A, E, R>
    where T: fmt::Debug,
          I: Iterator<Item = S> + fmt::Debug,
          A: fmt::Debug,
          S: Spawn<E> + fmt::Debug,
          S::EnvFuture: fmt::Debug,
          S::Future: fmt::Debug,
          S::Error: fmt::Debug,
          E: fmt::Debug,
          R: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Inner::CommandInit(ref inner) => {
                fmt.debug_tuple("Inner::CommandInit")
                    .field(inner)
                    .finish()
            },
            Inner::Command(ref inner) => {
                fmt.debug_tuple("Inner::Command")
                    .field(inner)
                    .finish()
            },
            Inner::Len(ref inner) => {
                fmt.debug_tuple("Inner::Len")
                    .field(inner)
                    .finish()
            },
            Inner::Arith(ref inner) => {
                fmt.debug_tuple("Inner::Arith")
                    .field(inner)
                    .finish()
            },
            Inner::Default(ref inner) => {
                fmt.debug_tuple("Inner::Default")
                    .field(inner)
                    .finish()
            },
            Inner::Assign(ref inner) => {
                fmt.debug_tuple("Inner::Assign")
                    .field(inner)
                    .finish()
            },
            Inner::Error(ref inner) => {
                fmt.debug_tuple("Inner::Error")
                    .field(inner)
                    .finish()
            },
            Inner::Alternative(ref inner) => {
                fmt.debug_tuple("Inner::Alternative")
                    .field(inner)
                    .finish()
            },
            Inner::RemoveSmallestSuffix(ref inner) => {
                fmt.debug_tuple("Inner::RemoveSmallestSuffix")
                    .field(inner)
                    .finish()
            },
            Inner::RemoveLargestSuffix(ref inner) => {
                fmt.debug_tuple("Inner::RemoveLargestSuffix")
                    .field(inner)
                    .finish()
            },
            Inner::RemoveSmallestPrefix(ref inner) => {
                fmt.debug_tuple("Inner::RemoveSmallestPrefix")
                    .field(inner)
                    .finish()
            },
            Inner::RemoveLargestPrefix(ref inner) => {
                fmt.debug_tuple("Inner::RemoveLargestPrefix")
                    .field(inner)
                    .finish()
            },
            Inner::Gone => {
                fmt.debug_tuple("Inner::Gone")
                    .finish()
            },
        }
    }
}

impl<T, F, I, A, E> EnvFuture<E> for ParameterSubstitution<T, F, I, A, E, E::Read>
    where T: StringWrapper,
          F: EnvFuture<E, Item = Fields<T>>,
          F::Error: From<::error::ExpansionError> + From<<I::Item as Spawn<E>>::Error>,
          I: Iterator,
          I::Item: Spawn<E>,
          <I::Item as Spawn<E>>::Error: IsFatalError + From<IoError>,
          A: ArithEval<E>,
          E: AsyncIoEnvironment
              + FileDescEnvironment
              + FileDescOpener
              + LastStatusEnvironment
              + ReportErrorEnvironment
              + SubEnvironment
              + VariableEnvironment<VarName = T, Var = T>,
          E::FileHandle: From<E::OpenedFileHandle>,
          E::IoHandle: From<E::OpenedFileHandle>,
{
    type Item = Fields<T>;
    type Error = F::Error;

    fn poll(&mut self, env: &mut E) -> Poll<Self::Item, Self::Error> {
        self.inner.poll(env)
    }

    fn cancel(&mut self, env: &mut E) {
        self.inner.cancel(env)
    }
}

impl<T, F, I, A, E> EnvFuture<E> for Inner<T, F, I, A, E, E::Read>
    where T: StringWrapper,
          F: EnvFuture<E, Item = Fields<T>>,
          F::Error: From<::error::ExpansionError> + From<<I::Item as Spawn<E>>::Error>,
          I: Iterator,
          I::Item: Spawn<E>,
          <I::Item as Spawn<E>>::Error: IsFatalError + From<IoError>,
          A: ArithEval<E>,
          E: AsyncIoEnvironment
            + FileDescEnvironment
            + FileDescOpener
            + LastStatusEnvironment
            + ReportErrorEnvironment
            + SubEnvironment
            + VariableEnvironment<VarName = T, Var = T>,
          E::FileHandle: From<E::OpenedFileHandle>,
          E::IoHandle: From<E::OpenedFileHandle>,
{
    type Item = Fields<T>;
    type Error = F::Error;

    fn poll(&mut self, env: &mut E) -> Poll<Self::Item, Self::Error> {
        loop {
            let next_state = match *self {
                Inner::CommandInit(ref mut f) => Inner::Command(try_ready!(f.poll(env))),
                Inner::Command(ref mut f) => {
                    let ret: String = try_ready!(f.poll());
                    return Ok(Async::Ready(Fields::from(T::from(ret))));
                },

                Inner::Len(ref mut len) => {
                    let len = len.take().expect("polled twice");
                    return Ok(Async::Ready(Fields::Single(len)));
                },

                Inner::Arith(ref a) => {
                    let ret = match a.as_ref() {
                        Some(a) => try!(a.eval(env)),
                        None => 0,
                    };

                    return Ok(Async::Ready(Fields::Single(ret.to_string().into())));
                },

                Inner::Default(ref mut f)              => return f.poll(env),
                Inner::Assign(ref mut f)               => return f.poll(env),
                Inner::Error(ref mut f)                => return f.poll(env),
                Inner::Alternative(ref mut f)          => return f.poll(env),
                Inner::RemoveSmallestSuffix(ref mut f) => return f.poll(env),
                Inner::RemoveLargestSuffix(ref mut f)  => return f.poll(env),
                Inner::RemoveSmallestPrefix(ref mut f) => return f.poll(env),
                Inner::RemoveLargestPrefix(ref mut f)  => return f.poll(env),
                Inner::Gone => panic!(POLLED_TWICE),
            };

            *self = next_state;
        }
    }

    fn cancel(&mut self, env: &mut E) {
        match *self {
            Inner::Len(_) |
            Inner::Arith(_) |
            Inner::Command(_) => {},

            Inner::CommandInit(ref mut f)          => f.cancel(env),
            Inner::Default(ref mut f)              => f.cancel(env),
            Inner::Assign(ref mut f)               => f.cancel(env),
            Inner::Error(ref mut f)                => f.cancel(env),
            Inner::Alternative(ref mut f)          => f.cancel(env),
            Inner::RemoveSmallestSuffix(ref mut f) => f.cancel(env),
            Inner::RemoveLargestSuffix(ref mut f)  => f.cancel(env),
            Inner::RemoveSmallestPrefix(ref mut f) => f.cancel(env),
            Inner::RemoveLargestPrefix(ref mut f)  => f.cancel(env),
            Inner::Gone => panic!(CANCELLED_TWICE),
        };

        *self = Inner::Gone;
    }
}
