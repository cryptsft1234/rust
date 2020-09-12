use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::ops::Not;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use rustc_version::VersionMeta;

const XARGO_MIN_VERSION: (u32, u32, u32) = (0, 3, 22);

const CARGO_MIRI_HELP: &str = r#"Runs binary crates and tests in Miri

Usage:
    cargo miri [subcommand] [<cargo options>...] [--] [<program/test suite options>...]

Subcommands:
    run                      Run binaries
    test                     Run tests
    setup                    Only perform automatic setup, but without asking questions (for getting a proper libstd)

The cargo options are exactly the same as for `cargo run` and `cargo test`, respectively.

Examples:
    cargo miri run
    cargo miri test -- test-suite-filter
"#;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum MiriCommand {
    Run,
    Test,
    Setup,
}

/// The inforamtion Miri needs to run a crate. Stored as JSON when the crate is "compiled".
#[derive(Serialize, Deserialize)]
struct CrateRunInfo {
    /// The command-line arguments.
    args: Vec<String>,
    /// The environment.
    env: Vec<(OsString, OsString)>,
}

impl CrateRunInfo {
    /// Gather all the information we need.
    fn collect(args: env::Args) -> Self {
        let args = args.collect();
        let env = env::vars_os().collect();
        CrateRunInfo { args, env }
    }

    fn store(&self, filename: &Path) {
        let file = File::create(filename)
            .unwrap_or_else(|_| show_error(format!("Cannot create `{}`", filename.display())));
        let file = BufWriter::new(file);
        serde_json::ser::to_writer(file, self)
            .unwrap_or_else(|_| show_error(format!("Cannot write to `{}`", filename.display())));
    }
}

fn show_help() {
    println!("{}", CARGO_MIRI_HELP);
}

fn show_version() {
    println!(
        "miri {} ({} {})",
        env!("CARGO_PKG_VERSION"),
        env!("VERGEN_SHA_SHORT"),
        env!("VERGEN_COMMIT_DATE")
    );
}

fn show_error(msg: String) -> ! {
    eprintln!("fatal error: {}", msg);
    std::process::exit(1)
}

// Determines whether a `--flag` is present.
fn has_arg_flag(name: &str) -> bool {
    let mut args = std::env::args().take_while(|val| val != "--");
    args.any(|val| val == name)
}

/// Gets the value of a `--flag`.
fn get_arg_flag_value(name: &str) -> Option<String> {
    // Stop searching at `--`.
    let mut args = std::env::args().take_while(|val| val != "--");
    loop {
        let arg = match args.next() {
            Some(arg) => arg,
            None => return None,
        };
        if !arg.starts_with(name) {
            continue;
        }
        // Strip leading `name`.
        let suffix = &arg[name.len()..];
        if suffix.is_empty() {
            // This argument is exactly `name`; the next one is the value.
            return args.next();
        } else if suffix.starts_with('=') {
            // This argument is `name=value`; get the value.
            // Strip leading `=`.
            return Some(suffix[1..].to_owned());
        }
    }
}

/// Returns the path to the `miri` binary
fn find_miri() -> PathBuf {
    if let Some(path) = env::var_os("MIRI") {
        return path.into();
    }
    let mut path = std::env::current_exe().expect("current executable path invalid");
    path.set_file_name("miri");
    path
}

fn miri() -> Command {
    Command::new(find_miri())
}

fn version_info() -> VersionMeta {
    VersionMeta::for_command(miri()).expect("failed to determine underlying rustc version of Miri")
}

