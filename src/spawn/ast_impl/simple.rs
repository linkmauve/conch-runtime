use conch_parser::ast;
use env::{AsyncIoEnvironment, ExecutableEnvironment, ExportedVariableEnvironment,
          FileDescEnvironment, FileDescOpener, FunctionEnvironment,
          RedirectRestorer, SetArgumentsEnvironment, VarRestorer,
          UnsetVariableEnvironment, WorkingDirectoryEnvironment};
use error::{CommandError, RedirectionError};
use eval::{RedirectEval, RedirectOrCmdWord, RedirectOrVarAssig, WordEval};
use failure::Fail;
use futures::Future;
use io::FileDescWrapper;
use spawn::{ExitResult, Spawn, SimpleCommand, simple_command, SpawnedSimpleCommand};
use std::borrow::Borrow;
use std::hash::Hash;
use std::vec::IntoIter;

/// A type alias for the `EnvFuture` implementation returned when spawning
/// a `SimpleCommand` AST node.
pub type SimpleCommandEnvFuture<R, V, W, E> = SimpleCommand<
    R, V, W,
    IntoIter<RedirectOrVarAssig<R, V, W>>,
    IntoIter<RedirectOrCmdWord<R, W>>,
    E,
    RedirectRestorer<E>,
    VarRestorer<E>,
>;

impl<V, W, R, S, E: ?Sized> Spawn<E> for ast::SimpleCommand<V, W, R>
    where R: RedirectEval<E, Handle = E::FileHandle>,
          R::Error: From<RedirectionError>,
          V: Hash + Eq + Borrow<String>,
          W: WordEval<E>,
          S: Clone + Spawn<E>,
          S::Error: From<CommandError>
              + From<RedirectionError>
              + From<R::Error>
              + From<W::Error>
              + From<<E::ExecFuture as Future>::Error>,
          E: AsyncIoEnvironment
              + ExecutableEnvironment
              + ExportedVariableEnvironment
              + FileDescEnvironment
              + FileDescOpener
              + FunctionEnvironment<Fn = S>
              + SetArgumentsEnvironment
              + UnsetVariableEnvironment
              + WorkingDirectoryEnvironment,
          E::Arg: From<W::EvalResult>,
          E::Args: From<Vec<E::Arg>>,
          E::FileHandle: Clone + FileDescWrapper + From<E::OpenedFileHandle>,
          E::FnName: From<W::EvalResult>,
          <E::ExecFuture as Future>::Error: Fail,
          E::IoHandle: From<E::FileHandle>,
          E::VarName: Borrow<String> + Clone + From<V>,
          E::Var: Borrow<String> + Clone + From<W::EvalResult>,
{
    type EnvFuture = SimpleCommandEnvFuture<R, V, W, E>;
    type Future = ExitResult<SpawnedSimpleCommand<E::ExecFuture, S::Future>>;
    type Error = S::Error;

    fn spawn(self, env: &E) -> Self::EnvFuture {
        let vars: Vec<_> = self.redirects_or_env_vars.into_iter().map(Into::into).collect();
        let words: Vec<_> = self.redirects_or_cmd_words.into_iter().map(Into::into).collect();

        simple_command(vars, words, env)
    }
}

impl<'a, V, W, R, S, E: ?Sized> Spawn<E> for &'a ast::SimpleCommand<V, W, R>
    where &'a R: RedirectEval<E, Handle = E::FileHandle>,
          <&'a R as RedirectEval<E>>::Error: From<RedirectionError>,
          V: Hash + Eq + Borrow<String> + Clone,
          &'a W: WordEval<E>,
          S: Clone + Spawn<E>,
          S::Error: From<CommandError>
              + From<RedirectionError>
              + From<<&'a R as RedirectEval<E>>::Error>
              + From<<&'a W as WordEval<E>>::Error>
              + From<<E::ExecFuture as Future>::Error>,
          E: AsyncIoEnvironment
              + ExecutableEnvironment
              + ExportedVariableEnvironment
              + FileDescEnvironment
              + FileDescOpener
              + FunctionEnvironment<Fn = S>
              + SetArgumentsEnvironment
              + UnsetVariableEnvironment
              + WorkingDirectoryEnvironment,
          E::Arg: From<<&'a W as WordEval<E>>::EvalResult>,
          E::Args: From<Vec<E::Arg>>,
          E::FileHandle: Clone + FileDescWrapper + From<E::OpenedFileHandle>,
          E::FnName: From<<&'a W as WordEval<E>>::EvalResult>,
          <E::ExecFuture as Future>::Error: Fail,
          E::IoHandle: From<E::FileHandle>,
          E::VarName: Borrow<String> + Clone + From<V>,
          E::Var: Borrow<String> + Clone + From<<&'a W as WordEval<E>>::EvalResult>,
{
    type EnvFuture = SimpleCommandEnvFuture<&'a R, V, &'a W, E>;
    type Future = ExitResult<SpawnedSimpleCommand<E::ExecFuture, S::Future>>;
    type Error = S::Error;

    fn spawn(self, env: &E) -> Self::EnvFuture {
        let vars: Vec<_> = self.redirects_or_env_vars.iter()
            .map(|v| {
                use self::ast::RedirectOrEnvVar::*;
                match *v {
                    Redirect(ref r) => RedirectOrVarAssig::Redirect(r),
                    EnvVar(ref v, ref w) => RedirectOrVarAssig::VarAssig(v.clone(), w.as_ref()),
                }
            })
            .collect();
        let words: Vec<_> = self.redirects_or_cmd_words.iter()
            .map(|w| match *w {
                ast::RedirectOrCmdWord::Redirect(ref r) => RedirectOrCmdWord::Redirect(r),
                ast::RedirectOrCmdWord::CmdWord(ref w) => RedirectOrCmdWord::CmdWord(w),
            })
            .collect();

        simple_command(vars, words, env)
    }
}
