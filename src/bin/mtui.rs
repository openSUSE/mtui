//! `mtui` — the interactive REPL binary. See [`mtui_cli::run`] for the entry
//! point implementation.

fn main() -> anyhow::Result<()> {
    mtui_cli::run()
}
