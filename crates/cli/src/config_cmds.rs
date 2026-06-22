//! Config file management: init, reset, backup, restore, and template generation.

use crate::args::ConfigAction;
use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::fs;
use std::path::PathBuf;

/// Generate default configuration content.
pub(crate) fn generate_default_config() -> String {
    leopardwm_ipc::config_template::render_default_config()
}

/// Get the default config file path.
pub(crate) fn default_config_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "leopardwm").map(|dirs| dirs.config_dir().join("config.toml"))
}

/// Handle the init command (generate default config).
fn handle_init(output: Option<PathBuf>, force: bool, profile: Option<String>) -> Result<()> {
    let path = output
        .or_else(default_config_path)
        .context("Could not determine config path. Use --output to specify a path.")?;

    if path.exists() && !force {
        anyhow::bail!(
            "Config file already exists at: {}\nUse --force to overwrite.",
            path.display()
        );
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    let config_content = match profile.as_deref() {
        Some("laptop") => generate_profile_config("laptop"),
        Some("ultrawide") => generate_profile_config("ultrawide"),
        Some("developer") => generate_profile_config("developer"),
        Some(other) => anyhow::bail!(
            "Unknown profile '{}'. Available: developer, laptop, ultrawide",
            other
        ),
        None => generate_default_config(),
    };
    fs::write(&path, &config_content)
        .with_context(|| format!("Failed to write config file: {}", path.display()))?;

    if let Some(name) = &profile {
        println!("Created config file ({} profile): {}", name, path.display());
    } else {
        println!("Created config file: {}", path.display());
    }
    println!("\nNote: The daemon creates a default config on first run.");
    println!("Use this command to regenerate or apply a profile preset.");
    println!("Run 'leopardwm-cli reload' to apply changes while daemon is running.");

    Ok(())
}

/// Generate config content for a named profile.
pub(crate) fn generate_profile_config(profile: &str) -> String {
    use leopardwm_ipc::config_template::{render_config, TemplateOverrides};

    let (gap, outer_gap, centering, name) = match profile {
        "laptop" => (6, 6, "center", Some("laptop")),
        "ultrawide" => (12, 16, "just_in_view", Some("ultrawide")),
        "developer" => (10, 10, "center", Some("developer")),
        _ => (10, 10, "center", None),
    };

    render_config(&TemplateOverrides {
        gap: Some(gap),
        outer_gap: Some(outer_gap),
        centering_mode: Some(centering),
        profile_name: name,
    })
}

/// Get the backup path for a config file.
pub(crate) fn config_backup_path(config_path: &std::path::Path) -> PathBuf {
    config_path.with_extension("toml.bak")
}

/// Handle config subcommands (init, reset, backup, restore).
pub(crate) fn handle_config(action: ConfigAction) -> Result<()> {
    let config_path = default_config_path().context("Could not determine config path.")?;
    let backup_path = config_backup_path(&config_path);

    match action {
        ConfigAction::Init {
            output,
            force,
            profile,
        } => {
            return handle_init(output, force, profile);
        }
        ConfigAction::Reset => {
            if config_path.exists() {
                fs::copy(&config_path, &backup_path)
                    .with_context(|| format!("Failed to backup to {}", backup_path.display()))?;
                println!("Backed up current config to: {}", backup_path.display());
            }
            let config_content = generate_default_config();
            if let Some(parent) = config_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&config_path, config_content)
                .with_context(|| format!("Failed to write: {}", config_path.display()))?;
            println!("Config reset to defaults: {}", config_path.display());
            println!("Run 'leopardwm-cli reload' to apply if daemon is running.");
        }
        ConfigAction::Backup => {
            if !config_path.exists() {
                anyhow::bail!("No config file found at: {}", config_path.display());
            }
            fs::copy(&config_path, &backup_path)
                .with_context(|| format!("Failed to backup to {}", backup_path.display()))?;
            println!("Config backed up to: {}", backup_path.display());
        }
        ConfigAction::Restore => {
            if !backup_path.exists() {
                anyhow::bail!("No backup found at: {}", backup_path.display());
            }
            fs::copy(&backup_path, &config_path)
                .with_context(|| format!("Failed to restore from {}", backup_path.display()))?;
            println!("Config restored from: {}", backup_path.display());
            println!("Run 'leopardwm-cli reload' to apply if daemon is running.");
        }
    }
    Ok(())
}
