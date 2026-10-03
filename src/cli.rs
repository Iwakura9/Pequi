//! `peq [DIR]`: open the TUI on a directory of AutoEQ files.

use anyhow::{bail, Result};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "peq",
    about = "Pick and edit EasyEffects EQ curves from AutoEQ files"
)]
struct Cli {
    /// Directory of AutoEQ ParametricEQ.txt files [default: $XDG_CONFIG_HOME/peq/profiles]
    dir: Option<PathBuf>,
}

pub fn run() -> Result<()> {
    let dir = match Cli::parse().dir {
        Some(dir) => dir,
        None => default_dir()?,
    };
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    crate::tui::run(dir)
}

fn default_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    match base {
        Some(base) => Ok(base.join("peq/profiles")),
        None => bail!("neither XDG_CONFIG_HOME nor HOME is set"),
    }
}
