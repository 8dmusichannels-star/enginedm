//! enginedmgreeterd: EngineDM greeter. Login logic is Rust; the interface is HTML/CSS rendered by Blitz.
//!
//!   enginedmgreeterd [--config FILE] [--interface DIR] [--check] [--run-handoff] [--restart-session]
//!
//! `--restart-session` asks the running session host (`--run-handoff`) to restart its session
//! inside the same PAM session (standalone mode; not available under greetd).
//!
//! `--check` loads config + interface + sessions, prints what it found and exits without opening
//! a window (exit code 1 if the selected interface is unusable).

mod backend;
mod controller;
mod interface;
#[cfg(feature = "ui")]
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

use edm_core::config::{load_config, EDMConfig, DEFAULT_CONFIG_PATH};

struct Args {
    config: PathBuf,
    interface: Option<PathBuf>,
    check: bool,
    run_handoff: bool,
    restart_session: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { config: PathBuf::from(DEFAULT_CONFIG_PATH), interface: None, check: false, run_handoff: false, restart_session: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => a.config = it.next().ok_or("--config needs a value")?.into(),
            "--interface" => a.interface = Some(it.next().ok_or("--interface needs a value")?.into()),
            "--check" => a.check = true,
            "--run-handoff" => a.run_handoff = true,
            "--restart-session" => a.restart_session = true,
            "-h" | "--help" => {
                println!("usage: enginedmgreeterd [--config FILE] [--interface DIR] [--check] [--run-handoff] [--restart-session]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    Ok(a)
}

/// The selected interface's HTML, or the built-in one. A broken custom UI must never leave the
/// machine without a login screen, so every failure falls back (and says why on stderr).
fn pick_html(cfg: &EDMConfig, dir_override: Option<PathBuf>) -> (String, String, Option<PathBuf>) {
    let dir = dir_override.or_else(|| cfg.find_interface());
    let (html, source) = pick_base(cfg, dir.clone());
    let Some(path) = resolve_configured_wallpaper(cfg, dir.as_deref()) else { return (html, source, None) };
    match interface::wallpaper_data_uri(&path) {
        Ok(uri) => match interface::embed_wallpaper(&html, &uri) {
            // Best case: the picture is part of the page, so it is there from the first frame.
            Some(page) => {
                eprintln!("edm: wallpaper {} embedded in the page ({} bytes)", path.display(), uri.len());
                (page, source, None)
            }
            // No #bg-pic in this interface: the UI adds the picture after the first event instead.
            None => (html, source, Some(path)),
        },
        Err(e) => {
            eprintln!("edm: wallpaper {} ignored: {e}", path.display());
            (html, source, None)
        }
    }
}

/// The configured `wallpaper=` as an existing image file. It is NOT injected as a `file://` CSS url
/// (that never showed up at startup); the UI embeds it as a `data:` URI exactly like a picture chosen
/// in the inspector. A wallpaper that cannot be used is reported and ignored; it must never stop the
/// login screen from appearing.
fn resolve_configured_wallpaper(cfg: &EDMConfig, dir: Option<&std::path::Path>) -> Option<PathBuf> {
    let value = cfg.wallpaper.as_deref()?;
    let found = interface::resolve_wallpaper(value, dir);
    if found.is_none() {
        eprintln!("edm: wallpaper '{value}' ignored (not an existing png/jpg/webp/gif file)");
    }
    found
}

fn pick_base(cfg: &EDMConfig, dir: Option<PathBuf>) -> (String, String) {
    if let Some(dir) = dir {
        match interface::load_dir(&dir) {
            Ok(html) => {
                #[cfg(feature = "ui")]
                if let Err(e) = ui::check_interface(&html) {
                    eprintln!("edm: {}: {e}; using the built-in interface", dir.display());
                    return (interface::default_html(), "built-in (fallback)".into());
                }
                return (html, dir.display().to_string());
            }
            Err(e) => eprintln!("edm: cannot load {}: {e}; using the built-in interface", dir.display()),
        }
    } else {
        eprintln!("edm: interface '{}' not found; using the built-in interface", cfg.interface_name());
    }
    (interface::default_html(), "built-in".into())
}

/// Standalone handover (started by `enginedm-run` after the greeter's compositor has exited):
/// opens the PAM session for the user named in the handoff file, starts the chosen session on the
/// freed VT and stays alive until it ends, because this process owns the PAM session.
fn run_handoff(cfg: &EDMConfig) -> ExitCode {
    use edm_core::auth::PamAuth;
    use edm_core::desktop::{available_sessions, prepare_sessions};
    use edm_core::launch::{SessionRunner, UserInfo};

    let fail = |m: String| {
        eprintln!("edm: handoff: {m}");
        ExitCode::FAILURE
    };
    let (user, name) = match edm_core::handoff::take(&edm_core::handoff::handoff_path()) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let sessions = prepare_sessions(available_sessions(), cfg.dbus_wrap);
    let Some(entry) = sessions.iter().find(|s| s.name.eq_ignore_ascii_case(&name)).cloned() else {
        return fail(format!("session '{name}' not found"));
    };
    let info = match UserInfo::lookup(&user) {
        Ok(i) => i,
        Err(e) => return fail(format!("user lookup failed for '{user}': {e}")),
    };
    let login = match PamAuth::new(cfg.pam_service.as_deref().unwrap_or("")).open_for(&user) {
        Ok(l) => l,
        Err(e) => return fail(e),
    };
    // Handler first (SIGUSR1 would otherwise terminate us), pid file once the session is up.
    edm_core::restart::install_handler();
    let mut runner = SessionRunner::default();
    if let Err(e) = runner.start(&entry, &info, Some(login)) {
        return fail(format!("could not start '{}': {e}", entry.name));
    }
    let _pid = match edm_core::restart::PidGuard::create(&edm_core::restart::pid_path()) {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("edm: handoff: no pid file, restart unavailable: {e}");
            None
        }
    };
    eprintln!("edm: session '{}' started for {user}", entry.name);
    runner.wait_with_restart(|r| match r {
        Ok(()) => eprintln!("edm: session '{}' of {user} restarted", entry.name),
        Err(e) => eprintln!("edm: session '{}' of {user} could not be restarted: {e}", entry.name),
    });
    eprintln!("edm: session '{}' of {user} ended", entry.name);
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("edm: {e}");
            return ExitCode::from(2);
        }
    };
    let cfg = load_config(&args.config).unwrap_or_else(|e| {
        eprintln!("edm: {e}; continuing with defaults");
        EDMConfig::default()
    });
    if args.restart_session {
        return match edm_core::restart::request_restart(&edm_core::restart::pid_path()) {
            Ok(()) => {
                println!("restart requested");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("edm: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if args.run_handoff {
        return run_handoff(&cfg);
    }
    let cfg_pam = cfg.pam_service.clone().unwrap_or_else(|| "system-login (default)".into());
    let (html, source, wallpaper) = pick_html(&cfg, args.interface);
    let user_dirs = cfg.user_dirs.clone();
    let user_settings = cfg.user_settings.clone();
    let config_path = args.config.clone();
    let backend = backend::RealBackend::new(cfg, args.config);
    let default_session = backend.default_session_name().to_string();
    let mut ctl = controller::Controller::new(backend);
    ctl.select_session_named(&default_session);

    if args.check {
        use controller::Backend;
        println!("interface : {source}");
        println!("sessions  : {}", ctl.backend.sessions().iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "));
        println!("themes    : {}", ctl.backend.themes().len());
        println!("pam       : {}", cfg_pam);
        println!("2fa       : {}", if ctl.backend.two_factor_required() { "required" } else { "off" });
        let dirs = if user_dirs.is_empty() { interface::default_user_dirs() } else { user_dirs.clone() };
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
        println!("pictures  : {}", interface::index_summary(&interface::index_pictures(&dirs, home.as_deref())));
        if let Some(p) = ctl.backend.config_problem() {
            println!("problem   : {p}");
        }
        return if source.contains("fallback") { ExitCode::from(1) } else { ExitCode::SUCCESS };
    }

    #[cfg(feature = "ui")]
    {
        if let Err(e) = ui::run(&html, ctl, user_dirs, config_path, wallpaper, user_settings) {
            eprintln!("edm: ui failed: {e}");
            return ExitCode::FAILURE;
        }
        ExitCode::SUCCESS
    }
    #[cfg(not(feature = "ui"))]
    {
        let _ = (html, ctl, user_dirs, config_path, wallpaper, user_settings);
        eprintln!("edm: built without the `ui` feature; use --check");
        ExitCode::FAILURE
    }
}
