//! System tests for compiling Rust code with cargo.
//!
//! Any copyright is dedicated to the Public Domain.
//! http://creativecommons.org/publicdomain/zero/1.0/

pub mod helpers;

use anyhow::{Context, Result, ensure};
use helpers::{CARGO, CRATE_DIR, cargo_clean, stop_sccache};

use assert_cmd::prelude::*;
use fs_err as fs;
use helpers::{SCCACHE_BIN, SccacheTest};
use predicates::prelude::*;
use serial_test::serial;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

#[macro_use]
extern crate log;

#[test]
#[serial]
fn test_rust_cargo_check() -> Result<()> {
    test_rust_cargo_cmd("check", SccacheTest::new(None)?)
}

#[test]
#[serial]
fn test_rust_cargo_check_readonly() -> Result<()> {
    test_rust_cargo_cmd_readonly("check", SccacheTest::new(None)?)
}

#[test]
#[serial]
fn test_rust_cargo_build() -> Result<()> {
    test_rust_cargo_cmd("build", SccacheTest::new(None)?)
}

#[test]
#[serial]
fn test_rust_cargo_build_readonly() -> Result<()> {
    test_rust_cargo_cmd_readonly("build", SccacheTest::new(None)?)
}

#[test]
#[serial]
fn test_rust_cargo_shares_cache_between_local_git_worktrees() -> Result<()> {
    let git_worktrees = [("SCCACHE_GIT_WORKTREES", OsString::from("1"))];
    let test_info = SccacheTest::new(Some(&git_worktrees))?;
    let worktree_parent = worktree_test_parent(test_info.tempdir.path())?;
    let repository = worktree_parent.join("repository");
    let linked = worktree_parent.join("linked");
    copy_worktree_test_crate(&repository)?;
    init_git_repository(&repository)?;
    git(
        &repository,
        &["worktree", "add", "--detach", path_str(&linked)?],
    )?;

    build_worktree(&test_info, &repository, "1")?;
    let hits_after_repository = rust_cache_stat("cache_hits")?;
    build_worktree(&test_info, &linked, "1")?;
    let hits_after_linked = rust_cache_stat("cache_hits")?;
    ensure!(
        hits_after_linked > hits_after_repository,
        "expected a Rust cache hit in the linked worktree (before: {hits_after_repository}, after: {hits_after_linked})"
    );
    ensure_dep_info_uses_current_worktree(&repository, &linked)?;

    compile_direct_input(
        &test_info,
        &repository,
        "relative_input",
        Path::new("src/lib.rs"),
        Path::new("relative-target"),
        None,
    )?;
    let hits_before_relative_linked = rust_cache_stat("cache_hits")?;
    compile_direct_input(
        &test_info,
        &linked,
        "relative_input",
        Path::new("src/lib.rs"),
        Path::new("relative-target"),
        None,
    )?;
    ensure!(
        rust_cache_stat("cache_hits")? > hits_before_relative_linked,
        "relative direct rustc paths should share an entry across worktrees"
    );
    ensure_dep_info_targets_are_relative(&linked.join("relative-target/relative_input.d"))?;

    compile_absolute_input(&test_info, &repository)?;
    let hits_before_absolute_linked = rust_cache_stat("cache_hits")?;
    let misses_before_absolute_linked = rust_cache_stat("cache_misses")?;
    compile_absolute_input(&test_info, &linked)?;
    ensure!(
        rust_cache_stat("cache_misses")? > misses_before_absolute_linked,
        "an absolute source input must not share an entry across worktrees"
    );
    ensure!(
        rust_cache_stat("cache_hits")? == hits_before_absolute_linked,
        "an absolute source input unexpectedly reused another worktree's entry"
    );

    let user_remap = format!("{}=redacted", path_str(&repository)?);
    compile_direct_input(
        &test_info,
        &repository,
        "user_remap",
        Path::new("src/lib.rs"),
        Path::new("user-remap-target"),
        Some(&user_remap),
    )?;
    let hits_before_user_remap_linked = rust_cache_stat("cache_hits")?;
    let misses_before_user_remap_linked = rust_cache_stat("cache_misses")?;
    compile_direct_input(
        &test_info,
        &linked,
        "user_remap",
        Path::new("src/lib.rs"),
        Path::new("user-remap-target"),
        Some(&user_remap),
    )?;
    ensure!(
        rust_cache_stat("cache_misses")? > misses_before_user_remap_linked,
        "a user path remap must not share an entry across worktrees"
    );
    ensure!(
        rust_cache_stat("cache_hits")? == hits_before_user_remap_linked,
        "a user path remap unexpectedly reused another worktree's entry"
    );

    let proc_macro = build_env_proc_macro(&test_info)?;
    let proc_macro_facade = build_proc_macro_facade(&test_info, &proc_macro)?;
    let proc_macro_dir = proc_macro.parent().context("proc macro has no parent")?;
    compile_proc_macro_consumer(&test_info, &repository, &proc_macro_facade, proc_macro_dir)?;
    let hits_before_proc_macro_linked = rust_cache_stat("cache_hits")?;
    let misses_before_proc_macro_linked = rust_cache_stat("cache_misses")?;
    compile_proc_macro_consumer(&test_info, &linked, &proc_macro_facade, proc_macro_dir)?;
    ensure!(
        rust_cache_stat("cache_misses")? > misses_before_proc_macro_linked,
        "a re-exported proc macro that can observe OUT_DIR must cause a worktree-specific miss"
    );
    ensure!(
        rust_cache_stat("cache_hits")? == hits_before_proc_macro_linked,
        "a re-exported proc macro unexpectedly reused another worktree's Cargo env entry"
    );

    clean_worktree(&test_info, &repository)?;
    build_worktree(&test_info, &repository, path_str(&repository)?)?;
    let misses_before_env_path = rust_cache_stat("cache_misses")?;
    clean_worktree(&test_info, &linked)?;
    build_worktree(&test_info, &linked, path_str(&linked)?)?;
    ensure!(
        rust_cache_stat("cache_misses")? > misses_before_env_path,
        "a worktree-specific value used by env! must cause a cache miss"
    );

    let source = linked.join("src/lib.rs");
    fs::write(
        &source,
        format!(
            "{}\npub fn changed_in_worktree() {{}}\n",
            fs::read_to_string(&source)?
        ),
    )?;
    clean_worktree(&test_info, &linked)?;
    let misses_before_source_change = rust_cache_stat("cache_misses")?;
    build_worktree(&test_info, &linked, "1")?;
    ensure!(
        rust_cache_stat("cache_misses")? > misses_before_source_change,
        "changed source must cause a cache miss"
    );

    let independent = test_info.tempdir.path().join("independent");
    git(
        test_info.tempdir.path(),
        &[
            "clone",
            "--local",
            path_str(&repository)?,
            path_str(&independent)?,
        ],
    )?;
    let hits_before_independent_clone = rust_cache_stat("cache_hits")?;
    let misses_before_independent_clone = rust_cache_stat("cache_misses")?;
    build_worktree(&test_info, &independent, "1")?;
    ensure!(
        rust_cache_stat("cache_misses")? > misses_before_independent_clone,
        "an independent clone must not share the worktree cache namespace"
    );
    ensure!(
        rust_cache_stat("cache_hits")? == hits_before_independent_clone,
        "an independent clone unexpectedly reused a linked-worktree entry"
    );
    Ok(())
}

