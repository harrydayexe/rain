//! rain — Rust Automated Issue eNgine.
//!
//! Give it issue numbers; it gives back pull requests that pass CI and have
//! already been through one round of automated review.

use std::process::ExitCode;

use clap::Parser;

use rain::cli::Cli;
use rain::ui;

/// Exit codes: 0 when every issue ended ready for review, 1 when some did not,
/// 2 when rain itself could not run.
fn main() -> ExitCode {
    let opts = match Cli::parse().validate() {
        Ok(opts) => opts,
        Err(e) => {
            ui::init(false);
            ui::error(&format!("{e:#}"));
            return ExitCode::from(2);
        }
    };

    ui::init(opts.verbose);

    match rain::run(opts) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            ui::error(&format!("{e:#}"));
            ExitCode::from(2)
        }
    }
}
