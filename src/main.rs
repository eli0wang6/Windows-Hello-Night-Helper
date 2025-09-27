use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return;
    }

    if args.iter().any(|a| a == "--startup") {
        handle_startup();
        return;
    }

    if args.iter().any(|a| a == "--restore") {
        handle_restore();
        return;
    }

    // No args: ensure installed location; then ensure admin for scheduled tasks
    let installed_exe = match ensure_installed_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to prepare install location: {}", e);
            std::process::exit(1);
        }
    };

    if !is_running_as_admin() {
        match try_elevate_to(&installed_exe, &[]) {
            Ok(()) => {
                println!("Requesting Administrator approval... If accepted, tasks will be installed by the elevated instance.");
                return;
            }
            Err(e) => {
                eprintln!("Elevation request failed: {}", e);
                std::process::exit(1);
            }
        }
    }

    match ensure_scheduled_tasks_for_exe(&installed_exe) {
        Ok(created) => {
            if created {
                println!("Installed/updated scheduled tasks (boot and logon).");
            } else {
                println!("Scheduled tasks already present. Nothing to do.");
            }
            println!("Executable location: {}", quote_path(&installed_exe));
            println!(
                "This tool will: pre-logon set brightness to 100, post-logon restore and exit."
            );
        }
        Err(e) => {
            eprintln!("Failed to install scheduled tasks: {}", e);
            std::process::exit(1);
        }
    }
}

fn print_usage() {
    println!(
        "Windows Hello Night Helper\n\n\
Usage:\n  - Run once as Administrator with no arguments to install tasks.\n  - At boot (pre-logon) it records current brightness and sets 100%.\n  - At user logon it restores brightness and exits.\n\n\
Manual modes:\n  --startup  Run the pre-logon behavior now\n  --restore  Run the post-logon restore now\n  --help     Show this message\n"
    );
}

fn handle_startup() {
    // Record current brightness (if not recorded yet), then set to 100
    let state_file = brightness_state_file();
    if !state_file.exists() {
        if let Ok(b) = get_current_brightness() {
            if let Err(e) = write_brightness_state(b) {
                eprintln!("Failed to write brightness state: {}", e);
            }
        }
    }

    if let Err(e) = set_brightness(100) {
        eprintln!("Failed to set brightness to 100: {}", e);
    }
}

fn handle_restore() {
    // Restore brightness from state, then remove state file
    match read_brightness_state() {
        Ok(Some(b)) => {
            if let Err(e) = set_brightness(b) {
                eprintln!("Failed to restore brightness: {}", e);
            }
            // Best effort cleanup
            let _ = fs::remove_file(brightness_state_file());
        }
        Ok(None) => {
            // Nothing to restore; exit quietly
        }
        Err(e) => {
            eprintln!("Failed to read brightness state: {}", e);
        }
    }
}

fn ensure_scheduled_tasks_for_exe(exe: &Path) -> Result<bool, String> {
    let exe_quoted = quote_path(exe);
    let startup_task = "WindowsHelloNightHelper_Startup";
    let restore_task = "WindowsHelloNightHelper_Restore";

    let startup_tr = format!("{} --startup", exe_quoted);
    let restore_tr = format!("{} --restore", exe_quoted);

    let mut created_any = false;

    if !task_exists(startup_task) {
        create_task_onstart(startup_task, &startup_tr)
            .map_err(|e| format!("create startup task failed: {}", e))?;
        created_any = true;
    }

    if !task_exists(restore_task) {
        create_task_onlogon(restore_task, &restore_tr)
            .map_err(|e| format!("create restore task failed: {}", e))?;
        created_any = true;
    }

    Ok(created_any)
}

fn task_exists(name: &str) -> bool {
    let status = Command::new("schtasks")
        .arg("/Query")
        .arg("/TN")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match status { Ok(s) => s.success(), Err(_) => false }
}

fn create_task_onstart(name: &str, tr: &str) -> Result<(), String> {
    create_task_common(name, tr, &[("/SC", "ONSTART")])
}

fn create_task_onlogon(name: &str, tr: &str) -> Result<(), String> {
    create_task_common(name, tr, &[("/SC", "ONLOGON")])
}

fn create_task_common(name: &str, tr: &str, extra: &[(&str, &str)]) -> Result<(), String> {
    let mut cmd = Command::new("schtasks");
    cmd.arg("/Create")
        .arg("/RU").arg("SYSTEM")
        .arg("/RL").arg("HIGHEST")
        .arg("/TN").arg(name)
        .arg("/TR").arg(tr)
        .arg("/F");

    for (k, v) in extra { cmd.arg(k).arg(v); }

    let output = cmd.output().map_err(|e| format!("spawn schtasks: {}", e))?;
    if output.status.success() { Ok(()) } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("schtasks failed: {}", stderr.trim()))
    }
}

fn current_exe_path() -> std::io::Result<PathBuf> {
    // Use canonicalized path to avoid issues with quotes/spaces
    let mut p = env::current_exe()?;
    if let Ok(c) = p.canonicalize() { p = c; }
    Ok(p)
}

fn quote_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    format!("\"{}\"", s)
}

