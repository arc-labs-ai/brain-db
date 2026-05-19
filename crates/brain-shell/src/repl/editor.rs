//! Persistent-history line editor wrapper.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use brain_core::MemoryId;
use rustyline::config::Config;
use rustyline::history::FileHistory;
use rustyline::{Editor, Result as RlResult};

use super::completion::ShellHelper;

/// rustyline editor parameterised over the shell's helper type.
pub type ShellEditor = Editor<ShellHelper, FileHistory>;

/// Build the editor and load history. The history file lives at
/// `$XDG_DATA_HOME/brain/history` (or `~/.brain_history` on
/// platforms without an XDG data dir).
///
/// `recent_ids` is the shared id ring; the helper reads it for
/// tab-completion of `MemoryId` arguments. The caller (REPL loop)
/// owns the same `Arc` and pushes ids onto it as commands run.
pub fn build(recent_ids: Arc<Mutex<VecDeque<MemoryId>>>) -> RlResult<(ShellEditor, PathBuf)> {
    let config = Config::builder()
        .auto_add_history(true)
        .max_history_size(10_000)?
        .build();
    let mut editor: ShellEditor = Editor::with_config(config)?;
    editor.set_helper(Some(ShellHelper::with_recent(recent_ids)));

    let path = history_path();
    if path.exists() {
        // Best effort — corrupt history shouldn't stop the shell.
        let _ = editor.load_history(&path);
    }
    Ok((editor, path))
}

/// Persist whatever's in the in-memory ring to disk.
pub fn save(editor: &mut ShellEditor, path: &PathBuf) {
    // Create the parent dir if needed.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = editor.save_history(path);
}

fn history_path() -> PathBuf {
    if let Some(data) = dirs::data_dir() {
        return data.join("brain").join("history");
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".brain_history");
    }
    PathBuf::from(".brain_history")
}
