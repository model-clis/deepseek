mod agent;
mod api;
mod cli;
mod config;
mod diagnostics;
mod search;
mod tools;

use anyhow::{Context, Result, bail};
use clap::{Parser, error::ErrorKind};
use cli::{Cli, Command};
use std::{
    ffi::{OsStr, OsString},
    io::Write as _,
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

#[tokio::main]
async fn main() -> ExitCode {
    diagnostics::init(capture_diagnostics_requested_from(std::env::args_os()));
    let task = run();
    tokio::pin!(task);
    let exit_code = tokio::select! {
        result = &mut task => match result {
            Ok(outcome) => { if let Some(report) = outcome.report { print!("{report}"); } outcome.exit_code },
            Err(e) => { diagnostics::log(format_args!("Error: {e:#}")); 1 }
        },
        _ = tokio::signal::ctrl_c() => { diagnostics::log(format_args!("Interrupted")); 130 },
    };
    let _ = std::io::stdout().flush();
    diagnostics::finish(exit_code);
    ExitCode::from(exit_code)
}

fn capture_diagnostics_requested_from(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter()
        .skip(1)
        .take_while(|arg| arg.as_os_str() != OsStr::new("--"))
        .any(|arg| arg.as_os_str() == OsStr::new("--capture-diagnostics"))
}

async fn run() -> Result<agent::Outcome> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            e.print()?;
            return Ok(agent::Outcome {
                exit_code: 0,
                report: None,
                usage: agent::UsageTotals::default(),
            });
        }
        Err(e) => {
            diagnostics::log(format_args!("{e}"));
            return Ok(agent::Outcome {
                exit_code: 1,
                report: None,
                usage: agent::UsageTotals::default(),
            });
        }
    };
    let _ = cli.capture_diagnostics;
    match cli.command {
        Some(Command::Login) => {
            #[cfg(windows)]
            let _ = tools::ShellInfo::detect().map_err(anyhow::Error::msg)?;
            if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                bail!("login requires an interactive terminal");
            }
            let key = rpassword::prompt_password("DeepSeek API key: ")?;
            if key.trim().is_empty() {
                bail!("API key must not be empty")
            }
            api::Client::new(key.clone(), api::DEFAULT_BASE)?
                .verify()
                .await?;
            config::save_key(&key)?;
            diagnostics::log(format_args!("Login successful"));
            Ok(agent::Outcome {
                exit_code: 0,
                report: None,
                usage: agent::UsageTotals::default(),
            })
        }
        Some(Command::Logout) => {
            config::logout()?;
            Ok(agent::Outcome {
                exit_code: 0,
                report: None,
                usage: agent::UsageTotals::default(),
            })
        }
        None => {
            let cwd = std::env::current_dir()?;
            let prompt = load_prompt(cli.prompt, cli.prompt_file, cli.delete_prompt_file, &cwd)?;
            let key = config::load_key()?;
            let client = api::Client::new(key, api::DEFAULT_BASE)?;
            run_agent(client, prompt, cwd, cli.max_turns).await
        }
    }
}

async fn run_agent(
    client: api::Client,
    prompt: String,
    cwd: PathBuf,
    max_turns: u32,
) -> Result<agent::Outcome> {
    let start = tokio::time::Instant::now();
    let mut heartbeat = Heartbeat::default();
    let mut interval =
        tokio::time::interval_at(start + Duration::from_secs(1), Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let task = agent::run(client, prompt, cwd, max_turns);
    tokio::pin!(task);

    loop {
        tokio::select! {
            result = &mut task => {
                match result {
                    Ok(outcome) => {
                        heartbeat.finish(start.elapsed().as_secs(), outcome.usage);
                        return Ok(outcome);
                    }
                    Err(error) => {
                        heartbeat.stop();
                        return Err(error);
                    }
                }
            }
            _ = interval.tick() => heartbeat.tick(),
        }
    }
}

#[derive(Default)]
struct Heartbeat {
    printed: bool,
}

impl Heartbeat {
    fn tick(&mut self) {
        print!(".");
        let _ = std::io::stdout().flush();
        self.printed = true;
    }

    fn finish(&mut self, elapsed_seconds: u64, usage: agent::UsageTotals) {
        if self.printed {
            println!();
        }
        println!("{}", final_status(elapsed_seconds, usage));
        println!("---");
        let _ = std::io::stdout().flush();
        self.printed = false;
    }

    fn stop(&mut self) {
        if self.printed {
            println!();
            let _ = std::io::stdout().flush();
            self.printed = false;
        }
    }
}

fn final_status(elapsed_seconds: u64, usage: agent::UsageTotals) -> String {
    if usage.total_tokens == 0 {
        format!("Elapsed: {elapsed_seconds}s")
    } else if usage.prompt_tokens == 0 {
        format!(
            "Elapsed: {elapsed_seconds}s | Tokens: {}",
            usage.total_tokens
        )
    } else {
        let cache_rate = usage.prompt_cache_hit_tokens as f64 * 100.0 / usage.prompt_tokens as f64;
        format!(
            "Elapsed: {elapsed_seconds}s | Tokens: {} | Cache: {cache_rate:.1}%",
            usage.total_tokens
        )
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop();
    }
}

fn load_prompt(
    prompt: Option<String>,
    file: Option<PathBuf>,
    delete: bool,
    cwd: &std::path::Path,
) -> Result<String> {
    let mut value = if let Some(prompt) = prompt {
        prompt
    } else if let Some(path) = file {
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        if delete {
            std::fs::remove_file(&path)
                .with_context(|| format!("Failed to delete {}", path.display()))?;
        }
        text
    } else {
        bail!("A prompt or --prompt-file is required")
    };
    if value.trim().is_empty() {
        bail!("prompt must not be empty")
    }
    // Preserve the user's bytes other than accepting the owned string.
    value.shrink_to_fit();
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn capture_flag_scan_stops_at_argument_delimiter() {
        assert!(capture_diagnostics_requested_from([
            OsString::from("deepseek"),
            OsString::from("--capture-diagnostics"),
            OsString::from("task"),
        ]));
        assert!(!capture_diagnostics_requested_from([
            OsString::from("deepseek"),
            OsString::from("--"),
            OsString::from("--capture-diagnostics"),
        ]));
    }

    #[test]
    fn prompt_file_deleted_after_read() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("p");
        std::fs::write(&p, "hello").unwrap();
        assert_eq!(
            load_prompt(None, Some(PathBuf::from("p")), true, d.path()).unwrap(),
            "hello"
        );
        assert!(!p.exists());
    }

    #[test]
    fn final_status_uses_actual_usage_and_hides_zero_fields() {
        assert_eq!(
            final_status(3, agent::UsageTotals::default()),
            "Elapsed: 3s"
        );
        assert_eq!(
            final_status(
                8,
                agent::UsageTotals {
                    total_tokens: 12_430,
                    ..Default::default()
                }
            ),
            "Elapsed: 8s | Tokens: 12430"
        );
        assert_eq!(
            final_status(
                18,
                agent::UsageTotals {
                    total_tokens: 24_821,
                    prompt_tokens: 20_000,
                    prompt_cache_hit_tokens: 15_860,
                }
            ),
            "Elapsed: 18s | Tokens: 24821 | Cache: 79.3%"
        );
    }
}
