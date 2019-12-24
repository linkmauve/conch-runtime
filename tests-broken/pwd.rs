#![deny(rust_2018_idioms)]
use conch_runtime;

use tokio_io;
use void;

use conch_runtime::io::Permissions;
use std::borrow::Cow;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink as symlink_dir;
#[cfg(windows)]
use std::os::windows::fs::symlink_dir;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[macro_use]
mod support;
pub use self::support::spawn::builtin::pwd;
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

async fn run_pwd(use_dots: bool, pwd_args: &[&str], physical_result: bool) {
    let tempdir = mktmp!();

    let tempdir_path = tempdir
        .path()
        .canonicalize()
        .expect("failed to canonicalize");

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

    let mut env = Env::with_config(
        DefaultEnvConfigArc::new(Some(2))
            .expect("failed to create test env")
            .change_file_desc_manager_env(PlatformSpecificFileDescManagerEnv::new(Some(2)))
            .change_var_env(VarEnv::<String, String>::new())
            .change_working_dir_env(DummyWorkingDirEnv(cur_dir)),
    );

    let pwd = pwd(pwd_args.iter().map(|&s| s.to_owned()));

    let pipe = env.open_pipe().expect("pipe failed");
    env.set_file_desc(
        conch_runtime::STDOUT_FILENO,
        pipe.writer,
        Permissions::Write,
    );

    let read_to_end = env
        .read_async(pipe.reader)
        .expect("failed to get read_to_end");
    let ((_, output), exit) = Compat01As03::new(
        tokio_io::io::read_to_end(read_to_end, Vec::new()).join(
            pwd.spawn(&env)
                .pin_env(env)
                .flatten()
                .map_err(|void| void::unreachable(void)),
        ),
    )
    .await
    .expect("future failed");

    assert_eq!(exit, EXIT_SUCCESS);

    let path_expected = if physical_result {
        path_foo_real
    } else {
        path_foo_sym
    };

    let path_expected = format!("{}\n", path_expected.to_string_lossy());
    assert_eq!(String::from_utf8_lossy(&output), path_expected);
}

#[tokio::test]
async fn physical() {
    run_pwd(false, &["-P"], true).await;
}

#[tokio::test]
async fn physical_removes_dot_components() {
    run_pwd(true, &["-P"], true).await;
}

#[tokio::test]
async fn logical() {
    run_pwd(false, &["-L"], false).await;
}

#[tokio::test]
async fn logical_behaves_as_physical_if_dot_components_present() {
    run_pwd(true, &["-L"], true).await;
}

#[tokio::test]
async fn no_arg_behaves_as_logical() {
    run_pwd(false, &[], false).await;
}

#[tokio::test]
async fn no_arg_behaves_as_physical_if_dot_components_present() {
    run_pwd(true, &[], true).await;
}

#[tokio::test]
async fn last_specified_flag_wins() {
    run_pwd(false, &["-L", "-P", "-L"], false).await;
    run_pwd(false, &["-P", "-L", "-P"], true).await;
}

#[tokio::test]
async fn successful_if_no_stdout() {
    let env = new_env_with_no_fds();
    let pwd = pwd(Vec::<Arc<String>>::new());
    let exit = Compat01As03::new(pwd.spawn(&env).pin_env(env).flatten()).await;
    assert_eq!(exit, Ok(EXIT_SUCCESS));
}

#[test]
#[should_panic]
fn polling_canceled_pwd_panics() {
    let mut env = new_env_with_no_fds();
    let mut pwd = pwd(Vec::<Arc<String>>::new()).spawn(&env);

    pwd.cancel(&mut env);
    let _ = pwd.poll(&mut env);
}