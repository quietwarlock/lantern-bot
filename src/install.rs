//! Start with Windows (the per-user Run key) and the permanent install folder
//! (`%LOCALAPPDATA%\Lantern`). Lantern keeps working from wherever it is,
//! including Downloads; installing only copies it.

use std::path::{Path, PathBuf};

pub const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
pub const RUN_VALUE: &str = "Lantern";
pub const EXE: &str = "Lantern.exe";

/// Files that move with Lantern. The settings file holds the token; it is
/// copied as a file, never read here.
pub const CARRY: &[&str] = &[
    "lantern-settings.json",
    "lantern-data.json",
    crate::library::INDEX_FILE,
    "yt-dlp.exe",
    "deno.exe",
    "lantern-ytdlp-check.txt",
];

/// `%LOCALAPPDATA%\Lantern`, or `None` off Windows.
pub fn install_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("Lantern"))
}

pub fn running_from(dir: &Path) -> bool {
    let here = crate::settings::app_dir();
    match (here.canonicalize(), dir.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The Run value: the exe in quotes, so spaces in the path are fine.
pub fn run_command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

#[cfg(windows)]
fn reg(args: &[&str]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("reg")
        .args(args)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .map_err(|e| format!("couldn't run reg.exe: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(not(windows))]
fn reg(_args: &[&str]) -> Result<String, String> {
    Err("Start with Windows only works on Windows.".into())
}

/// The exe Windows starts at sign-in, if any.
pub fn start_with_windows() -> Option<String> {
    let out = reg(&["query", RUN_KEY, "/v", RUN_VALUE]).ok()?;
    parse_query(&out)
}

/// The data part of `reg query` output for the Lantern value.
pub fn parse_query(out: &str) -> Option<String> {
    out.lines().find_map(|line| {
        let line = line.trim();
        let rest = line.strip_prefix(RUN_VALUE)?.trim_start();
        let rest = rest.strip_prefix("REG_SZ")?;
        Some(rest.trim().to_string()).filter(|s| !s.is_empty())
    })
}

/// Turns Start with Windows on for `exe`, or off.
pub fn set_start_with_windows(on: bool, exe: &Path) -> Result<(), String> {
    if on {
        let data = run_command(exe);
        reg(&["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &data, "/f"]).map(|_| ())
    } else if start_with_windows().is_some() {
        reg(&["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]).map(|_| ())
    } else {
        Ok(())
    }
}

/// Copies the running exe and its files into `dest`. A file already there is
/// replaced only when the one here is newer, so installing twice is safe.
/// Returns the new exe's path.
pub fn install_into(from: &Path, exe: &Path, dest: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("couldn't create {}: {e}", dest.display()))?;
    let target = dest.join(EXE);
    copy_file(exe, &target, true)?;
    for name in CARRY {
        let source = from.join(name);
        if source.is_file() {
            let newer = is_newer(&source, &dest.join(name));
            if newer {
                copy_file(&source, &dest.join(name), false)?;
            }
        }
    }
    Ok(target)
}

fn is_newer(a: &Path, b: &Path) -> bool {
    let time = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (time(a), time(b)) {
        (_, None) => true,
        (Some(a), Some(b)) => a > b,
        (None, Some(_)) => false,
    }
}

fn copy_file(source: &Path, dest: &Path, exe: bool) -> Result<(), String> {
    let mut part = dest.as_os_str().to_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    std::fs::copy(source, &part).map_err(|e| format!("couldn't copy {}: {e}", source.display()))?;
    let renamed = std::fs::rename(&part, dest);
    if let Err(err) = renamed {
        let _ = std::fs::remove_file(&part);
        let hint = if exe { " Close the installed Lantern first." } else { "" };
        return Err(format!("couldn't replace {}: {err}.{hint}", dest.display()));
    }
    Ok(())
}

/// Adds a Start menu shortcut to the installed exe. Failure is not fatal.
#[cfg(windows)]
pub fn start_menu_shortcut(exe: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let Some(appdata) = std::env::var_os("APPDATA") else { return Err("no APPDATA".into()) };
    let link = PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Lantern.lnk");
    let quote = |p: &Path| p.display().to_string().replace('\'', "''");
    let script = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}');$s.TargetPath='{}';$s.WorkingDirectory='{}';$s.Save()",
        quote(&link),
        quote(exe),
        quote(exe.parent().unwrap_or(exe)),
    );
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x0800_0000)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("PowerShell couldn't make the shortcut".into())
    }
}

#[cfg(not(windows))]
pub fn start_menu_shortcut(_exe: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reg_query_output_parses() {
        let out = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\r\n    Lantern    REG_SZ    \"C:\\Users\\a b\\Lantern.exe\"\r\n\r\n";
        assert_eq!(parse_query(out).as_deref(), Some("\"C:\\Users\\a b\\Lantern.exe\""));
        assert_eq!(parse_query("    LanternOther    REG_SZ    x"), None);
        assert_eq!(parse_query(""), None);
    }

    #[test]
    fn run_value_is_quoted() {
        assert_eq!(run_command(Path::new("C:/a b/Lantern.exe")), "\"C:/a b/Lantern.exe\"");
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lantern-inst-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn install_copies_the_exe_and_data_and_leaves_the_original() {
        let from = temp("from");
        let dest = temp("dest").join("Lantern");
        let exe = from.join("Lantern.exe");
        std::fs::write(&exe, b"MZ program").unwrap();
        std::fs::write(from.join("lantern-settings.json"), b"{}").unwrap();
        std::fs::write(from.join("lantern-data.json"), b"{\"a\":1}").unwrap();
        std::fs::write(from.join("unrelated.txt"), b"x").unwrap();
        let target = install_into(&from, &exe, &dest).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"MZ program");
        assert_eq!(std::fs::read(dest.join("lantern-data.json")).unwrap(), b"{\"a\":1}");
        assert!(dest.join("lantern-settings.json").is_file());
        assert!(!dest.join("unrelated.txt").exists());
        assert!(exe.is_file() && from.join("lantern-settings.json").is_file(), "the original keeps working");
        // A newer file at the destination is not overwritten by an older one.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(dest.join("lantern-data.json"), b"newer").unwrap();
        install_into(&from, &exe, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("lantern-data.json")).unwrap(), b"newer");
    }
}
