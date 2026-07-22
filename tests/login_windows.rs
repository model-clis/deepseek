#![cfg(windows)]

use std::process::Command;

#[test]
fn login_reports_missing_power_shell_7_before_prompting() {
    let output = Command::new(env!("CARGO_BIN_EXE_deepseek"))
        .arg("login")
        .env("PATH", "")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("PowerShell 7+ (`pwsh.exe`) is required"));
    assert!(stderr.contains("then rerun deepseek login"));
    assert!(!stderr.contains("DeepSeek API key:"));
}
