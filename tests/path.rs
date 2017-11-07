use std::fs;
#[cfg(unix)] use std::os::unix::fs::symlink as symlink_dir;
#[cfg(windows)] use std::os::windows::fs::symlink_dir as symlink_dir;
use std::path::Path;

#[macro_use]
mod support;
pub use self::support::*;

#[test]
fn join_logical_normalizes_root_paths() {
    let mut path = NormalizedPath::new();
    path.join_normalized_logial("some/path");

    path.join_normalized_logial("/foo/./bar/../baz");
    assert_eq!(*path, Path::new("/foo/baz"));
}

#[test]
fn join_logical_normalizes_relative_paths() {
    let mut path = NormalizedPath::new();
    path.join_normalized_logial("foo/bar");

    path.join_normalized_logial("./../qux/./bar/../baz");
    assert_eq!(*path, Path::new("foo/qux/baz"));
}

#[test]
fn join_physical_normalizes_paths_and_resolves_symlinks() {
    let tempdir = mktmp!();
    let tempdir_path = tempdir.path().canonicalize().expect("failed to canonicalize");

    let path_real = tempdir_path.join("real");
    let path_sym = tempdir_path.join("sym");
    let path_foo_real = path_real.join("foo");
    let path_foo_sym = path_sym.join("foo");

    fs::create_dir(&path_real).expect("failed to create real");
    symlink_dir(&path_real, &path_sym).expect("failed to create symlink");
    fs::create_dir(&path_foo_sym).expect("failed to create foo");

    // Test that paths with relative components are canonicalized
    {
        let mut path = NormalizedPath::new();
        path.join_normalized_physical(&path_sym.join("./../sym/./foo/./.")).unwrap();

        assert_eq!(*path, path_foo_real);
    }

    // Test that even paths without relative components are canonicalized
    {
        let mut path = NormalizedPath::new();
        path.join_normalized_physical(&path_foo_sym).unwrap();

        assert_eq!(*path, path_foo_real);
    }

    // Test path is not changed if an error occurs
    {
        let mut path = NormalizedPath::new();
        path.join_normalized_logial(&path_foo_real);
        let orig_path = path.clone();

        path.join_normalized_physical("../if_this_exists_the_world_has_ended/../foo")
            .unwrap_err();

        assert_eq!(path, orig_path);
    }
}
