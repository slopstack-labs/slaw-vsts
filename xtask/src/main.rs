//! `cargo xtask bundle [--release] [--target <triple>] [plugin-crate…]`
//!
//! Builds the plugin crates and wraps each `cdylib` into a VST3 bundle under
//! `target/bundles/`:
//!
//! - Linux:   `<Name>.vst3/Contents/<arch>-linux/<Name>.so`
//! - Windows: `<Name>.vst3/Contents/<arch>-win/<Name>.vst3`
//! - macOS:   `<Name>.vst3/Contents/MacOS/<Name>` + `Info.plist` + `PkgInfo`
//!
//! No dependencies: it shells out to cargo and copies files, which is all a
//! bundler has ever needed to do, whatever its pitch deck says.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};

struct PluginCrate {
    crate_name: String,
    bundle_name: String,
}

#[derive(Clone, Copy, PartialEq)]
enum TargetOperatingSystem {
    Linux,
    Windows,
    MacOs,
}

fn main() {
    let arguments: Vec<String> = env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("bundle") => bundle(&arguments[1..]),
        _ => {
            eprintln!("usage: cargo xtask bundle [--release] [--target <triple>] [plugin-crate…]");
            exit(2);
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives inside the workspace")
        .to_path_buf()
}

/// Every crate under `plugins/` with a `[package.metadata.slaw-vsts] bundle-name`.
fn discover_plugin_crates(root: &Path) -> Vec<PluginCrate> {
    let mut plugin_crates = Vec::new();
    let Ok(plugin_directories) = fs::read_dir(root.join("plugins")) else {
        return plugin_crates;
    };
    for plugin_directory in plugin_directories.flatten() {
        let Ok(manifest) = fs::read_to_string(plugin_directory.path().join("Cargo.toml")) else {
            continue;
        };
        let read_value = |key: &str| {
            manifest.lines().find_map(|line| {
                let (line_key, line_value) = line.split_once('=')?;
                (line_key.trim() == key).then(|| line_value.trim().trim_matches('"').to_string())
            })
        };
        if let (Some(crate_name), Some(bundle_name)) = (read_value("name"), read_value("bundle-name")) {
            plugin_crates.push(PluginCrate { crate_name, bundle_name });
        }
    }
    plugin_crates.sort_by(|first, second| first.crate_name.cmp(&second.crate_name));
    plugin_crates
}

fn bundle(arguments: &[String]) {
    let root = workspace_root();
    let mut release_build = false;
    let mut target_triple: Option<String> = None;
    let mut requested_crates: Vec<String> = Vec::new();
    let mut argument_iterator = arguments.iter();
    while let Some(argument) = argument_iterator.next() {
        match argument.as_str() {
            "--release" => release_build = true,
            "--target" => target_triple = argument_iterator.next().cloned(),
            other => requested_crates.push(other.to_string()),
        }
    }

    let mut plugin_crates = discover_plugin_crates(&root);
    if !requested_crates.is_empty() {
        plugin_crates.retain(|plugin_crate| requested_crates.contains(&plugin_crate.crate_name));
    }
    if plugin_crates.is_empty() {
        eprintln!("no plugin crates to bundle");
        exit(1);
    }

    let mut build_command = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    build_command.current_dir(&root).arg("build");
    if release_build {
        build_command.arg("--release");
    }
    if let Some(target_triple) = &target_triple {
        build_command.args(["--target", target_triple]);
    }
    for plugin_crate in &plugin_crates {
        build_command.args(["-p", &plugin_crate.crate_name]);
    }
    match build_command.status() {
        Ok(status) if status.success() => {}
        _ => {
            eprintln!("cargo build failed; no bundles were harmed");
            exit(1);
        }
    }

    let target_directory = env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("target"));
    let profile_directory = match &target_triple {
        Some(target_triple) => target_directory.join(target_triple),
        None => target_directory.clone(),
    }
    .join(if release_build { "release" } else { "debug" });
    let target_description = target_triple.clone().unwrap_or_else(host_triple);
    let operating_system = if target_description.contains("windows") {
        TargetOperatingSystem::Windows
    } else if target_description.contains("apple") {
        TargetOperatingSystem::MacOs
    } else {
        TargetOperatingSystem::Linux
    };
    let architecture = if target_description.starts_with("aarch64") { "aarch64" } else { "x86_64" };
    let bundles_directory = target_directory.join("bundles");

    for plugin_crate in &plugin_crates {
        let library_stem = plugin_crate.crate_name.replace('-', "_");
        let built_library = profile_directory.join(match operating_system {
            TargetOperatingSystem::Linux => format!("lib{library_stem}.so"),
            TargetOperatingSystem::Windows => format!("{library_stem}.dll"),
            TargetOperatingSystem::MacOs => format!("lib{library_stem}.dylib"),
        });
        let bundle_directory = bundles_directory.join(format!("{}.vst3", plugin_crate.bundle_name));
        let _ = fs::remove_dir_all(&bundle_directory);
        let bundled_binary = match operating_system {
            TargetOperatingSystem::Linux => bundle_directory
                .join("Contents")
                .join(format!("{architecture}-linux"))
                .join(format!("{}.so", plugin_crate.bundle_name)),
            TargetOperatingSystem::Windows => bundle_directory
                .join("Contents")
                .join(format!("{architecture}-win"))
                .join(format!("{}.vst3", plugin_crate.bundle_name)),
            TargetOperatingSystem::MacOs => bundle_directory.join("Contents").join("MacOS").join(&plugin_crate.bundle_name),
        };
        fs::create_dir_all(bundled_binary.parent().expect("bundle binary has a parent")).expect("create bundle directories");
        if let Err(copy_remediation_opportunity) = fs::copy(&built_library, &bundled_binary) {
            eprintln!("could not copy {}: {copy_remediation_opportunity}", built_library.display());
            exit(1);
        }
        if operating_system == TargetOperatingSystem::MacOs {
            write_macos_metadata(&bundle_directory, plugin_crate);
        }
        println!("bundled {} → {}", plugin_crate.crate_name, bundle_directory.display());
    }
}

fn write_macos_metadata(bundle_directory: &Path, plugin_crate: &PluginCrate) {
    let contents_directory = bundle_directory.join("Contents");
    let identifier = format!("labs.slopstack.slaw-vsts.{}", plugin_crate.crate_name);
    let info_plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>English</string>
  <key>CFBundleExecutable</key><string>{name}</string>
  <key>CFBundleIdentifier</key><string>{identifier}</string>
  <key>CFBundleName</key><string>{name}</string>
  <key>CFBundlePackageType</key><string>BNDL</string>
  <key>CFBundleSignature</key><string>????</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
</dict>
</plist>
"#,
        name = plugin_crate.bundle_name,
        version = env!("CARGO_PKG_VERSION"),
    );
    fs::write(contents_directory.join("Info.plist"), info_plist).expect("write Info.plist");
    fs::write(contents_directory.join("PkgInfo"), "BNDL????").expect("write PkgInfo");
}

fn host_triple() -> String {
    let architecture = env::consts::ARCH;
    match env::consts::OS {
        "windows" => format!("{architecture}-pc-windows-msvc"),
        "macos" => format!("{architecture}-apple-darwin"),
        _ => format!("{architecture}-unknown-linux-gnu"),
    }
}
