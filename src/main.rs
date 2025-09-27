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

    if let Some(idx) = args.iter().position(|a| a == "--ensure-tasks") {
        // Optional next arg: explicit exe path to use in tasks
        let explicit_exe_arg = args.get(idx + 1).and_then(|s| if s.starts_with("--") { None } else { Some(s.clone()) });

        // If not admin, elevate and forward the explicit path (or derive it first)
        if !is_running_as_admin() {
            let exe_to_use = match explicit_exe_arg.clone() {
                Some(p) => PathBuf::from(p),
                None => match ensure_installed_exe() { Ok(p) => p, Err(e) => { eprintln!("Failed to prepare install location: {}", e); std::process::exit(1);} }
            };
            match try_elevate_to(&exe_to_use, &["--ensure-tasks", &exe_to_use.to_string_lossy()]) {
                Ok(()) => std::process::exit(0),
                Err(e) => { eprintln!("Elevation request failed: {}", e); std::process::exit(1); }
            }
        }

        let target_exe = if let Some(p) = explicit_exe_arg { PathBuf::from(p) } else {
            // Fallback: derive install path in this context
            match ensure_installed_exe() { Ok(p) => p, Err(e) => { eprintln!("Failed to prepare install location: {}", e); std::process::exit(1);} }
        };

        match ensure_scheduled_tasks_for_exe(&target_exe) {
            Ok(_) => { println!("Scheduled tasks ensured."); std::process::exit(0); }
            Err(e) => { eprintln!("Failed to install scheduled tasks: {}", e); std::process::exit(1); }
        }
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
        match try_elevate_to(&installed_exe, &["--ensure-tasks", &installed_exe.to_string_lossy()]) {
            Ok(()) => {
                println!("Requested Administrator approval. If accepted, tasks will be installed by the elevated instance.");
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
                println!("Installed/updated scheduled tasks (boot/logon/lock/unlock).");
            } else {
                println!("Scheduled tasks already present. Nothing to do.");
            }
            println!("Executable location: {}", quote_path(&installed_exe));
            println!(
                "This tool will: pre-logon or on lock set brightness to 100, and on logon/unlock restore and exit."
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
    // Create or update tasks for: boot, logon, lock, unlock
    let mut created_any = false;

    // Clean up legacy tasks created by older versions
    let _ = delete_task_if_exists("WindowsHelloNightHelper_Startup");
    let _ = delete_task_if_exists("WindowsHelloNightHelper_Restore");

    // Resolve current user SID for per-user triggers (logon/lock/unlock)
    let user_sid = get_current_user_sid().map_err(|e| format!("get user sid: {}", e))?;

    match ensure_task_xml(
        "WindowsHelloNightHelper_OnBoot",
        &render_task_xml_system(exe, "--startup", &boot_trigger_xml(), "Set brightness to 100 at system startup"),
    ) {
        Ok(c) => { created_any |= c; }
        Err(e) => {
            eprintln!("XML create for OnBoot failed: {}. Falling back to CLI...", e);
            create_task_onstart_cli_system("WindowsHelloNightHelper_OnBoot", exe, "--startup")
                .map_err(|e| format!("fallback create OnBoot failed: {}", e))?;
            created_any = true;
        }
    }

    match ensure_task_xml(
        "WindowsHelloNightHelper_OnLogon",
        &render_task_xml_user(exe, "--restore", &logon_trigger_xml(), "Restore brightness at user logon", &user_sid),
    ) {
        Ok(c) => { created_any |= c; }
        Err(e) => {
            eprintln!("XML create for OnLogon failed: {}. Falling back to CLI...", e);
            create_task_onlogon_cli_system("WindowsHelloNightHelper_OnLogon", exe, "--restore")
                .map_err(|e| format!("fallback create OnLogon failed: {}", e))?;
            created_any = true;
        }
    }

    match ensure_task_xml(
        "WindowsHelloNightHelper_OnLock",
        &render_task_xml_user(exe, "--startup", &session_lock_trigger_xml(), "Set brightness to 100 on workstation lock", &user_sid),
    ) {
        Ok(c) => { created_any |= c; }
        Err(e) => {
            eprintln!("XML create for OnLock failed: {}. Skipping lock trigger.", e);
        }
    }

    match ensure_task_xml(
        "WindowsHelloNightHelper_OnUnlock",
        &render_task_xml_user(exe, "--restore", &session_unlock_trigger_xml(), "Restore brightness on workstation unlock", &user_sid),
    ) {
        Ok(c) => { created_any |= c; }
        Err(e) => {
            eprintln!("XML create for OnUnlock failed: {}. Skipping unlock trigger.", e);
        }
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

fn delete_task_if_exists(name: &str) -> bool {
    if !task_exists(name) { return false; }
    match Command::new("schtasks")
        .arg("/Delete")
        .arg("/TN").arg(name)
        .arg("/F")
        .status() {
        Ok(s) => s.success(),
        Err(_) => false,
    }
}

fn ensure_task_xml(name: &str, xml: &str) -> Result<bool, String> {
    let tmp = env::temp_dir().join(format!("whnh_{}_{}.xml", name, std::process::id()));
    write_utf16le_file(&tmp, xml).map_err(|e| format!("write xml {:?}: {}", tmp, e))?;

    let existed = task_exists(name);

    let mut cmd = Command::new("schtasks");
    cmd.arg("/Create")
        .arg("/F")
        .arg("/TN").arg(name)
        .arg("/XML").arg(&tmp);

    let output = cmd.output().map_err(|e| format!("spawn schtasks: {}", e))?;

    let _ = fs::remove_file(&tmp);

    if output.status.success() {
        Ok(!existed)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("schtasks /XML failed: {}", stderr.trim()))
    }
}

fn create_task_onstart_cli_system(task_name: &str, exe: &Path, args: &str) -> Result<(), String> {
    let tr = format!("{} {}", quote_path(exe), args);
    let output = Command::new("schtasks")
        .arg("/Create").arg("/F")
        .arg("/TN").arg(task_name)
        .arg("/SC").arg("ONSTART")
        .arg("/RU").arg("SYSTEM")
        .arg("/RL").arg("HIGHEST")
        .arg("/TR").arg(tr)
        .output()
        .map_err(|e| format!("spawn schtasks: {}", e))?;
    if output.status.success() { Ok(()) } else { Err(format!("schtasks ONSTART failed: {}", String::from_utf8_lossy(&output.stderr).trim())) }
}

fn create_task_onlogon_cli_system(task_name: &str, exe: &Path, args: &str) -> Result<(), String> {
    let tr = format!("{} {}", quote_path(exe), args);
    let output = Command::new("schtasks")
        .arg("/Create").arg("/F")
        .arg("/TN").arg(task_name)
        .arg("/SC").arg("ONLOGON")
        .arg("/RU").arg("SYSTEM")
        .arg("/RL").arg("HIGHEST")
        .arg("/TR").arg(tr)
        .output()
        .map_err(|e| format!("spawn schtasks: {}", e))?;
    if output.status.success() { Ok(()) } else { Err(format!("schtasks ONLOGON failed: {}", String::from_utf8_lossy(&output.stderr).trim())) }
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

fn write_utf16le_file(path: &Path, content: &str) -> std::io::Result<()> {
    let mut f = File::create(path)?;
    // BOM for UTF-16LE
    f.write_all(&[0xFF, 0xFE])?;
    let mut buf = Vec::with_capacity(content.len() * 2);
    for u in content.encode_utf16() { buf.extend_from_slice(&u.to_le_bytes()); }
    f.write_all(&buf)
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
        format!("Start-Process -FilePath '{}' -Verb RunAs -WindowStyle Hidden -Wait", file.replace("'", "''"))
    } else {
        format!("Start-Process -FilePath '{}' -ArgumentList {} -Verb RunAs -WindowStyle Hidden -Wait", file.replace("'", "''"), arg_list)
    };

    let status = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-WindowStyle").arg("Hidden")
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

fn base_task_xml_prefix_system(description: &str) -> String {
    format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-16\"?>",
            "<Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">",
            "<RegistrationInfo><Description>{}</Description></RegistrationInfo>",
            "<Principals>",
            "  <Principal id=\"Author\">",
            "    <UserId>S-1-5-18</UserId>",
            "    <RunLevel>HighestAvailable</RunLevel>",
            "  </Principal>",
            "</Principals>",
            "<Settings>",
            "  <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "  <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "  <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "  <AllowHardTerminate>true</AllowHardTerminate>",
            "  <StartWhenAvailable>true</StartWhenAvailable>",
            "  <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>",
            "  <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>",
            "  <AllowStartOnDemand>true</AllowStartOnDemand>",
            "  <Enabled>true</Enabled>",
            "  <Hidden>true</Hidden>",
            "  <RunOnlyIfIdle>false</RunOnlyIfIdle>",
            "  <DisallowStartOnRemoteAppSession>false</DisallowStartOnRemoteAppSession>",
            "  <UseUnifiedSchedulingEngine>true</UseUnifiedSchedulingEngine>",
            "  <WakeToRun>true</WakeToRun>",
            "  <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "  <Priority>7</Priority>",
            "</Settings>"
        ),
        description
    )
}

fn base_task_xml_prefix_user(description: &str, user_sid: &str) -> String {
    format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-16\"?>",
            "<Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">",
            "<RegistrationInfo><Description>{}</Description></RegistrationInfo>",
            "<Principals>",
            "  <Principal id=\"Author\">",
            "    <UserId>{}</UserId>",
            "    <LogonType>InteractiveToken</LogonType>",
            "    <RunLevel>HighestAvailable</RunLevel>",
            "  </Principal>",
            "</Principals>",
            "<Settings>",
            "  <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "  <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "  <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "  <AllowHardTerminate>true</AllowHardTerminate>",
            "  <StartWhenAvailable>true</StartWhenAvailable>",
            "  <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>",
            "  <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>",
            "  <AllowStartOnDemand>true</AllowStartOnDemand>",
            "  <Enabled>true</Enabled>",
            "  <Hidden>true</Hidden>",
            "  <RunOnlyIfIdle>false</RunOnlyIfIdle>",
            "  <DisallowStartOnRemoteAppSession>false</DisallowStartOnRemoteAppSession>",
            "  <UseUnifiedSchedulingEngine>true</UseUnifiedSchedulingEngine>",
            "  <WakeToRun>true</WakeToRun>",
            "  <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "  <Priority>7</Priority>",
            "</Settings>"
        ),
        description, user_sid
    )
}

