//! More windows: each database opens in a window of its own, which is another
//! `sdb-gui` process. The database goes to it through the environment, so a
//! password typed in the Connections dialog stays off the command line.

use simple_database::connections::SafeMode;

const TARGET: &str = "SDB_GUI_TARGET";
const LABEL: &str = "SDB_GUI_LABEL";
const SAFE_MODE: &str = "SDB_GUI_SAFE_MODE";
/// Set on every window opened from another one.
const SPAWNED: &str = "SDB_GUI_SPAWNED";

/// What a new window connects to.
pub struct Launch {
    pub target: String,
    /// Label and safe mode chosen in the Connections dialog; without them
    /// `Database::open` works them out from the target.
    pub labeled: Option<(String, SafeMode)>,
    /// The tab to start on (`--tab`).
    pub tab: Option<&'static str>,
}

/// Start another window: with `launch`'s database, or with the welcome dialog.
pub fn open(launch: Option<Launch>) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.env(SPAWNED, "1").env_remove(TARGET).env_remove(LABEL).env_remove(SAFE_MODE);
    if let Some(l) = launch {
        cmd.env(TARGET, &l.target);
        if let Some((label, mode)) = l.labeled {
            cmd.env(LABEL, label).env(SAFE_MODE, serde_json::to_string(&mode).unwrap_or_default());
        }
        if let Some(tab) = l.tab {
            cmd.args(["--tab", tab]);
        }
    }
    let mut child = cmd.spawn()?;
    // Collect its exit status, so it doesn't linger as a zombie on Unix.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Whether another window opened this one.
pub fn spawned() -> bool {
    std::env::var_os(SPAWNED).is_some()
}

/// The database another window asked this one to open.
pub fn launch() -> Option<Launch> {
    let target = std::env::var(TARGET).ok().filter(|t| !t.is_empty())?;
    let labeled = std::env::var(LABEL).ok().map(|label| {
        let mode = std::env::var(SAFE_MODE).ok().and_then(|m| serde_json::from_str(&m).ok()).unwrap_or_default();
        (label, mode)
    });
    Some(Launch { target, labeled, tab: None })
}