#[cfg(unix)]
fn worktree_test_parent(tempdir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::symlink;

    let canonical = tempdir.join("canonical");
    let alias = tempdir.join("alias");
    fs::create_dir(&canonical)?;
    symlink(&canonical, &alias)?;
    Ok(alias)
}

#[cfg(not(unix))]
fn worktree_test_parent(tempdir: &Path) -> Result<PathBuf> {
    Ok(tempdir.to_owned())
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("Path is not UTF-8: {}", path.display()))
}

fn copy_worktree_test_crate(destination: &Path) -> Result<()> {
    let fixture = CRATE_DIR
        .parent()
        .expect("test-crate parent")
        .join("worktree-crate");
    fs::create_dir_all(destination.join("src"))?;
    for relative in ["Cargo.toml", "src/lib.rs", "src/proc_macro_consumer.rs"] {
        fs::copy(fixture.join(relative), destination.join(relative))?;
    }
    Ok(())
}

fn git(cwd: &Path, arguments: &[&str]) -> Result<()> {
    Command::new("git")
        .args(arguments)
        .current_dir(cwd)
        .assert()
        .try_success()
        .with_context(|| format!("git {} failed", arguments.join(" ")))?;
    Ok(())
}

fn init_git_repository(repository: &Path) -> Result<()> {
    git(repository, &["init", "--initial-branch=main"])?;
    git(repository, &["add", "."])?;
    git(
        repository,
        &[
            "-c",
            "user.name=sccache tests",
            "-c",
            "user.email=sccache@example.invalid",
            "commit",
            "-m",
            "initial",
        ],
    )
}

