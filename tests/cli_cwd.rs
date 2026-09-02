use std::path::{Path, PathBuf};
use std::process::Command;

fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new("target/cli-cwd").join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn cli_cwd_sets_the_child_directory() {
    let dir = test_dir("sets-directory");
    let working_dir = dir.join("short-context");
    std::fs::create_dir_all(&working_dir).unwrap();
    let output = dir.join("cwd.png");

    let result = Command::new(env!("CARGO_BIN_EXE_termshot"))
        .args([
            "exec",
            "--cwd",
            working_dir.to_str().unwrap(),
            "--no-prompt",
            "--plain-text",
            "--output",
            output.to_str().unwrap(),
            "pwd",
        ])
        .output()
        .expect("run termshot");

    assert!(
        result.status.success(),
        "termshot failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let description = termshot::renderer::read_png_description(&output).expect("description");
    assert_eq!(
        description,
        working_dir.canonicalize().unwrap().display().to_string()
    );
}

#[test]
fn invalid_cli_cwd_fails_before_the_command_runs() {
    let dir = test_dir("invalid-directory");
    let missing = dir.join("missing");
    let marker = dir.join("must-not-exist");

    let result = Command::new(env!("CARGO_BIN_EXE_termshot"))
        .args([
            "exec",
            "--cwd",
            missing.to_str().unwrap(),
            "--no-prompt",
            &format!("touch {}", marker.display()),
        ])
        .output()
        .expect("run termshot");

    assert!(
        !result.status.success(),
        "invalid cwd unexpectedly succeeded"
    );
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("Working directory"),
        "unexpected error:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!marker.exists(), "command ran despite invalid cwd");
}
