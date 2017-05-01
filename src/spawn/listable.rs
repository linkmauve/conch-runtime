use {ExitStatus, EXIT_SUCCESS, STDIN_FILENO, STDOUT_FILENO, Spawn};
use env::{FileDescEnvironment, SubEnvironment};
use future::{Async, EnvFuture, InvertStatus, Pinned, Poll};
use futures::future::{Either, Flatten, Future};
use io::{FileDesc, Permissions, Pipe};
use std::io;
use std::mem;
use std::slice;
use std::vec;
use syntax::ast::ListableCommand;

/// A future representing the spawning of a `ListableCommand`.
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct ListableCommandEnvFuture<T, I, F> {
    invert_last_status: bool,
    pipeline_state: State<T, I, F>,
}

#[derive(Debug)]
enum State<T, I, F> {
    InitSingle(Option<T>),
    InitMany(I),
    Single(F),
}

/// A future representing the execution of a `ListableCommand`.
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct Pipeline<F> where F: Future {
    pipeline: Vec<F>,
    last: LastState<F, F::Error>,
}

impl<F: Future> Pipeline<F> {
    /// Creates a new pipline from a list of futures.
    fn new(mut pipeline: Vec<F>) -> Self {
        debug_assert!(!pipeline.is_empty());

        Pipeline {
            last: LastState::Pending(pipeline.pop().unwrap()),
            pipeline: pipeline,
        }
    }

    /// Creates an adapter with a finished error, essentially a `FutureResult`
    /// but without needing an extra type.
    fn finished(result: Result<ExitStatus, F::Error>) -> Self {
        Pipeline {
            last: LastState::Exited(Some(result)),
            pipeline: Vec::new(),
        }
    }
}

#[derive(Debug)]
enum LastState<F, E> {
    Pending(F),
    Exited(Option<Result<ExitStatus, E>>),
}

#[derive(Debug)]
enum Transition<F, S> {
    Done(F),
    RunSingle(S),
}

/// Type alias for pinned and flattened futures
pub type PinnedFlattenedFuture<E, F> = Flatten<Pinned<E, F>>;

/// Type alias for the future that fully resolves a `ListableCommand`.
pub type ListableCommandFuture<ENV, EF, F> = InvertStatus<Either<
    Pipeline<PinnedFlattenedFuture<ENV, EF>>,
    F
>>;

impl<E: ?Sized, T> Spawn<E> for ListableCommand<T>
    where E: FileDescEnvironment + SubEnvironment,
          E::FileHandle: From<FileDesc> + Clone,
          T: Spawn<E>,
          T::Error: From<io::Error>,
{
    type Error = T::Error;
    type EnvFuture = ListableCommandEnvFuture<T, vec::IntoIter<T>, T::EnvFuture>;
    type Future = ListableCommandFuture<E, T::EnvFuture, T::Future>;

    fn spawn(self, _: &E) -> Self::EnvFuture {
        match self {
            ListableCommand::Single(cmd) => ListableCommandEnvFuture {
                invert_last_status: false,
                pipeline_state: State::InitSingle(Some(cmd)),
            },
            ListableCommand::Pipe(invert, cmds) => pipeline(invert, cmds),
        }
    }
}

impl<'a, E: ?Sized, T> Spawn<E> for &'a ListableCommand<T>
    where E: FileDescEnvironment + SubEnvironment,
          E::FileHandle: From<FileDesc> + Clone,
          &'a T: Spawn<E>,
          <&'a T as Spawn<E>>::Error: From<io::Error>,
{
    type Error = <&'a T as Spawn<E>>::Error;
    type EnvFuture = ListableCommandEnvFuture<
        &'a T,
        slice::Iter<'a, T>,
        <&'a T as Spawn<E>>::EnvFuture
    >;
    type Future = ListableCommandFuture<
        E,
        <&'a T as Spawn<E>>::EnvFuture,
        <&'a T as Spawn<E>>::Future
    >;

    fn spawn(self, _: &E) -> Self::EnvFuture {
        match *self {
            ListableCommand::Single(ref cmd) => ListableCommandEnvFuture {
                invert_last_status: false,
                pipeline_state: State::InitSingle(Some(cmd)),
            },
            ListableCommand::Pipe(invert, ref cmds) => pipeline(invert, cmds),
        }
    }
}