fn build_worktree(test_info: &SccacheTest<'_>, root: &Path, env_value: &str) -> Result<()> {
    Command::new(CARGO.as_os_str())
        .args(["build", "--color=never"])
        .envs(test_info.env.iter().cloned())
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("TEST_ENV_VAR", env_value)
        .current_dir(root)
        .assert()
        .try_success()?;
    Ok(())
}

fn compile_absolute_input(test_info: &SccacheTest<'_>, root: &Path) -> Result<()> {
    compile_direct_input(
        test_info,
        root,
        "absolute_input",
        &root.join("src/lib.rs"),
        Path::new("absolute-target"),
        None,
    )
}

fn compile_direct_input(
    test_info: &SccacheTest<'_>,
    root: &Path,
    crate_name: &str,
    source: &Path,
    output_dir: &Path,
    user_remap: Option<&str>,
) -> Result<()> {
    fs::create_dir_all(root.join(output_dir))?;
    let mut command = Command::new(SCCACHE_BIN.as_os_str());
    command
        .arg("rustc")
        .args(["--crate-name", crate_name, "--crate-type", "lib"])
        .args(["--emit=link,dep-info", "--color=never"])
        .arg("--out-dir")
        .arg(output_dir);
    if let Some(remap) = user_remap {
        command.arg("--remap-path-prefix").arg(remap);
    }
    command
        .arg(source)
        .envs(test_info.env.iter().cloned())
        .current_dir(root)
        .assert()
        .try_success()?;
    Ok(())
}

fn ensure_dep_info_targets_are_relative(dep_info: &Path) -> Result<()> {
    let contents = fs::read_to_string(dep_info)?;
    let target = contents
        .lines()
        .find_map(|line| line.split_once(": ").map(|(target, _)| target))
        .context("dep-info did not contain a target with dependencies")?;
    ensure!(
        !Path::new(target).is_absolute(),
        "{} contains an absolute target {target}",
        dep_info.display()
    );
    ensure!(
        target.starts_with("relative-target"),
        "{} contains the unexpected target {target}",
        dep_info.display()
    );
    Ok(())
}

fn build_env_proc_macro(test_info: &SccacheTest<'_>) -> Result<std::path::PathBuf> {
    let source = test_info.tempdir.path().join("env_proc_macro.rs");
    let output_dir = test_info.tempdir.path().join("proc-macro-target");
    fs::create_dir_all(&output_dir)?;
    fs::write(
        &source,
        r#"extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro]
pub fn observed_out_dir(_input: TokenStream) -> TokenStream {
    format!("{:?}", std::env::var("OUT_DIR").unwrap())
        .parse()
        .unwrap()
}
"#,
    )?;
    Command::new("rustc")
        .args([
            "--crate-name",
            "env_proc_macro",
            "--crate-type",
            "proc-macro",
        ])
        .arg(&source)
        .arg("--out-dir")
        .arg(&output_dir)
        .assert()
        .try_success()?;
    Ok(output_dir.join(format!(
        "{}env_proc_macro.{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_EXTENSION
    )))
}

fn build_proc_macro_facade(test_info: &SccacheTest<'_>, proc_macro: &Path) -> Result<PathBuf> {
    let source = test_info.tempdir.path().join("env_proc_macro_facade.rs");
    let output_dir = test_info.tempdir.path().join("proc-macro-facade-target");
    fs::create_dir_all(&output_dir)?;
    fs::write(
        &source,
        r#"extern crate env_proc_macro;
pub use env_proc_macro::observed_out_dir;
"#,
    )?;
    let mut extern_arg = OsString::from("env_proc_macro=");
    extern_arg.push(proc_macro);
    Command::new("rustc")
        .args([
            "--crate-name",
            "env_proc_macro_facade",
            "--crate-type",
            "lib",
            "--emit=metadata,link",
            "--extern",
        ])
        .arg(extern_arg)
        .arg(&source)
        .arg("--out-dir")
        .arg(&output_dir)
        .assert()
        .try_success()?;
    Ok(output_dir.join("libenv_proc_macro_facade.rmeta"))
}

