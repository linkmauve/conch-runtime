use {ExitStatus, Spawn};
use env::{FileDescEnvironment, LastStatusEnvironment, SubEnvironment};
use error::IsFatalError;
use future::{Async, EnvFuture, Poll};
use futures::future::{Either, Future, FutureResult, ok};
use spawn::{Sequence, sequence};
use std::fmt;
use void::Void;

/// A future that represents the sequential execution of commands in a subshell
/// environment.
///
/// Commands are sequentially executed regardless of the exit status of
/// previous commands. All errors are reported and swallowed.
#[must_use = "futures do nothing unless polled"]
pub struct Subshell<E, I>
    where I: Iterator,
          I::Item: Spawn<E>,
{
    env: E,
    inner: Sequence<E, I>,
}

impl<E, I> fmt::Debug for Subshell<E, I>
    where E: fmt::Debug,
          I: Iterator + fmt::Debug,
          I::Item: Spawn<E> + fmt::Debug,
          <I::Item as Spawn<E>>::EnvFuture: fmt::Debug,
          <I::Item as Spawn<E>>::Future: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Subshell")
            .field("env", &self.env)
            .field("inner", &self.inner)
            .finish()
    }
}

impl<E, I, S> Future for Subshell<E, I>
    where E: FileDescEnvironment + LastStatusEnvironment,
          I: Iterator<Item = S>,
          S: Spawn<E>,
          S::Error: IsFatalError,
{
    type Item = Either<S::Future, FutureResult<ExitStatus, S::Error>>;
    type Error = Void;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.inner.poll(&mut self.env) {
            Ok(Async::Ready(exit)) => Ok(Async::Ready(exit)),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(err) => {
                self.env.report_error(&err);
                let exit = self.env.last_status();
                debug_assert_eq!(exit.success(), false);
                Ok(Async::Ready(Either::B(ok(exit))))
            },
        }
    }
}

/// Spawns any iterable collection of sequential items as if they were running
/// in a subshell environment.
///
/// The `env` parameter will be copied as a `SubEnvironment`, in whose context
/// the commands will be executed.
pub fn subshell<I, E: ?Sized>(iter: I, env: &E) -> Subshell<E, I::IntoIter>
    where I: IntoIterator,
          I::Item: Spawn<E>,
          E: FileDescEnvironment + LastStatusEnvironment + SubEnvironment,
{
    Subshell {
        env: env.sub_env(),
        inner: sequence(iter),
    }
}
