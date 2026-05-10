use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    if let Err(error) = bundle() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn bundle() -> Result<(), String> {
    let root = workspace_root();
    run_command(
        Command::new("cargo")
            .arg("build")
            .arg("--release")
            .arg("--bin")
            .arg("pavlovd-rs")
            .current_dir(&root),
    )?;

    let app = root.join(".build/PavlovD-Rust.app");
    let contents = app.join("Contents");
    let macos = contents.join("MacOS");

    remove_dir_if_exists(&app)?;
    fs::create_dir_all(&macos).map_err(|error| format!("create {}: {error}", macos.display()))?;
    copy(
        root.join("target/release/pavlovd-rs"),
        macos.join("pavlovd-rs"),
    )?;
    copy(
        root.join("Resources/PavlovDRust-Info.plist"),
        contents.join("Info.plist"),
    )?;

    run_command(
        Command::new("codesign")
            .arg("--force")
            .arg("--sign")
            .arg("-")
            .arg("--entitlements")
            .arg(root.join("Resources/PavlovDRust.entitlements"))
            .arg(&app),
    )?;

    println!("{}", app.display());
    Ok(())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn remove_dir_if_exists(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove {}: {error}", path.display())),
    }
}

fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<(), String> {
    let from = from.as_ref();
    let to = to.as_ref();
    fs::copy(from, to)
        .map(|_| ())
        .map_err(|error| format!("copy {} to {}: {error}", from.display(), to.display()))
}

fn run_command(command: &mut Command) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|error| format!("run {command:?}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{command:?} exited with {status}"))
    }
}