/// Spawns a pipeline of commands.
///
/// The standard output of the previous command will be piped as standard input
/// to the next. The very first and last commands will inherit standard intput
/// and output from the environment, respectively.
///
/// If `invert_last_status` is set to `false`, the pipeline will fully resolve
/// to the last command's exit status. Otherwise, `EXIT_ERROR` will be returned
/// if the last command succeeds, and `EXIT_SUCCESS` will be returned otherwise.
pub fn pipeline<E: ?Sized, T, I>(invert_last_status: bool, commands: I)
    -> ListableCommandEnvFuture<T, I::IntoIter, T::EnvFuture>
    where I: IntoIterator<Item = T>,
          T: Spawn<E>,
{
    ListableCommandEnvFuture {
        invert_last_status: invert_last_status,
        pipeline_state: State::InitMany(commands.into_iter()),
    }
}

impl<E: ?Sized, I, T> EnvFuture<E> for ListableCommandEnvFuture<T, I, T::EnvFuture>
    where E: FileDescEnvironment + SubEnvironment,
          E::FileHandle: From<FileDesc> + Clone,
          I: Iterator<Item = T>,
          T: Spawn<E>,
          T::Error: From<io::Error>,
{
    type Item = ListableCommandFuture<E, T::EnvFuture, T::Future>;
    type Error = T::Error;

    fn poll(&mut self, env: &mut E) -> Poll<Self::Item, Self::Error> {
        loop {
            let state = match self.pipeline_state {
                State::Single(ref mut f) => {
                    let future = match f.poll(env) {
                        Ok(Async::Ready(future)) => Either::B(future),
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Err(e) => Either::A(Pipeline::finished(Err(e))),
                    };

                    Transition::Done(future)
                },

                State::InitSingle(ref mut cmd) => {
                    // We treat single commands specially so that their side effects
                    // can be reflected in the main/parent environment (since "regular"
                    // pipeline commands will each get their own sub-environment
                    // and their changes will not be reflected on the parent)
                    Transition::RunSingle(cmd.take().expect("polled twice").spawn(env))
                }

                State::InitMany(ref mut cmds) => {
                    let mut cmds = cmds.fuse();
                    match (cmds.next(), cmds.next()) {
                        (None, None) => {
                            // Empty pipelines aren't particularly well-formed, but
                            // we'll just treat it as a successful command.
                            let pipeline = Pipeline::finished(Ok(EXIT_SUCCESS));
                            Transition::Done(Either::A(pipeline))
                        },

                        (None, Some(cmd)) | // Should be unreachable
                        (Some(cmd), None) => Transition::RunSingle(cmd.spawn(env)),

                        (Some(first), Some(second)) => {
                            let iter = ::std::iter::once(second).chain(cmds);
                            let pipeline = try!(init_pipeline(env, first, iter));
                            Transition::Done(Either::A(Pipeline::new(pipeline)))
                        }
                    }
                },
            };

            match state {
                Transition::Done(f) => {
                    let future = InvertStatus::new(self.invert_last_status, f);
                    return Ok(Async::Ready(future));
                },

                // Loop around and poll the inner future again. We could just
                // signal that we are ready and get polled again, but that would
                // require traversing an arbitrarily large future tree, so it's
                // probably more efficient for us to quickly retry here.
                Transition::RunSingle(single) => self.pipeline_state = State::Single(single),
            }
        }
    }

    fn cancel(&mut self, env: &mut E) {
        match self.pipeline_state {
            State::InitSingle(_) |
            State::InitMany(_) => {},
            State::Single(ref mut e) => e.cancel(env),
        }
    }
}

