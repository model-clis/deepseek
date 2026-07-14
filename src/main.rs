mod agent;
mod api;
mod cli;
mod config;
mod tools;

use anyhow::{Context, Result, bail};
use clap::{Parser, error::ErrorKind};
use cli::{Cli, Command};
use std::{path::PathBuf, process::ExitCode};

#[tokio::main]
async fn main() -> ExitCode {
    let task = run();
    tokio::pin!(task);
    tokio::select! {
        result = &mut task => match result {
            Ok(outcome) => { if let Some(report) = outcome.report { print!("{report}"); } ExitCode::from(outcome.exit_code) },
            Err(e) => { eprintln!("Error: {e:#}"); ExitCode::from(1) }
        },
        _ = tokio::signal::ctrl_c() => ExitCode::from(130),
    }
}

async fn run() -> Result<agent::Outcome> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            e.print()?;
            return Ok(agent::Outcome {
                exit_code: 0,
                report: None,
            });
        }
        Err(e) => {
            e.print()?;
            return Ok(agent::Outcome {
                exit_code: 1,
                report: None,
            });
        }
    };
    match cli.command {
        Some(Command::Login) => {
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
            eprintln!("Login successful");
            Ok(agent::Outcome {
                exit_code: 0,
                report: None,
            })
        }
        Some(Command::Logout) => {
            config::logout()?;
            Ok(agent::Outcome {
                exit_code: 0,
                report: None,
            })
        }
        None => {
            let cwd = std::env::current_dir()?;
            let prompt = load_prompt(cli.prompt, cli.prompt_file, cli.delete_prompt_file, &cwd)?;
            let key = config::load_key()?;
            let client = api::Client::new(key, api::DEFAULT_BASE)?;
            agent::run(client, prompt, cwd, cli.max_turns).await
        }
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
}