fn compile_proc_macro_consumer(
    test_info: &SccacheTest<'_>,
    root: &Path,
    proc_macro_facade: &Path,
    proc_macro_dir: &Path,
) -> Result<()> {
    let output_dir = Path::new("proc-macro-consumer-target");
    fs::create_dir_all(root.join(output_dir))?;
    let mut extern_arg = OsString::from("env_proc_macro_facade=");
    extern_arg.push(proc_macro_facade);
    let mut dependency_path = OsString::from("dependency=");
    dependency_path.push(proc_macro_dir);
    Command::new(SCCACHE_BIN.as_os_str())
        .arg("rustc")
        .args([
            "--crate-name",
            "proc_macro_consumer",
            "--crate-type",
            "lib",
            "--emit=link,dep-info",
            "--color=never",
            "--out-dir",
        ])
        .arg(output_dir)
        .arg("--extern")
        .arg(extern_arg)
        .arg("-L")
        .arg(dependency_path)
        .arg("src/proc_macro_consumer.rs")
        .envs(test_info.env.iter().cloned())
        .env("OUT_DIR", root.join("generated"))
        .current_dir(root)
        .assert()
        .try_success()?;
    Ok(())
}

fn clean_worktree(test_info: &SccacheTest<'_>, root: &Path) -> Result<()> {
    Command::new(CARGO.as_os_str())
        .arg("clean")
        .envs(test_info.env.iter().cloned())
        .env("CARGO_TARGET_DIR", root.join("target"))
        .current_dir(root)
        .assert()
        .try_success()?;
    Ok(())
}

fn rust_cache_stat(category: &str) -> Result<u64> {
    let output = Command::new(SCCACHE_BIN.as_os_str())
        .args(["--show-stats", "--stats-format=json"])
        .output()?;
    ensure!(output.status.success(), "sccache --show-stats failed");
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    Ok(stats["stats"][category]["counts"]["Rust"]
        .as_u64()
        .unwrap_or(0))
}

fn ensure_dep_info_uses_current_worktree(repository: &Path, linked: &Path) -> Result<()> {
    let repository_paths = [repository.to_owned(), fs::canonicalize(repository)?];
    let mut dep_info_files = 0;
    for entry in WalkDir::new(linked.join("target")) {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("d")
        {
            continue;
        }
        dep_info_files += 1;
        let contents = fs::read_to_string(entry.path())?;
        for repository in &repository_paths {
            ensure!(
                !contents.contains(path_str(repository)?),
                "{} still references the original worktree {}:\n{contents}",
                entry.path().display(),
                repository.display()
            );
        }
    }
    ensure!(dep_info_files > 0, "no dep-info files were generated");
    Ok(())
}

#[test]
#[serial]
#[cfg(unix)]
fn test_run_log_no_perm() -> Result<()> {
    trace!("sccache with log");
    stop_sccache()?;
    let mut cmd = Command::new(SCCACHE_BIN.as_os_str());
    cmd.arg("gcc")
        .env("SCCACHE_ERROR_LOG", "/no-perm.log") // Should not work
        .env("SCCACHE_LOG", "debug");

    cmd.assert().failure().stderr(predicate::str::contains(
        "Cannot open/write log file '/no-perm.log'",
    ));
    Ok(())
}

#[test]
#[serial]
fn test_run_log() -> Result<()> {
    trace!("sccache with log");
    stop_sccache()?;

    let tempdir = tempfile::Builder::new()
        .prefix("sccache_test_rust_cargo")
        .tempdir()
        .context("Failed to create tempdir")?;
    let tmppath = tempdir.path().join("perm.log");
    let mut cmd = Command::new(SCCACHE_BIN.as_os_str());
    cmd.arg("--start-server")
        .env("SCCACHE_ERROR_LOG", &tmppath) // Should not work
        .env("SCCACHE_LOG", "debug");

    cmd.assert().success();
    stop_sccache()?;
    assert!(Path::new(&tmppath).is_file());
    Ok(())
}

/// This test checks that changing an environment variable reference by env! is detected by
/// sccache, causes a rebuild and is correctly printed to stdout.
#[test]
#[serial]
fn test_rust_cargo_run_with_env_dep_parsing() -> Result<()> {
    test_rust_cargo_env_dep(SccacheTest::new(None)?)
}

