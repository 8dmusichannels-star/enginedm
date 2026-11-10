//! configmanager: prints the EngineDM config and applies the autologin / systemd-service choice.
//! Port of the old `configmanager` crate on top of `edm-core`.

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::UNIX_EPOCH;

use edm_core::config::{load_or_create_config, DEFAULT_CONFIG_PATH};

const SERVICE_PATH: &str = "/etc/systemd/system/edm.service";
const LAST_CHECK_FILE: &str = "/var/lib/enginedm/config_last_check";

fn run(reload: bool) {
    let config_path = Path::new(DEFAULT_CONFIG_PATH);

    if reload {
        let modified = match fs::metadata(config_path).and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => {
                eprintln!("Configuration file does not exist.");
                return;
            }
        };
        let modified_secs = modified.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let last_check = fs::read_to_string(LAST_CHECK_FILE)
            .unwrap_or_default()
            .trim()
            .parse::<u64>()
            .unwrap_or(0);
        if modified_secs <= last_check {
            eprintln!("Configuration not changed, nothing to reload.");
            return;
        }
        // Only restart on a config that parses and validates.
        if let Err(e) = load_or_create_config(config_path) {
            eprintln!("Config file invalid ({e}). systemctl restart skipped.");
            return;
        }
        if let Err(e) = fs::write(LAST_CHECK_FILE, modified_secs.to_string()) {
            eprintln!("Failed to update last check timestamp: {e}");
        }
        println!("Configuration changed and valid, reloading...");
        match Command::new("sudo").args(["systemctl", "restart", "enginedm"]).status() {
            Ok(s) if s.success() => println!("enginedm restarted successfully."),
            Ok(s) => eprintln!("systemctl restart enginedm failed: {s}"),
            Err(e) => eprintln!("Failed to restart enginedm: {e}"),
        }
    }

    let config = match load_or_create_config(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load or create config: {e}");
            return;
        }
    };

    println!("Configuration loaded:");
    println!("Theme: {}", config.theme);
    println!("Tiling: {}", config.tiling);
    println!("Default session: {}", config.default_session);
    println!("Autologin: {}", config.autologin);
    println!("Two-factor enabled: {}", config.two_factor_enabled);
    if let Some(method) = &config.two_factor_method {
        println!("2FA method: {method}");
    }
    // The secret used to be printed in clear text (into terminals and logs).
    if config.two_factor_secret.is_some() {
        println!("2FA secret: <set>");
    }
    println!("Systemctl usedefine: {}", config.systemctl_usedefine);

    match config.find_interface() {
        Some(dir) => println!("Selected interface: {}", dir.display()),
        None => eprintln!(
            "Could not find an interface named '{}' (main.html); the built-in one will be used",
            config.interface_name()
        ),
    }

    // autologin && systemctl_usedefine is rejected by config validation, so only 3 cases remain.
    match (config.autologin, config.systemctl_usedefine) {
        (false, true) => {
            println!("Systemctl service will be created and enabled.");
            create_service(SERVICE_PATH);
            enable_service("edm.service");
        }
        (true, _) => {
            println!("Autologin enabled, systemctl service disabled.");
            remove_service_if_exists(SERVICE_PATH);
        }
        (false, false) => {
            println!("Neither autologin nor systemctl service enabled.");
            remove_service_if_exists(SERVICE_PATH);
        }
    }
}

fn create_service(path: &str) {
    let content = "[Unit]\nDescription=EngineDM Display Manager\nAfter=graphical.target\n\n\
                   [Service]\nType=simple\nExecStart=/usr/bin/enginedm\nRestart=always\n\n\
                   [Install]\nWantedBy=multi-user.target\n";
    match fs::write(path, content) {
        Ok(()) => println!("Service file written at {path}"),
        Err(e) => eprintln!("Failed to write systemd service: {e}"),
    }
}

fn enable_service(name: &str) {
    match Command::new("systemctl").args(["enable", name]).status() {
        Ok(s) if s.success() => println!("{name} enabled successfully"),
        Ok(_) => eprintln!("Failed to enable {name}"),
        Err(e) => eprintln!("Error running systemctl enable {name}: {e}"),
    }
}

fn remove_service_if_exists(path: &str) {
    if Path::new(path).exists() {
        match fs::remove_file(path) {
            Ok(()) => println!("Service file {path} removed"),
            Err(e) => eprintln!("Failed to remove service file {path}: {e}"),
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--help") {
        println!(
            "configmanager - EngineDM configuration manager\n\n\
             Usage:\n    configmanager [OPTIONS]\n\n\
             Options:\n    --reload   Reload configuration if it has changed and restart EngineDM.\n    --help     Show this help message."
        );
        return;
    }
    run(args.iter().any(|a| a == "--reload"));
}
