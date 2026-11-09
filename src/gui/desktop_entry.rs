//! The app's icon and, on Linux, its entry in the applications menu. Wayland takes a window's
//! icon only from the installed `.desktop` file whose name matches the window's app id.

#[cfg(any(target_os = "linux", test))]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::{env, fs, io, path::PathBuf};

/// The window's app id, and the name of the `.desktop` file and icon installed for it.
pub const APP_ID: &str = "sdb-gui";

/// The Magellan violet (Viola magellanica), drawn as `assets/icon/sdb.svg`.
pub const APP_ICON_PNG: &[u8] = include_bytes!("../../assets/icon/sdb.png");

/// Writes the icon and `sdb-gui.desktop`, pointing at this build, into the user's data folder;
/// replaces what an earlier install wrote. Returns the `.desktop` file.
#[cfg(target_os = "linux")]
pub fn install() -> io::Result<PathBuf> {
    let program = env::current_exe()?.canonicalize()?;
    let data = data_folder()?;
    let icons = data.join("icons").join("hicolor").join("512x512").join("apps");
    fs::create_dir_all(&icons)?;
    fs::write(icons.join(format!("{APP_ID}.png")), APP_ICON_PNG)?;
    let applications = data.join("applications");
    fs::create_dir_all(&applications)?;
    let entry = applications.join(format!("{APP_ID}.desktop"));
    fs::write(&entry, desktop_file(&program))?;
    Ok(entry)
}

/// `$XDG_DATA_HOME`, or `~/.local/share`.
#[cfg(target_os = "linux")]
fn data_folder() -> io::Result<PathBuf> {
    if let Some(data) = env::var_os("XDG_DATA_HOME").filter(|data| !data.is_empty()) {
        return Ok(PathBuf::from(data));
    }
    env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local").join("share"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

#[cfg(any(target_os = "linux", test))]
fn desktop_file(program: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=sdb\n\
         Comment=Schema-first database designer\n\
         Exec={} %F\n\
         Icon={APP_ID}\n\
         StartupWMClass={APP_ID}\n\
         Terminal=false\n\
         Categories=Development;Database;\n",
        quoted_exec(program)
    )
}

/// The program path as the Desktop Entry spec wants it in `Exec`: double-quoted, with `"`,
/// `` ` ``, `$` and `\` escaped by a backslash and `%` doubled; then every backslash doubled
/// again, as in any string value of the file.
#[cfg(any(target_os = "linux", test))]
fn quoted_exec(program: &Path) -> String {
    let mut quoted = String::from('"');
    for c in program.to_string_lossy().chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_a_png() {
        assert!(eframe::icon_data::from_png_bytes(APP_ICON_PNG).is_ok());
    }

    #[test]
    fn exec_quotes_spaces_and_escapes_specials() {
        assert_eq!(quoted_exec(Path::new("/opt/My Apps/sdb$1%")), "\"/opt/My Apps/sdb\\\\$1%%\"");
    }

    #[test]
    fn entry_matches_the_window_app_id() {
        let file = desktop_file(Path::new("/usr/bin/sdb-gui"));
        assert!(file.contains("Exec=\"/usr/bin/sdb-gui\" %F\n"));
        assert!(file.contains("Icon=sdb-gui\n"));
        assert!(file.contains("StartupWMClass=sdb-gui\n"));
    }
}
