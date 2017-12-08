extern crate conch_runtime;
extern crate futures;
extern crate tokio_io;
extern crate void;

use conch_runtime::io::{FileDesc, Permissions, Pipe};
use std::borrow::Cow;
use std::fs;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
#[cfg(unix)] use std::os::unix::fs::symlink as symlink_dir;
#[cfg(windows)] use std::os::windows::fs::symlink_dir as symlink_dir;
use std::rc::Rc;

#[macro_use]
mod support;
pub use self::support::*;

#[derive(Debug, Clone)]
struct DummyWorkingDirEnv(PathBuf);

impl WorkingDirectoryEnvironment for DummyWorkingDirEnv {
    fn path_relative_to_working_dir<'a>(&self, path: Cow<'a, Path>) -> Cow<'a, Path> {
        Cow::Owned(self.0.join(path))
    }

    fn current_working_dir(&self) -> &Path {
        &self.0
    }
}

fn run_pwd(use_dots: bool, pwd_args: &[&str], physical_result: bool) {
    let tempdir = mktmp!();

    let tempdir_path = tempdir.path().canonicalize().expect("failed to canonicalize");

    let path_real = tempdir_path.join("real");
    let path_sym = tempdir_path.join("sym");
    let path_foo_real = path_real.join("foo");
    let path_foo_sym = path_sym.join("foo");

    let cur_dir = if use_dots {
        // NB: on windows we apparently can't append a path with `/` separators
        // if the path we're joining to has already been canonicalized
        path_foo_sym
            .join(".")
            .join("..")
            .join("foo")
            .join("..")
            .join(".")
            .join("foo")
            .join(".")
    } else {
        path_foo_sym.clone()
    };

    fs::create_dir(&path_real).expect("failed to create real");
    symlink_dir(&path_real, &path_sym).expect("failed to create symlink");
    fs::create_dir(&path_foo_sym).expect("failed to create foo");

    let mut lp = Core::new().expect("failed to create core");
    let mut env = Env::with_config(EnvConfig {
        interactive: false,
        args_env: ArgsEnv::with_name_and_args("shell name".to_owned(), vec!()),
        async_io_env: PlatformSpecificAsyncIoEnv::new(lp.remote(), Some(2)),
        file_desc_env: FileDescEnv::<Rc<FileDesc>>::new(),
        last_status_env: LastStatusEnv::new(),
        var_env: VarEnv::<String, String>::new(),
        exec_env: ExecEnv::new(lp.remote()),
        working_dir_env: DummyWorkingDirEnv(cur_dir),
        fn_name: PhantomData as PhantomData<Rc<String>>,
        fn_error: PhantomData as PhantomData<RuntimeError>,
    });

    let pwd = builtin::pwd(pwd_args.iter().map(|&s| s.to_owned()));

    let pipe = Pipe::new().expect("pipe failed");
    env.set_file_desc(conch_runtime::STDOUT_FILENO, pipe.writer.into(), Permissions::Write);

    let ((_, output), exit) = lp.run(
        tokio_io::io::read_to_end(env.read_async(pipe.reader), Vec::new())
            .join(pwd.spawn(&env)
                  .pin_env(env)
                  .flatten()
                  .map_err(|void| void::unreachable(void))
            )
    ).expect("future failed");

    assert_eq!(exit, EXIT_SUCCESS);

    let path_expected = if physical_result {
        path_foo_real
    } else {
        path_foo_sym
    };

    let path_expected = format!("{}\n", path_expected.to_string_lossy());
    assert_eq!(String::from_utf8_lossy(&output), path_expected);
}

#[test]
fn physical() {
    run_pwd(false, &["-P"], true);
}

#[test]
fn physical_removes_dot_components() {
    run_pwd(true, &["-P"], true);
}

#[test]
fn logical() {
    run_pwd(false, &["-L"], false);
}

#[test]
fn logical_behaves_as_physical_if_dot_components_present() {
    run_pwd(true, &["-L"], true);
}

#[test]
fn no_arg_behaves_as_logical() {
    run_pwd(false, &[], false);
}

#[test]
fn no_arg_behaves_as_physical_if_dot_components_present() {
    run_pwd(true, &[], true);
}

#[test]
fn last_specified_flag_wins() {
    run_pwd(false, &["-L", "-P", "-L"], false);
    run_pwd(false, &["-P", "-L", "-P"], true);
}

#[test]
fn successful_if_no_stdout() {
    let (mut lp, env) = new_env_with_no_fds();
    let pwd = builtin::pwd(Vec::<Rc<String>>::new());
    let exit = lp.run(pwd.spawn(&env).pin_env(env).flatten());
    assert_eq!(exit, Ok(EXIT_SUCCESS));
}

#[test]
#[should_panic]
fn polling_canceled_pwd_panics() {
    let (_, mut env) = new_env_with_no_fds();
    let mut pwd = builtin::pwd(Vec::<Rc<String>>::new())
        .spawn(&env);

    pwd.cancel(&mut env);
    let _ = pwd.poll(&mut env);
}