fn cargo() -> Command {
    Command::new(env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")))
}

fn xargo_check() -> Command {
    Command::new(env::var_os("XARGO_CHECK").unwrap_or_else(|| OsString::from("xargo-check")))
}

/// Execute the command if it fails, fail this process with the same exit code.
/// Otherwise, continue.
fn exec(mut cmd: Command) {
    let exit_status = cmd.status().expect("failed to run command");
    if exit_status.success().not() {
        std::process::exit(exit_status.code().unwrap_or(-1))
    }
}

fn xargo_version() -> Option<(u32, u32, u32)> {
    let out = xargo_check().arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    // Parse output. The first line looks like "xargo 0.3.12 (b004f1c 2018-12-13)".
    let line = out
        .stderr
        .lines()
        .nth(0)
        .expect("malformed `xargo --version` output: not at least one line")
        .expect("malformed `xargo --version` output: error reading first line");
    let (name, version) = {
        let mut split = line.split(' ');
        (
            split.next().expect("malformed `xargo --version` output: empty"),
            split.next().expect("malformed `xargo --version` output: not at least two words"),
        )
    };
    if name != "xargo" {
        // This is some fork of xargo
        return None;
    }
    let mut version_pieces = version.split('.');
    let major = version_pieces
        .next()
        .expect("malformed `xargo --version` output: not a major version piece")
        .parse()
        .expect("malformed `xargo --version` output: major version is not an integer");
    let minor = version_pieces
        .next()
        .expect("malformed `xargo --version` output: not a minor version piece")
        .parse()
        .expect("malformed `xargo --version` output: minor version is not an integer");
    let patch = version_pieces
        .next()
        .expect("malformed `xargo --version` output: not a patch version piece")
        .parse()
        .expect("malformed `xargo --version` output: patch version is not an integer");
    if !version_pieces.next().is_none() {
        panic!("malformed `xargo --version` output: more than three pieces in version");
    }
    Some((major, minor, patch))
}

fn ask_to_run(mut cmd: Command, ask: bool, text: &str) {
    // Disable interactive prompts in CI (GitHub Actions, Travis, AppVeyor, etc).
    // Azure doesn't set `CI` though (nothing to see here, just Microsoft being Microsoft),
    // so we also check their `TF_BUILD`.
    let is_ci = env::var_os("CI").is_some() || env::var_os("TF_BUILD").is_some();
    if ask && !is_ci {
        let mut buf = String::new();
        print!("I will run `{:?}` to {}. Proceed? [Y/n] ", cmd, text);
        io::stdout().flush().unwrap();
        io::stdin().read_line(&mut buf).unwrap();
        match buf.trim().to_lowercase().as_ref() {
            // Proceed.
            "" | "y" | "yes" => {}
            "n" | "no" => show_error(format!("Aborting as per your request")),
            a => show_error(format!("I do not understand `{}`", a)),
        };
    } else {
        println!("Running `{:?}` to {}.", cmd, text);
    }

    if cmd.status().expect(&format!("failed to execute {:?}", cmd)).success().not() {
        show_error(format!("Failed to {}", text));
    }
}

/// Performs the setup required to make `cargo miri` work: Getting a custom-built libstd. Then sets
/// `MIRI_SYSROOT`. Skipped if `MIRI_SYSROOT` is already set, in which case we expect the user has
/// done all this already.
fn setup(subcommand: MiriCommand) {
    if std::env::var_os("MIRI_SYSROOT").is_some() {
        if subcommand == MiriCommand::Setup {
            println!("WARNING: MIRI_SYSROOT already set, not doing anything.")
        }
        return;
    }

    // Subcommands other than `setup` will do a setup if necessary, but
    // interactively confirm first.
    let ask_user = subcommand != MiriCommand::Setup;

    // First, we need xargo.
    if xargo_version().map_or(true, |v| v < XARGO_MIN_VERSION) {
        if std::env::var_os("XARGO_CHECK").is_some() {
            // The user manually gave us a xargo binary; don't do anything automatically.
            show_error(format!("Your xargo is too old; please upgrade to the latest version"))
        }
        let mut cmd = cargo();
        cmd.args(&["install", "xargo", "-f"]);
        ask_to_run(cmd, ask_user, "install a recent enough xargo");
    }

    // Determine where the rust sources are located.  `XARGO_RUST_SRC` env var trumps everything.
    let rust_src = match std::env::var_os("XARGO_RUST_SRC") {
        Some(path) => {
            let path = PathBuf::from(path);
            // Make path absolute if possible.
            path.canonicalize().unwrap_or(path)
        }
        None => {
            // Check for `rust-src` rustup component.
            let sysroot = miri()
                .args(&["--print", "sysroot"])
                .output()
                .expect("failed to determine sysroot")
                .stdout;
            let sysroot = std::str::from_utf8(&sysroot).unwrap();
            let sysroot = Path::new(sysroot.trim_end_matches('\n'));
            // Check for `$SYSROOT/lib/rustlib/src/rust/library`; test if that contains `std/Cargo.toml`.
            let rustup_src =
                sysroot.join("lib").join("rustlib").join("src").join("rust").join("library");
            if !rustup_src.join("std").join("Cargo.toml").exists() {
                // Ask the user to install the `rust-src` component, and use that.
                let mut cmd = Command::new("rustup");
                cmd.args(&["component", "add", "rust-src"]);
                ask_to_run(
                    cmd,
                    ask_user,
                    "install the `rust-src` component for the selected toolchain",
                );
            }
            rustup_src
        }
    };
    if !rust_src.exists() {
        show_error(format!("Given Rust source directory `{}` does not exist.", rust_src.display()));
    }

    // Next, we need our own libstd. Prepare a xargo project for that purpose.
    // We will do this work in whatever is a good cache dir for this platform.
    let dirs = directories::ProjectDirs::from("org", "rust-lang", "miri").unwrap();
    let dir = dirs.cache_dir();
    if !dir.exists() {
        fs::create_dir_all(&dir).unwrap();
    }
    // The interesting bit: Xargo.toml
    File::create(dir.join("Xargo.toml"))
        .unwrap()
        .write_all(
            br#"
[dependencies.std]
default_features = false
# We need the `panic_unwind` feature because we use the `unwind` panic strategy.
# Using `abort` works for libstd, but then libtest will not compile.
features = ["panic_unwind"]

[dependencies.test]
"#,
        )
        .unwrap();
    // The boring bits: a dummy project for xargo.
    // FIXME: With xargo-check, can we avoid doing this?
    File::create(dir.join("Cargo.toml"))
        .unwrap()
        .write_all(
            br#"
[package]
name = "miri-xargo"
description = "A dummy project for building libstd with xargo."
version = "0.0.0"

[lib]
path = "lib.rs"
"#,
        )
        .unwrap();
    File::create(dir.join("lib.rs")).unwrap();

    // Determine architectures.
    // We always need to set a target so rustc bootstrap can tell apart host from target crates.
    let host = version_info().host;
    let target = get_arg_flag_value("--target");
    let target = target.as_ref().unwrap_or(&host);
    // Now invoke xargo.
    let mut command = xargo_check();
    command.arg("check").arg("-q");
    command.arg("--target").arg(target);
    command.current_dir(&dir);
    command.env("XARGO_HOME", &dir);
    command.env("XARGO_RUST_SRC", &rust_src);
    // Use Miri as rustc to build a libstd compatible with us (and use the right flags).
    // However, when we are running in bootstrap, we cannot just overwrite `RUSTC`,
    // because we still need bootstrap to distinguish between host and target crates.
    // In that case we overwrite `RUSTC_REAL` instead which determines the rustc used
    // for target crates.
    if env::var_os("RUSTC_STAGE").is_some() {
        command.env("RUSTC_REAL", find_miri());
    } else {
        command.env("RUSTC", find_miri());
    }
    command.env("MIRI_BE_RUSTC", "1");
    // Make sure there are no other wrappers or flags getting in our way
    // (Cc https://github.com/rust-lang/miri/issues/1421).
    // This is consistent with normal `cargo build` that does not apply `RUSTFLAGS`
    // to the sysroot either.
    command.env_remove("RUSTC_WRAPPER");
    command.env_remove("RUSTFLAGS");
    // Finally run it!
    if command.status().expect("failed to run xargo").success().not() {
        show_error(format!("Failed to run xargo"));
    }

    // That should be it! But we need to figure out where xargo built stuff.
    // Unfortunately, it puts things into a different directory when the
    // architecture matches the host.
    let sysroot = if target == &host { dir.join("HOST") } else { PathBuf::from(dir) };
    std::env::set_var("MIRI_SYSROOT", &sysroot); // pass the env var to the processes we spawn, which will turn it into "--sysroot" flags
    // Figure out what to print.
    let print_sysroot = subcommand == MiriCommand::Setup && has_arg_flag("--print-sysroot"); // whether we just print the sysroot path
    if print_sysroot {
        // Print just the sysroot and nothing else; this way we do not need any escaping.
        println!("{}", sysroot.display());
    } else if subcommand == MiriCommand::Setup {
        println!("A libstd for Miri is now available in `{}`.", sysroot.display());
    }
}

fn phase_cargo_miri(mut args: env::Args) {
    // Check for version and help flags even when invoked as `cargo-miri`.
    if has_arg_flag("--help") || has_arg_flag("-h") {
        show_help();
        return;
    }
    if has_arg_flag("--version") || has_arg_flag("-V") {
        show_version();
        return;
    }

    // Require a subcommand before any flags.
    // We cannot know which of those flags take arguments and which do not,
    // so we cannot detect subcommands later.
    let subcommand = match args.next().as_deref() {
        Some("test") => MiriCommand::Test,
        Some("run") => MiriCommand::Run,
        Some("setup") => MiriCommand::Setup,
        // Invalid command.
        _ => show_error(format!("`cargo miri` supports the following subcommands: `run`, `test`, and `setup`.")),
    };
    let verbose = has_arg_flag("-v");

    // We always setup.
    setup(subcommand);

    // Invoke actual cargo for the job, but with different flags.
    let miri_path = std::env::current_exe().expect("current executable path invalid");
    let cargo_cmd = match subcommand {
        MiriCommand::Test => "test",
        MiriCommand::Run => "run",
        MiriCommand::Setup => return, // `cargo miri setup` stops here.
    };
    let mut cmd = cargo();
    cmd.arg(cargo_cmd);

    // Make sure we know the build target, and cargo does, too.
    // This is needed to make the `CARGO_TARGET_*_RUNNER` env var do something,
    // and it later helps us detect which crates are proc-macro/build-script
    // (host crates) and which crates are needed for the program itself.
    let target = if let Some(target) = get_arg_flag_value("--target") {
        target
    } else {
        // No target given. Pick default and tell cargo about it.
        let host = version_info().host;
        cmd.arg("--target");
        cmd.arg(&host);
        host
    };

    // Forward all further arguments. We do some processing here because we want to
    // detect people still using the old way of passing flags to Miri
    // (`cargo miri -- -Zmiri-foo`).
    while let Some(arg) = args.next() {
        cmd.arg(&arg);
        if arg == "--" {
            // Check if the next argument starts with `-Zmiri`. If yes, we assume
            // this is an old-style invocation.
            if let Some(next_arg) = args.next() {
                if next_arg.starts_with("-Zmiri") {
                    eprintln!(
                        "WARNING: it seems like you are setting Miri's flags in `cargo miri` the old way,\n\
                        i.e., by passing them after the first `--`. This style is deprecated; please set\n\
                        the MIRIFLAGS environment variable instead. `cargo miri run/test` now interprets\n\
                        arguments the exact same way as `cargo run/test`."
                    );
                    // Old-style invocation. Turn these into MIRIFLAGS.
                    let mut miriflags = env::var("MIRIFLAGS").unwrap_or_default();
                    miriflags.push(' ');
                    miriflags.push_str(&next_arg);
                    while let Some(further_arg) = args.next() {
                        if further_arg == "--" {
                            // End of the Miri flags!
                            break;
                        }
                        miriflags.push(' ');
                        miriflags.push_str(&further_arg);
                    }
                    env::set_var("MIRIFLAGS", miriflags);
                    // Pass the remaining flags to cargo.
                    cmd.args(args);
                    break;
                }
                // Not a Miri argument after all, make sure we pass it to cargo.
                cmd.arg(next_arg);
            }
        }
    }

    // Set `RUSTC_WRAPPER` to ourselves.  Cargo will prepend that binary to its usual invocation,
    // i.e., the first argument is `rustc` -- which is what we use in `main` to distinguish
    // the two codepaths. (That extra argument is why we prefer this over setting `RUSTC`.)
    if env::var_os("RUSTC_WRAPPER").is_some() {
        println!("WARNING: Ignoring `RUSTC_WRAPPER` environment variable, Miri does not support wrapping.");
    }
    cmd.env("RUSTC_WRAPPER", &miri_path);
    if verbose {
        eprintln!("+ RUSTC_WRAPPER={:?}", miri_path);
    }

    // Set the runner for the current target to us as well, so we can interpret the binaries.
    let runner_env_name = format!("CARGO_TARGET_{}_RUNNER", target.to_uppercase().replace('-', "_"));
    cmd.env(runner_env_name, &miri_path);

    // Set rustdoc to us as well, so we can make it do nothing (see issue #584).
    cmd.env("RUSTDOC", &miri_path);

    // Run cargo.
    if verbose {
        cmd.env("MIRI_VERBOSE", ""); // This makes the other phases verbose.
        eprintln!("[cargo-miri miri] {:?}", cmd);
    }
    exec(cmd)
}

fn phase_cargo_rustc(args: env::Args) {
    /// Determines if we are being invoked (as rustc) to build a crate for
    /// the "target" architecture, in contrast to the "host" architecture.
    /// Host crates are for build scripts and proc macros and still need to
    /// be built like normal; target crates need to be built for or interpreted
    /// by Miri.
    ///
    /// Currently, we detect this by checking for "--target=", which is
    /// never set for host crates. This matches what rustc bootstrap does,
    /// which hopefully makes it "reliable enough". This relies on us always
    /// invoking cargo itself with `--target`, which `in_cargo_miri` ensures.
    fn is_target_crate() -> bool {
        get_arg_flag_value("--target").is_some()
    }

    /// Returns whether or not Cargo invoked the wrapper (this binary) to compile
    /// the final, binary crate (either a test for 'cargo test', or a binary for 'cargo run')
    /// Cargo does not give us this information directly, so we need to check
    /// various command-line flags.
    fn is_runnable_crate() -> bool {
        let is_bin = get_arg_flag_value("--crate-type").as_deref() == Some("bin");
        let is_test = has_arg_flag("--test");
        let print = get_arg_flag_value("--print").is_some();
        (is_bin || is_test) && !print
    }

    fn out_filename(prefix: &str, suffix: &str) -> PathBuf {
        let mut path = PathBuf::from(get_arg_flag_value("--out-dir").unwrap());
        path.push(format!(
            "{}{}{}{}",
            prefix,
            get_arg_flag_value("--crate-name").unwrap(),
            // This is technically a `-C` flag but the prefix seems unique enough...
            // (and cargo passes this before the filename so it should be unique)
            get_arg_flag_value("extra-filename").unwrap_or(String::new()),
            suffix,
        ));
        path
    }

    let verbose = std::env::var_os("MIRI_VERBOSE").is_some();
    let target_crate = is_target_crate();

    if target_crate && is_runnable_crate() {
        // This is the binary or test crate that we want to interpret under Miri.
        // But we cannot run it here, as cargo invoked us as a compiler -- our stdin and stdout are not
        // like we want them.
        // Instead of compiling, we write JSON into the output file with all the relevant command-line flags
        // and environment variables; this is used when cargo calls us again in the CARGO_TARGET_RUNNER phase.
        let info = CrateRunInfo::collect(args);
        let filename = out_filename("", "");
        if verbose {
            eprintln!("[cargo-miri rustc] writing run info to `{}`", filename.display());
        }

        info.store(&filename);
        // For Windows, do the same thing again with `.exe` appended to the filename.
        // (Need to do this here as cargo moves that "binary" to a different place before running it.)
        info.store(&out_filename("", ".exe"));

        return;
    }

    let mut cmd = miri();
    let mut emit_link_hack = false;
    // Arguments are treated very differently depending on whether this crate is
    // for interpretation by Miri, or for use by a build script / proc macro.
    if target_crate {
        // Forward arguments, but remove "link" from "--emit" to make this a check-only build.
        let emit_flag = "--emit";
        for arg in args {
            if arg.starts_with(emit_flag) {
                // Patch this argument. First, extract its value.
                let val = &arg[emit_flag.len()..];
                assert!(val.starts_with("="), "`cargo` should pass `--emit=X` as one argument");
                let val = &val[1..];
                let mut val: Vec<_> = val.split(',').collect();
                // Now make sure "link" is not in there, but "metadata" is.
                if let Some(i) = val.iter().position(|&s| s == "link") {
                    emit_link_hack = true;
                    val.remove(i);
                    if !val.iter().any(|&s| s == "metadata") {
                        val.push("metadata");
                    }
                }
                cmd.arg(format!("{}={}", emit_flag, val.join(",")));
            } else {
                cmd.arg(arg);
            }
        }

        // Use our custom sysroot.
        let sysroot =
            env::var_os("MIRI_SYSROOT").expect("The wrapper should have set MIRI_SYSROOT");
        cmd.arg("--sysroot");
        cmd.arg(sysroot);
    } else {
        // For host crates, just forward everything.
        cmd.args(args);
    }

    // We want to compile, not interpret. We still use Miri to make sure the compiler version etc
    // are the exact same as what is used for interpretation.
    cmd.env("MIRI_BE_RUSTC", "1");

    // Run it.
    if verbose {
        eprintln!("[cargo-miri rustc] {:?}", cmd);
    }
    exec(cmd);

    // Create a stub .rlib file if "link" was requested by cargo.
    if emit_link_hack {
        // Some platforms prepend "lib", some do not... let's just create both files.
        let filename = out_filename("lib", ".rlib");
        File::create(filename).expect("Failed to create rlib file");
        let filename = out_filename("", ".rlib");
        File::create(filename).expect("Failed to create rlib file");
    }
}

fn phase_cargo_runner(binary: &Path, binary_args: env::Args) {
    let verbose = std::env::var_os("MIRI_VERBOSE").is_some();

    let file = File::open(&binary)
        .unwrap_or_else(|_| show_error(format!("File {:?} not found or `cargo-miri` invoked incorrectly; please only invoke this binary through `cargo miri`", binary)));
    let file = BufReader::new(file);
    let info: CrateRunInfo = serde_json::from_reader(file)
        .unwrap_or_else(|_| show_error(format!("File {:?} does not contain valid JSON", binary)));

    // Set missing env vars.
    for (name, val) in info.env {
        if env::var_os(&name).is_none() {
            env::set_var(name, val);
        }
    }

    let mut cmd = miri();
    // Forward rustc arguments.
    // We need to patch "--extern" filenames because we forced a check-only
    // build without cargo knowing about that: replace `.rlib` suffix by
    // `.rmeta`.
    // We also need to remove `--error-format` as cargo specifies that to be JSON,
    // but when we run here, cargo does not interpret the JSON any more. `--json`
    // then also nees to be dropped.
    let mut args = info.args.into_iter();
    let extern_flag = "--extern";
    let error_format_flag = "--error-format";
    let json_flag = "--json";
    while let Some(arg) = args.next() {
        if arg == extern_flag {
            // `--extern` is always passed as a separate argument by cargo.
            let next_arg = args.next().expect("`--extern` should be followed by a filename");
            let next_arg = next_arg.strip_suffix(".rlib").expect("all extern filenames should end in `.rlib`");
            cmd.arg(extern_flag);
            cmd.arg(format!("{}.rmeta", next_arg));
        } else if arg.starts_with(error_format_flag) {
            let suffix = &arg[error_format_flag.len()..];
            assert!(suffix.starts_with('='));
            // Drop this argument.
        } else if arg.starts_with(json_flag) {
            let suffix = &arg[json_flag.len()..];
            assert!(suffix.starts_with('='));
            // Drop this argument.
        } else {
            cmd.arg(arg);
        }
    }
    // Set sysroot.
    let sysroot =
        env::var_os("MIRI_SYSROOT").expect("The wrapper should have set MIRI_SYSROOT");
    cmd.arg("--sysroot");
    cmd.arg(sysroot);
    // Respect `MIRIFLAGS`.
    if let Ok(a) = env::var("MIRIFLAGS") {
        // This code is taken from `RUSTFLAGS` handling in cargo.
        let args = a
            .split(' ')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        cmd.args(args);
    }

    // Then pass binary arguments.
    cmd.arg("--");
    cmd.args(binary_args);

    // Run it.
    if verbose {
        eprintln!("[cargo-miri runner] {:?}", cmd);
    }
    exec(cmd)
}

fn main() {
    // Rustc does not support non-UTF-8 arguments so we make no attempt either.
    // (We do support non-UTF-8 environment variables though.)
    let mut args = std::env::args();
    // Skip binary name.
    args.next().unwrap();

    // Dispatch to `cargo-miri` phase. There are three phases:
    // - When we are called via `cargo miri`, we run as the frontend and invoke the underlying
    //   cargo. We set RUSTC_WRAPPER and CARGO_TARGET_RUNNER to ourselves.
    // - When we are executed due to RUSTC_WRAPPER, we build crates or store the flags of
    //   binary crates for later interpretation.
    // - When we are executed due to CARGO_TARGET_RUNNER, we start interpretation based on the
    //   flags that were stored earlier.
    // On top of that, we are also called as RUSTDOC, but that is just a stub currently.
    match args.next().as_deref() {
        Some("miri") => phase_cargo_miri(args),
        Some("rustc") => phase_cargo_rustc(args),
        Some(arg) => {
            // We have to distinguish the "runner" and "rustfmt" cases.
            // As runner, the first argument is the binary (a file that should exist, with an absolute path);
            // as rustfmt, the first argument is a flag (`--something`).
            let binary = Path::new(arg);
            if binary.exists() {
                assert!(!arg.starts_with("--")); // not a flag
                phase_cargo_runner(binary, args);
            } else if arg.starts_with("--") {
                // We are rustdoc.
                eprintln!("Running doctests is not currently supported by Miri.")
            } else {
                show_error(format!("`cargo-miri` called with unexpected first argument `{}`; please only invoke this binary through `cargo miri`", arg));
            }
        }
        _ => show_error(format!("`cargo-miri` called without first argument; please only invoke this binary through `cargo miri`")),
    }
}