#[cfg(feature = "unstable")]
#[test]
#[serial]
fn test_rust_cargo_check_nightly() -> Result<()> {
    use std::ffi::OsString;

    test_rust_cargo_cmd(
        "check",
        SccacheTest::new(Some(&[(
            "RUSTFLAGS",
            OsString::from("-Cprofile-generate=."),
        )]))?,
    )
}

#[cfg(feature = "unstable")]
#[test]
#[serial]
fn test_rust_cargo_check_nightly_readonly() -> Result<()> {
    use std::ffi::OsString;

    test_rust_cargo_cmd_readonly(
        "check",
        SccacheTest::new(Some(&[(
            "RUSTFLAGS",
            OsString::from("-Cprofile-generate=."),
        )]))?,
    )
}

#[cfg(feature = "unstable")]
#[test]
#[serial]
fn test_rust_cargo_build_nightly() -> Result<()> {
    use std::ffi::OsString;

    test_rust_cargo_cmd(
        "build",
        SccacheTest::new(Some(&[(
            "RUSTFLAGS",
            OsString::from("-Cprofile-generate=."),
        )]))?,
    )
}

#[cfg(feature = "unstable")]
#[test]
#[serial]
fn test_rust_cargo_build_nightly_readonly() -> Result<()> {
    use std::ffi::OsString;

    test_rust_cargo_cmd_readonly(
        "build",
        SccacheTest::new(Some(&[(
            "RUSTFLAGS",
            OsString::from("-Cprofile-generate=."),
        )]))?,
    )
}

