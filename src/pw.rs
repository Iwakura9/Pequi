//! PipeWire lifecycle: checking whether the `peq` sink is up, and reloading it.
//!
//! See NOTES.md: runtime `Props` writes never reach the filter-chain graph on this
//! PipeWire build, so "apply a preset" means regenerating the config (chain.rs) and
//! restarting the PipeWire user session - not a live write. This module only reads
//! (does the sink exist?) and drives that restart.

use crate::chain::SINK_NODE_NAME;
use anyhow::{bail, Context, Result};
use std::process::Command;
use std::time::{Duration, Instant};

const SINK_QUERY_TIMEOUT: Duration = Duration::from_millis(500);

/// Connects briefly and reports whether a node named [`SINK_NODE_NAME`] is registered.
/// Returns `Ok(false)` (not an error) if PipeWire just isn't running the sink; returns
/// `Err` only if we couldn't connect to PipeWire at all.
pub fn sink_exists() -> Result<bool> {
    use pipewire as pw;
    use std::cell::Cell;
    use std::rc::Rc;

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("creating mainloop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("creating context")?;
    let core = context.connect_rc(None).context("connecting to PipeWire")?;
    let registry = core.get_registry_rc().context("getting registry")?;

    let found = Rc::new(Cell::new(false));
    let found_cb = found.clone();
    let _listener_reg = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ == pw::types::ObjectType::Node {
                if let Some(props) = &global.props {
                    if props.get("node.name") == Some(SINK_NODE_NAME) {
                        found_cb.set(true);
                    }
                }
            }
        })
        .register();

    let done = Rc::new(Cell::new(false));
    let done_cb = done.clone();
    let pending = core.sync(0).context("sync")?;
    let _listener_core = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                done_cb.set(true);
            }
        })
        .register();

    let deadline = Instant::now() + SINK_QUERY_TIMEOUT;
    while !done.get() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(remaining));
    }
    Ok(found.get())
}

/// Restart the PipeWire user session so the regenerated filter-chain config takes effect,
/// then poll for the `peq` sink to reappear. This is the documented fallback (NOTES.md) -
/// it audibly interrupts all system audio briefly, not just `peq`'s own sink.
pub fn reload() -> Result<()> {
    let status = Command::new("systemctl")
        .args([
            "--user",
            "restart",
            "pipewire",
            "pipewire.socket",
            "wireplumber",
        ])
        .status()
        .context("running systemctl --user restart")?;
    if !status.success() {
        bail!("systemctl --user restart pipewire failed: {status}");
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if sink_exists().unwrap_or(false) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("pipewire restarted but the `{SINK_NODE_NAME}` sink never came back");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