fn desired_install_dir() -> Result<PathBuf, String> {
    // Prefer LOCALAPPDATA; fallback to USERPROFILE based path
    if let Ok(local) = env::var("LOCALAPPDATA") {
        Ok(PathBuf::from(local).join("Programs").join("WindowsHelloNightHelper"))
    } else if let Ok(profile) = env::var("USERPROFILE") {
        Ok(PathBuf::from(profile).join("AppData").join("Local").join("Programs").join("WindowsHelloNightHelper"))
    } else {
        Err("Cannot determine LocalAppData path".to_string())
    }
}

fn ensure_installed_exe() -> Result<PathBuf, String> {
    let current = current_exe_path().map_err(|e| format!("cannot get exe path: {}", e))?;
    let install_dir = desired_install_dir()?;

    // If already running from desired directory, return current path
    if current.starts_with(&install_dir) {
        return Ok(current);
    }

    // Ensure directory exists
    fs::create_dir_all(&install_dir).map_err(|e| format!("create install dir {:?}: {}", install_dir, e))?;

    // Determine target file name (reuse current executable file name if possible)
    let file_name = current
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "WindowsHelloNightHelper.exe".into());
    let target = install_dir.join(file_name);

    // Copy and overwrite if exists
    fs::copy(&current, &target)
        .map_err(|e| format!("copy to {:?} failed: {}", target, e))?;

    Ok(target)
}

fn is_running_as_admin() -> bool {
    let ps = "$p = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent()); if ($p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { exit 0 } else { exit 1 }";
    let status = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(ps)
        .status();
    match status { Ok(s) => s.success(), Err(_) => false }
}

fn try_elevate_to(exe: &Path, args: &[&str]) -> Result<(), String> {
    let file = exe.to_string_lossy();
    let mut arg_list = String::new();
    if !args.is_empty() {
        arg_list = args
            .iter()
            .map(|a| format!("'{}'", a.replace("'", "''")))
            .collect::<Vec<_>>()
            .join(", ");
    }
    let ps = if arg_list.is_empty() {
        format!("Start-Process -FilePath '{}' -Verb RunAs", file.replace("'", "''"))
    } else {
        format!("Start-Process -FilePath '{}' -ArgumentList {} -Verb RunAs", file.replace("'", "''"), arg_list)
    };

    let status = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(ps)
        .status()
        .map_err(|e| format!("spawn powershell: {}", e))?;
    if status.success() { Ok(()) } else { Err(format!("UAC prompt rejected or failed (code {:?})", status.code())) }
}

fn program_data_dir() -> PathBuf {
    if let Ok(v) = env::var("PROGRAMDATA") {
        PathBuf::from(v).join("WindowsHelloNightHelper")
    } else {
        PathBuf::from(r"C:\\ProgramData").join("WindowsHelloNightHelper")
    }
}

fn brightness_state_file() -> PathBuf { program_data_dir().join("brightness.txt") }

fn write_brightness_state(value: u8) -> Result<(), String> {
    let dir = program_data_dir();
    if !dir.exists() {
        fs::create_dir_all(&dir).map_err(|e| format!("mkdir {:?}: {}", dir, e))?;
    }
    let mut f = File::create(brightness_state_file()).map_err(|e| format!("create state: {}", e))?;
    f.write_all(value.to_string().as_bytes()).map_err(|e| format!("write state: {}", e))
}

fn read_brightness_state() -> Result<Option<u8>, String> {
    let p = brightness_state_file();
    if !p.exists() { return Ok(None); }
    let mut s = String::new();
    File::open(&p).map_err(|e| format!("open state: {}", e))?.read_to_string(&mut s).map_err(|e| format!("read state: {}", e))?;
    let trimmed = s.trim();
    if trimmed.is_empty() { return Ok(None); }
    match trimmed.parse::<u8>() { Ok(v) => Ok(Some(v)), Err(e) => Err(format!("parse state: {}", e)) }
}

fn get_current_brightness() -> Result<u8, String> {
    let ps = "[int](Get-WmiObject -Namespace root/WMI -Class WmiMonitorBrightness | Select-Object -First 1 -ExpandProperty CurrentBrightness)";
    let output = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(ps)
        .output()
        .map_err(|e| format!("spawn powershell: {}", e))?;

    if !output.status.success() {
        return Err(format!("powershell get brightness failed (code {:?})", output.status.code()));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    let v: i32 = line.parse().map_err(|e| format!("parse output '{}': {}", line, e))?;
    if v < 0 || v > 100 { return Err("brightness out of range".to_string()); }
    Ok(v as u8)
}

fn set_brightness(percent: u8) -> Result<(), String> {
    let v = percent.clamp(0, 100);
    let ps = format!(
        "$m=Get-WmiObject -Namespace root/WMI -Class WmiMonitorBrightnessMethods; if ($m) {{ $m | ForEach-Object {{ $_.WmiSetBrightness(1,{}) | Out-Null }} }}",
        v
    );
    let status = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(ps)
        .status()
        .map_err(|e| format!("spawn powershell: {}", e))?;
    if status.success() { Ok(()) } else { Err(format!("powershell set brightness failed (code {:?})", status.code())) }
}

