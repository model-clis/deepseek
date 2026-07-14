use clap::{ArgGroup, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "deepseek",
    version,
    about = "Stateless DeepSeek subagent CLI",
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true
)]
#[command(group(ArgGroup::new("input").args(["prompt", "prompt_file"]).required(true).multiple(false)))]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[arg(value_name = "PROMPT")]
    pub prompt: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub prompt_file: Option<PathBuf>,
    #[arg(long, requires = "prompt_file")]
    pub delete_prompt_file: bool,
    #[arg(long, default_value_t=128, value_parser=clap::value_parser!(u32).range(1..))]
    pub max_turns: u32,
}

#[derive(Subcommand)]
pub enum Command {
    Login,
    Logout,
}
