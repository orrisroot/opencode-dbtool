//! opencode-dbtool: maintenance tool for the opencode SQLite database.
//!
//! Module layout:
//! - `cli`       clap command tree (help, validation, completions)
//! - `dispatch`  command dispatch, confirmation, running-instance guards
//! - `config`    data-dir resolution
//! - `sys`       running opencode detection
//! - `db`        connection handling, generic DB helpers
//! - `repo`      all SQL against session/project tables
//! - `models`    read models + JSON conversion + filtering
//! - `commands`  one module per top-level subcommand
//! - `output`    JSON/table printing
//! - `confirm`   interactive yes/no prompt
//! - `util`      formatting helpers
//! - `error`     AppError (exit code + message)

mod cli;
mod commands;
mod config;
mod confirm;
mod db;
mod dispatch;
mod error;
mod lock;
mod models;
mod output;
mod repo;
mod service;
mod settings;
mod sys;
#[cfg(test)]
mod testdb;
mod util;

use clap::Parser;
use std::process::exit;

fn main() {
    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    match dispatch::execute(cli) {
        Ok(()) => exit(error::EXIT_OK),
        Err(e) => {
            if !e.message.is_empty() {
                eprintln!("error: {}", e.message);
            }
            exit(e.code);
        }
    }
}
