sccache includes support for caching Rust compilation. This includes many caveats, and is primarily focused on caching rustc invocations as produced by cargo. A (possibly-incomplete) list follows:
* `--emit` is required.
* `--crate-name` is required.
* Only `link`, `metadata` and `dep-info` are supported as `--emit` values, and `link` must be present.
* `--out-dir` is required.
* `-o file` is not supported.
* Compilation from stdin is not supported, a source file must be provided.
* Values from `env!` require Rust >= 1.46 to be tracked in caching.
* Procedural macros that read files from the filesystem may not be cached properly.
* `rustc`'s incremental compilation needs to be disabled. See [The Cargo Book](https://doc.rust-lang.org/cargo/reference/profiles.html#incremental)
* Crates that invoke the system linker cannot be cached. Examples are `bin`, `dylib`, `cdylib`, and `proc-macro` crates.

If you are using Rust 1.18 or later, you can ask cargo to wrap all compilation with sccache by setting `RUSTC_WRAPPER=sccache` in your build environment.

## Sharing cache entries between Git worktrees

Rust artifacts normally contain the absolute source directory, so equivalent builds in two linked Git worktrees have different cache keys. Set `SCCACHE_GIT_WORKTREES=1` to make sccache detect the local Git worktree, add `--remap-path-prefix=<worktree>=.` to rustc, and key paths relative to the worktree. Worktrees linked to the same local repository share a namespace based on Git's common directory.

The recommended project-level `.cargo/config.toml` is:

```toml
[build]
rustc-wrapper = "sccache"
incremental = false
dep-info-basedir = "."

[env]
SCCACHE_GIT_WORKTREES = { value = "1", force = true }
```

`incremental = false` is required because sccache cannot cache incremental Rust compilations. `dep-info-basedir = "."` is not required by sccache, but keeps Cargo's exported dependency files relative to the project as well. This environment option is evaluated for each compiler invocation and does not require restarting the sccache server.

This mode is intentionally local:

* Linked worktrees created by `git worktree` share entries because they have the same Git common directory.
* Independent clones do not share entries, even when they have the same remote URL.
* Paths outside the worktree remain absolute cache-key inputs. Using the default `target` directory (or the same target path relative to every worktree root) gives the best results.
* Source dependencies passed to rustc as absolute paths remain worktree-specific because rustc preserves those paths in dep-info files. Cargo normally passes project sources as relative paths.
* Values actually read through `env!` are not normalized. A worktree-specific absolute value therefore causes a safe cache miss.
* Crates that load proc macros keep `CARGO_*` environment paths worktree-specific because proc macros can observe those values without rustc reporting an env-dep.
* User-provided `--remap-path-prefix` and `--remap-path-scope` options are preserved unchanged. Because their effective matches can differ between worktrees, compilations that use them remain worktree-specific.
* Distributed compilation is disabled in worktree mode because worker path mappings cannot preserve the local worktree remap safely. Local and remote cache storage remain available.

The automatic remap can make paths embedded by `file!`, debug information, diagnostics, and similar compiler output relative instead of absolute. Run `cargo clean` once after enabling or disabling this mode so Cargo does not reuse artifacts produced with the other path convention.
