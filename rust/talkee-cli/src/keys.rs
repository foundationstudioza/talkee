//! `talkee keys`: see which dictation keys this PC has, and import them from a `.env` file.
//!
//! Importing copies `GROQ_API_KEY` and `OPENROUTER_API_KEY` from the first `.env` in the current
//! directory or its parents into Talkee's encrypted store (`%APPDATA%\Talkee\keys.dat`, DPAPI), where
//! the installed app reads them. Keys are never printed.

use anyhow::{bail, Context, Result};
use clap::Args;
use talkee_dictate::Keys;
use talkee_store::Store;

#[derive(Args)]
pub struct KeysArgs {
    /// Copy keys from a .env file in this folder (or a parent) into the encrypted store.
    #[arg(long)]
    import: bool,
}

pub fn run(args: KeysArgs) -> Result<()> {
    let store = Store::open()?;
    let mut saved = Keys::default();
    if let Some(text) = store.load_keys()? {
        saved.fill_from(&text);
    }
    if args.import {
        let cwd = std::env::current_dir().context("reading the current directory")?;
        let (found, file) = Keys::load(&cwd);
        let Some(file) = file else {
            bail!("no .env file in {} or its parents", cwd.display());
        };
        // The file's keys replace saved ones; a key the file lacks is kept.
        let mut merged = found;
        merged.fill_gaps(&saved);
        let mut text = String::new();
        if let Some(k) = &merged.groq {
            text.push_str(&format!("{}={k}\n", talkee_dictate::keys::GROQ));
        }
        if let Some(k) = &merged.openrouter {
            text.push_str(&format!("{}={k}\n", talkee_dictate::keys::OPENROUTER));
        }
        store.save_keys(&text)?;
        println!(
            "imported keys from {} into {}",
            file.display(),
            store.keys_path().display()
        );
        saved = merged;
    }
    let state = |k: &Option<String>| if k.is_some() { "saved" } else { "not saved" };
    println!("Groq:       {}", state(&saved.groq));
    println!("OpenRouter: {}", state(&saved.openrouter));
    Ok(())
}
