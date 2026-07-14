fn main() {
    use std::process::Command;

    fn git_output(args: &[&str]) -> Option<String> {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    fn track_git_path(spec: &str) {
        if let Some(path) = git_output(&["rev-parse", "--git-path", spec]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let commit = std::env::var("CENTAURAI_CORE_COMMIT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| git_output(&["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=BUILD_TIME={ts}");
    println!("cargo:rustc-env=CENTAURAI_CORE_COMMIT={commit}");
    println!("cargo:rerun-if-env-changed=CENTAURAI_CORE_COMMIT");
    track_git_path("HEAD");
    track_git_path("packed-refs");
    if let Some(reference) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        track_git_path(&reference);
    }
    println!("cargo:rerun-if-changed=build.rs");
}