/// Test that building a simple Rust crate with cargo using sccache results in a cache hit
/// when built a second time and a cache miss, when the environment variable referenced via
/// env! is changed.
fn test_rust_cargo_cmd(cmd: &str, test_info: SccacheTest) -> Result<()> {
    // `cargo clean` first, just to be sure there's no leftover build objects.
    cargo_clean(&test_info)?;

    // Now build the crate with cargo.
    Command::new(CARGO.as_os_str())
        .args([cmd, "--color=never"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8().not())?
        .try_success()?;
    // Clean it so we can build it again.
    cargo_clean(&test_info)?;
    Command::new(CARGO.as_os_str())
        .args([cmd, "--color=always"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8())?
        .try_success()?;

    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(
                r#""cache_hits":{"counts":{"Rust":2},"adv_counts":{"rust":2}}"#,
            )
            .from_utf8(),
        )?
        .try_success()?;

    Ok(())
}

fn restart_sccache(
    test_info: &SccacheTest,
    additional_envs: Option<Vec<(String, String)>>,
) -> Result<()> {
    let cache_dir = test_info.tempdir.path().join("cache");

    stop_sccache()?;

    trace!("sccache --start-server");

    let mut cmd = Command::new(SCCACHE_BIN.as_os_str());
    cmd.arg("--start-server");
    cmd.env("SCCACHE_DIR", &cache_dir);

    if let Some(additional_envs) = additional_envs {
        cmd.envs(additional_envs);
    }

    cmd.assert()
        .try_success()
        .context("Failed to start sccache server")?;

    Ok(())
}

/// Test that building a simple Rust crate with cargo using sccache results in the following behaviors (for three different runs):
/// - In read-only mode, a cache miss.
/// - In read-write mode, a cache miss.
/// - In read-only mode, a cache hit.
///
/// The environment variable for read/write mode is added by this function.
fn test_rust_cargo_cmd_readonly(cmd: &str, test_info: SccacheTest) -> Result<()> {
    // `cargo clean` first, just to be sure there's no leftover build objects.
    cargo_clean(&test_info)?;

    // The cache must be put into read-only mode, and that can only be configured
    // when the server starts up, so we need to restart it.
    restart_sccache(
        &test_info,
        Some(vec![("SCCACHE_LOCAL_RW_MODE".into(), "READ_ONLY".into())]),
    )?;

    // Now build the crate with cargo.
    Command::new(CARGO.as_os_str())
        .args([cmd, "--color=never"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8().not())?
        .try_success()?;

    // Stats reset on server restart, so this needs to be run for each build.
    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(r#""cache_hits":{"counts":{},"adv_counts":{}}"#).from_utf8(),
        )?
        .try_stdout(
            predicates::str::contains(
                r#""cache_misses":{"counts":{"Rust":2},"adv_counts":{"rust":2}}"#,
            )
            .from_utf8(),
        )?
        .try_success()?;

    cargo_clean(&test_info)?;
    restart_sccache(
        &test_info,
        Some(vec![("SCCACHE_LOCAL_RW_MODE".into(), "READ_WRITE".into())]),
    )?;
    Command::new(CARGO.as_os_str())
        .args([cmd, "--color=always"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8())?
        .try_success()?;

    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(r#""cache_hits":{"counts":{},"adv_counts":{}}"#).from_utf8(),
        )?
        .try_stdout(
            predicates::str::contains(
                r#""cache_misses":{"counts":{"Rust":2},"adv_counts":{"rust":2}}"#,
            )
            .from_utf8(),
        )?
        .try_success()?;

    cargo_clean(&test_info)?;
    restart_sccache(
        &test_info,
        Some(vec![("SCCACHE_LOCAL_RW_MODE".into(), "READ_ONLY".into())]),
    )?;
    Command::new(CARGO.as_os_str())
        .args([cmd, "--color=always"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8())?
        .try_success()?;

    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(
                r#""cache_hits":{"counts":{"Rust":2},"adv_counts":{"rust":2}}"#,
            )
            .from_utf8(),
        )?
        .try_stdout(
            predicates::str::contains(r#""cache_misses":{"counts":{},"adv_counts":{}}"#)
                .from_utf8(),
        )?
        .try_success()?;

    Ok(())
}

fn test_rust_cargo_env_dep(test_info: SccacheTest) -> Result<()> {
    cargo_clean(&test_info)?;
    // Now build the crate with cargo.
    Command::new(CARGO.as_os_str())
        .args(["run", "--color=never"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8().not())?
        .try_stdout(predicates::str::contains("Env var: 1"))?
        .try_success()?;
    // Clean it so we can build it again.
    cargo_clean(&test_info)?;

    Command::new(CARGO.as_os_str())
        .args(["run", "--color=always"])
        .envs(test_info.env.iter().cloned())
        .env("TEST_ENV_VAR", "OTHER_VALUE")
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8())?
        .try_stdout(predicates::str::contains("Env var: OTHER_VALUE"))?
        .try_success()?;

    // Now get the stats and ensure that we had one cache hit for the second build.
    // The test crate has one dependency (itoa) so there are two separate compilations, but only
    // itoa should be cached (due to the changed environment variable).
    test_info
        .show_stats()?
        .try_stdout(predicates::str::contains(r#""cache_hits":{"counts":{"Rust":1}"#).from_utf8())?
        .try_success()?;

    drop(test_info);
    Ok(())
}

/// Test that building a simple Rust crate with cargo using sccache in read-only mode with an empty cache results in
/// a cache miss that is produced by the readonly storage wrapper (and does not attempt to write to the underlying cache).
#[test]
#[serial]
fn test_rust_cargo_cmd_readonly_preemtive_block() -> Result<()> {
    let test_info = SccacheTest::new(None)?;
    // `cargo clean` first, just to be sure there's no leftover build objects.
    cargo_clean(&test_info)?;

    let sccache_log = test_info.tempdir.path().join("sccache.log");

    stop_sccache()?;

    restart_sccache(
        &test_info,
        Some(vec![
            ("SCCACHE_LOCAL_RW_MODE".into(), "READ_ONLY".into()),
            ("SCCACHE_LOG".into(), "trace".into()),
            (
                "SCCACHE_ERROR_LOG".into(),
                sccache_log.to_str().unwrap().into(),
            ),
        ]),
    )?;

    // Now build the crate with cargo.
    // Assert that our cache miss is due to the readonly storage wrapper, not due to the underlying disk cache.
    Command::new(CARGO.as_os_str())
        .args(["build", "--color=never"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8().not())?
        .try_success()?;

    let log_contents = fs::read_to_string(sccache_log)?;
    assert!(
        predicates::str::contains("server has setup with ReadOnly").eval(log_contents.as_str())
    );
    assert!(
        predicates::str::contains("Error executing cache write: Cannot write to read-only storage")
            .eval(log_contents.as_str())
    );
    assert!(
        predicates::str::contains("DiskCache::finish_put")
            .not()
            .eval(log_contents.as_str())
    );

    // Stats reset on server restart, so this needs to be run for each build.
    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(r#""cache_hits":{"counts":{},"adv_counts":{}}"#).from_utf8(),
        )?
        .try_stdout(
            predicates::str::contains(
                r#""cache_misses":{"counts":{"Rust":2},"adv_counts":{"rust":2}}"#,
            )
            .from_utf8(),
        )?
        .try_success()?;
    Ok(())
}