impl<F: Future<Item = ExitStatus>> Future for Pipeline<F> {
    type Item = ExitStatus;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        poll_pipeline(&mut self.pipeline);

        let last_status = match self.last {
            LastState::Pending(ref mut f) => match f.poll() {
                Ok(Async::Ready(status)) => Ok(status),
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Err(err) => Err(err),
            },

            LastState::Exited(ref mut ret) => if self.pipeline.is_empty() {
                return ret.take().expect("polled twice after completion").map(Async::Ready);
            } else {
                return Ok(Async::NotReady);
            },
        };

        if self.pipeline.is_empty() {
            Ok(Async::Ready(try!(last_status)))
        } else {
            self.last = LastState::Exited(Some(last_status));
            Ok(Async::NotReady)
        }
    }
}

fn poll_pipeline<F: Future>(pipeline: &mut Vec<F>) {
    if pipeline.is_empty() {
        return;
    }

    *pipeline = mem::replace(pipeline, Vec::new())
        .into_iter()
        .filter_map(|mut future| match future.poll() {
            Ok(Async::NotReady) => Some(future), // Future pending, keep it around

            Err(_) | // Swallow all errors, only the last command can return an error
            Ok(Async::Ready(_)) => None, // Future done, no need to keep polling it
        })
        .collect();
}

/// Spawns each command in the pipeline, and pins them to their own environments.
///
/// bash will apparently run each pipeline command in its own environment, thus
/// no side-effects (e.g. setting variables) are reflected on the parent environment,
/// (though this is probably a side effect of bash forking on each command).
///
/// zsh, on the other hand, does persist side effects from individual commands
/// to the parent environment. Although we could implement this behavior as well,
/// it would require custom fiddling and book keeping with the environment (e.g.
/// only swap the file descriptors between commands, but persist other things
/// like variables), but this doesn't go well with our *generic* approach to everything.
///
/// There is also a question of how useful something like `echo foo | var=value`
/// even is, and whether such a command would even appear in regular scripts.
/// Given that bash is pretty popular, and given that the POSIX spec is slient
/// on how side-effects from pipelines should be handled, we have a pretty low
/// risk of behaving differently than the script author intends, so we'll take
/// bash's approach and spawn each command with its own environment and hide any
/// lasting effects.
fn init_pipeline<E: ?Sized, S, I>(env: &E, first: S, mut pipeline: I)
    -> io::Result<Vec<PinnedFlattenedFuture<E, S::EnvFuture>>>
    where E: FileDescEnvironment + SubEnvironment,
          E::FileHandle: From<FileDesc> + Clone,
          S: Spawn<E>,
          I: Iterator<Item = S>,
{
    let (lo, hi) = pipeline.size_hint();
    let mut result = Vec::with_capacity(hi.unwrap_or(lo) + 1);
    let mut next_in = {
        // First command will automatically inherit the stdin of the
        // parent environment, so no need to manually set it
        let pipe = try!(Pipe::new());

        let mut env = env.sub_env();
        env.set_file_desc(STDOUT_FILENO, pipe.writer.into(), Permissions::Write);
        result.push(first.spawn(&env).pin_env(env).flatten());

        pipe.reader
    };

    let mut last = pipeline.next().expect("pipelines must have at least two commands");
    for next in pipeline {
        let cmd = last;
        last = next;

        let pipe = try!(Pipe::new());

        let mut env = env.sub_env();
        env.set_file_desc(STDIN_FILENO, next_in.into(), Permissions::Read);
        env.set_file_desc(STDOUT_FILENO, pipe.writer.into(), Permissions::Write);
        next_in = pipe.reader;

        result.push(cmd.spawn(&env).pin_env(env).flatten());
    }

    let mut env = env.sub_env();
    env.set_file_desc(STDIN_FILENO, next_in.into(), Permissions::Read);
    result.push(last.spawn(&env).pin_env(env).flatten());
    Ok(result)
}
