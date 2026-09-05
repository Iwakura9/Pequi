//! Command-line adapter for `peq`.
//!
//! Parsing and presentation stay here; state-changing operations are delegated to
//! [`crate::application`] so the TUI and future daemon client share their behavior.

use crate::{application, chain, dsp, preset, render, tui};
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "peq",
    about = "Parametric EQ for the terminal, backed by PipeWire's filter-chain"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List available presets (one per line)
    Ls,
    /// Bypass: zero all gains, keep the active preset remembered
    Off,
    /// Undo `off`: re-apply the active preset
    On,
    /// Cycle to the next preset
    Next,
    /// Print a preset's curve without applying it
    Show { name: String },
    /// Open the TUI on a preset
    Edit { name: String },
    /// Import an AutoEQ ParametricEQ.txt file as a preset
    Import {
        file: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
    /// Generate the filter-chain config
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Print the active preset (for waybar etc.)
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Print shell completions
    Completions { shell: clap_complete::Shell },
    /// (implicit) apply a preset by name - `peq <name>`
    #[command(external_subcommand)]
    Apply(Vec<String>),
}

/// Parse command-line arguments and dispatch the selected command.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => print_active(),
        Some(Command::Ls) => cmd_ls(),
        Some(Command::Off) => cmd_off(),
        Some(Command::On) => cmd_on(),
        Some(Command::Next) => cmd_next(),
        Some(Command::Show { name }) => cmd_show(&name),
        Some(Command::Edit { name }) => cmd_edit(&name),
        Some(Command::Import { file, name }) => cmd_import(&file, name.as_deref()),
        Some(Command::Init { force }) => cmd_init(force),
        Some(Command::Status { json }) => cmd_status(json),
        Some(Command::Completions { shell }) => cmd_completions(shell),
        Some(Command::Apply(args)) => match args.as_slice() {
            [name] => cmd_apply(name),
            _ => bail!("usage: peq <preset-name>"),
        },
    }
}

fn resolve(query: &str) -> Result<String> {
    let available = preset::list_presets()?;
    if available.is_empty() {
        bail!("no presets yet - see `peq import <file>`");
    }
    preset::resolve_name(query, &available).map_err(Into::into)
}

fn cmd_apply(query: &str) -> Result<()> {
    let name = resolve(query)?;
    let preset = preset::load_preset(&name)?;
    application::apply_preset(&preset)?;
    println!("applied {name} (preamp {:.1} dB)", preset.preamp_db);
    Ok(())
}

fn print_active() -> Result<()> {
    match preset::read_active() {
        None => {
            println!("no active preset - run `peq <name>` (see `peq ls`)");
            Ok(())
        }
        Some(name) => match preset::load_preset(&name) {
            Ok(preset) => {
                print_preset(&preset);
                Ok(())
            }
            Err(_) => {
                println!("active preset '{name}' no longer exists on disk");
                Ok(())
            }
        },
    }
}

fn cmd_show(query: &str) -> Result<()> {
    let name = resolve(query)?;
    let preset = preset::load_preset(&name)?;
    print_preset(&preset);
    Ok(())
}

fn print_preset(preset: &preset::Preset) {
    println!("{}  (preamp {:.1} dB)", preset.name, preset.preamp_db);
    let width = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
        .clamp(20, 120);
    let lines = render::plot(|f| response_db(preset, f), width, 12);
    for line in lines {
        println!("{line}");
    }
}

fn response_db(preset: &preset::Preset, freq: f64) -> f64 {
    preset.preamp_db
        + preset
            .bands
            .iter()
            .filter(|b| b.enabled)
            .map(|b| dsp::band_response_db(b.kind, b.freq, b.gain, b.q, freq))
            .sum::<f64>()
}

fn cmd_ls() -> Result<()> {
    for name in preset::list_presets()? {
        println!("{name}");
    }
    Ok(())
}

fn cmd_off() -> Result<()> {
    let name = preset::read_active().unwrap_or_else(|| "off".to_string());
    application::bypass(true)?;
    println!("bypassed (preset '{name}' remembered)");
    Ok(())
}

fn cmd_on() -> Result<()> {
    application::bypass(false)?;
    let name = preset::read_active().unwrap_or_else(|| "unknown".to_string());
    println!("restored {name}");
    Ok(())
}

fn cmd_next() -> Result<()> {
    let available = preset::list_presets()?;
    if available.is_empty() {
        bail!("no presets yet - see `peq import <file>`");
    }
    let current = preset::read_active();
    let next_index = match current
        .as_deref()
        .and_then(|c| available.iter().position(|n| n == c))
    {
        Some(i) => (i + 1) % available.len(),
        None => 0,
    };
    cmd_apply(&available[next_index])
}

fn cmd_edit(query: &str) -> Result<()> {
    let name = resolve(query)?;
    let preset = preset::load_preset(&name)?;
    tui::run(preset)
}

fn cmd_import(file: &PathBuf, name: Option<&str>) -> Result<()> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let import = preset::parse_autoeq(&text);
    for w in &import.warnings {
        eprintln!("warning: {w}");
    }
    let derived = name.map(str::to_string).unwrap_or_else(|| {
        file.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "imported".to_string())
    });
    let preset = preset::Preset {
        preamp_db: import.preamp_db,
        bands: import.bands,
        ..preset::Preset::new(derived.clone())
    };
    preset::save_preset(&preset)?;
    println!("saved preset '{derived}' ({} bands)", preset.bands.len());
    Ok(())
}

fn cmd_init(force: bool) -> Result<()> {
    let path = chain::config_file_path();
    if path.exists() && !force {
        bail!(
            "{} already exists, pass --force to overwrite",
            path.display()
        );
    }
    std::fs::create_dir_all(preset::preset_dir()).context("creating preset dir")?;
    std::fs::create_dir_all(preset::state_dir()).context("creating state dir")?;
    chain::write_config(&chain::silent_preset("none"))?;
    println!("wrote {}", path.display());
    println!();
    println!("reload PipeWire to activate it:");
    println!("    systemctl --user restart pipewire pipewire.socket wireplumber");
    println!();
    println!("then make peq's sink the default:");
    println!("    wpctl set-default $(wpctl status | grep -m1 peq | grep -oE '[0-9]+')");
    println!("(apps already playing won't move automatically - see README)");
    Ok(())
}

fn cmd_status(json: bool) -> Result<()> {
    let snapshot = application::status();
    let text = snapshot.text();
    let tooltip = snapshot.tooltip();
    let class = snapshot.class();

    if json {
        println!(
            "{{\"text\":\"{}\",\"tooltip\":\"{}\",\"class\":\"{class}\"}}",
            escape_json(text),
            escape_json(&tooltip)
        );
    } else {
        println!("{text}\t{tooltip}\t{class}");
    }
    Ok(())
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn cmd_completions(shell: clap_complete::Shell) -> Result<()> {
    let mut cmd = <Cli as clap::CommandFactory>::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
    if shell == clap_complete::Shell::Fish {
        println!("complete -c peq -a \"(peq ls)\" -d 'preset'");
    }
    Ok(())
}
