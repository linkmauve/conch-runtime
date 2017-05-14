use {EXIT_SUCCESS, ExitStatus};
use error::IsFatalError;
use env::{LastStatusEnvironment, ReportErrorEnvironment};
use future::{Async, EnvFuture, Poll};
use futures::future::Future;
use spawn::{ExitResult, GuardBodyPair, SpawnRef, SwallowNonFatal, swallow_non_fatal_errors,
            VecSequence};
use std::fmt;
use std::mem;

/// Spawns a loop command such as `While` or `Until` using a guard and a body.
///
/// The guard will be repeatedly executed and its exit status used to determine
/// if the loop should be broken, or if the body should be executed. If
/// `invert_guard_status == false`, the loop will continue as long as the guard
/// exits successfully. If `invert_guard_status == true`, the loop will continue
/// **until** the guard exits successfully.
///
/// Any nonfatal errors will be swallowed and reported. Fatal errors will be
/// propagated to the caller.
// FIXME: implement a `break` built in command to break loops
pub fn loop_cmd<S, E: ?Sized>(
    invert_guard_status: bool,
    guard_body_pair: GuardBodyPair<Vec<S>>,
) -> Loop<S, E> where S: SpawnRef<E>,
{
    Loop {
        invert_guard_status: invert_guard_status,
        guard: guard_body_pair.guard,
        body: guard_body_pair.body,
        has_run_body: false,
        state: State::Init,
    }
}

/// A future representing the execution of a loop (e.g. `while`/`until`) command.
#[must_use = "futures do nothing unless polled"]
pub struct Loop<S, E: ?Sized> where S: SpawnRef<E>
{
    invert_guard_status: bool,
    guard: Vec<S>,
    body: Vec<S>,
    has_run_body: bool,
    state: State<VecSequence<S, E>, S::Future>,
}

impl<S, E: ?Sized> fmt::Debug for Loop<S, E>
    where S: SpawnRef<E> + fmt::Debug,
          S::EnvFuture: fmt::Debug,
          S::Future: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Loop")
            .field("invert_guard_status", &self.invert_guard_status)
            .field("guard", &self.guard)
            .field("body", &self.body)
            .field("has_run_body", &self.has_run_body)
            .field("state", &self.state)
            .finish()
    }
}

#[derive(Debug)]
enum State<V, F> {
    Init,
    /// The initial guard sequence.
    Guard(V),
    /// The context-free future that finally resolves the guard.
    GuardLast(SwallowNonFatal<Bridge<F>>),
    /// The initial body sequence.
    Body(V),
    /// The context-free future that finally resolves the body.
    BodyLast(SwallowNonFatal<Bridge<F>>),
}

#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
struct Bridge<F>(ExitResult<F>);

impl<F, E: ?Sized> EnvFuture<E> for Bridge<F>
    where F: Future<Item = ExitStatus>
{
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self, _: &mut E) -> Poll<Self::Item, Self::Error> {
        self.0.poll()
    }

    fn cancel(&mut self, _: &mut E) {
        // Nothing to cancel
    }
}

impl<S, E: ?Sized> EnvFuture<E> for Loop<S, E>
    where S: SpawnRef<E>,
          S::Error: IsFatalError,
          E: LastStatusEnvironment + ReportErrorEnvironment,
{
    type Item = ExitStatus;
    type Error = S::Error;

    fn poll(&mut self, env: &mut E) -> Poll<Self::Item, Self::Error> {
        if self.guard.is_empty() && self.body.is_empty() {
            // Not a well formed command, rather than burning CPU and spinning
            // here, we'll just bail out. Alternatively we can just return
            // `NotReady` without ever making any progress, but that may not be
            // worth any downstream debugging headaches.
            return Ok(Async::Ready(EXIT_SUCCESS));
        }

        loop {
            let next_state = match self.state {
                State::Init => None,

                State::Guard(ref mut f) => {
                    let (guard, last) = try_ready!(f.poll(env));
                    self.guard = guard;
                    Some(State::GuardLast(swallow_non_fatal_errors(Bridge(last))))
                },

                State::GuardLast(ref mut f) => {
                    let status = try_ready!(f.poll(env));
                    let should_continue = status.success() ^ self.invert_guard_status;
                    if !should_continue {
                        if !self.has_run_body {
                            // bash/zsh will exit loops with a successful status if
                            // loop breaks out of the first round without running the body
                            env.set_last_status(EXIT_SUCCESS);
                        }

                        // NB: last status should contain the status of the last body execution
                        return Ok(Async::Ready(env.last_status()));
                    }

                    // Set the last status as that of the guard so that the body
                    // will have access to it, but only if we are to continue executing
                    // the loop, otherwise we want the caller to observe the last status
                    // as that of the body itself.
                    env.set_last_status(status);
                    let body = mem::replace(&mut self.body, Vec::new());
                    Some(State::Body(VecSequence::new(body)))
                },

                State::Body(ref mut f) => {
                    let (body, last) = try_ready!(f.poll(env));
                    self.has_run_body = true;
                    self.body = body;
                    Some(State::BodyLast(swallow_non_fatal_errors(Bridge(last))))
                },

                State::BodyLast(ref mut f) => {
                    let status = try_ready!(f.poll(env));
                    env.set_last_status(status);
                    None
                },
            };

            self.state = next_state.unwrap_or_else(|| {
                let guard = mem::replace(&mut self.guard, Vec::new());
                State::Guard(VecSequence::new(guard))
            });
        }
    }

    fn cancel(&mut self, env: &mut E) {
        match self.state {
            State::Init |
            State::GuardLast(_) |
            State::BodyLast(_) => {},
            State::Guard(ref mut f) |
            State::Body(ref mut f) => f.cancel(env),
        }
    }
}