fn render_task_xml_system(exe: &Path, args: &str, trigger_xml: &str, description: &str) -> String {
    render_task_xml_with_prefix(base_task_xml_prefix_system(description), exe, args, trigger_xml)
}

fn render_task_xml_user(exe: &Path, args: &str, trigger_xml: &str, description: &str, user_sid: &str) -> String {
    render_task_xml_with_prefix(base_task_xml_prefix_user(description, user_sid), exe, args, trigger_xml)
}

fn render_task_xml_with_prefix(prefix: String, exe: &Path, args: &str, trigger_xml: &str) -> String {
    let mut xml = String::new();
    xml.push_str(&prefix);
    xml.push_str("<Triggers>");
    xml.push_str(trigger_xml);
    xml.push_str("</Triggers>");
    xml.push_str("<Actions Context=\"Author\"><Exec>");
    xml.push_str(&format!("<Command>{}</Command>", exe.to_string_lossy()));
    if !args.is_empty() { xml.push_str(&format!("<Arguments>{}</Arguments>", args)); }
    xml.push_str("</Exec></Actions></Task>");
    xml
}

fn boot_trigger_xml() -> String { "<BootTrigger><Enabled>true</Enabled></BootTrigger>".to_string() }
fn logon_trigger_xml() -> String { "<LogonTrigger><Enabled>true</Enabled></LogonTrigger>".to_string() }
fn session_lock_trigger_xml() -> String { "<SessionStateChangeTrigger><Enabled>true</Enabled><State>SessionLock</State></SessionStateChangeTrigger>".to_string() }
fn session_unlock_trigger_xml() -> String { "<SessionStateChangeTrigger><Enabled>true</Enabled><State>SessionUnlock</State></SessionStateChangeTrigger>".to_string() }

fn get_current_user_sid() -> Result<String, String> {
    let ps = "[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value";
    let output = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(ps)
        .output()
        .map_err(|e| format!("spawn powershell: {}", e))?;
    if !output.status.success() {
        return Err(format!("powershell get sid failed (code {:?})", output.status.code()));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim().to_string())
}

