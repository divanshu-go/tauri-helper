use rayon::prelude::*;
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use syn::parse_file;
use tauri_helper_core::{find_workspace_dir, get_workspace, get_workspace_members};

pub use tauri_helper_core::types::TauriHelperOptions;
pub use tauri_helper_macros::*;

#[derive(Debug, Clone)]
struct SourceFile {
    path: PathBuf,
    module_path: String,
}

#[derive(Debug, Clone)]
struct CollectedCommand {
    path: String,
    specta: bool,
}

fn module_path_for_source(source_dir: &Path, source_path: &Path) -> Option<String> {
    let relative = source_path.strip_prefix(source_dir).ok()?;
    if relative == Path::new("main.rs") || relative == Path::new("lib.rs") {
        return Some(String::new());
    }

    let mut parts: Vec<String> = relative
        .iter()
        .map(|part| part.to_string_lossy().to_string())
        .collect();
    let last = parts.last_mut()?;
    if !last.ends_with(".rs") {
        return None;
    }
    last.truncate(last.len() - ".rs".len());

    if last == "mod" {
        parts.pop();
    }

    Some(parts.join("::"))
}

fn collect_rs_files(source_dir: &Path, dir: &Path, files: &mut Vec<SourceFile>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(source_dir, &path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            if let Some(module_path) = module_path_for_source(source_dir, &path) {
                files.push(SourceFile { path, module_path });
            }
        }
    }
}

fn collect_path_module_files(files: &mut Vec<SourceFile>) {
    for source_file in files.clone() {
        let Ok(source) = fs::read_to_string(&source_file.path) else {
            continue;
        };

        for line in source.lines() {
            let trimmed = line.trim();
            let Some(path_attr) = trimmed
                .strip_prefix("#[path = \"")
                .and_then(|rest| rest.strip_suffix("\"]"))
            else {
                continue;
            };

            let Some(parent) = source_file.path.parent() else {
                continue;
            };
            let path = parent.join(path_attr);
            if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                files.push(SourceFile {
                    path,
                    module_path: source_file.module_path.clone(),
                });
            }
        }
    }
}

fn is_tauri_command_attr(path: &syn::Path) -> bool {
    path.is_ident("command")
        || (path.segments.len() == 2
            && path.segments[0].ident == "tauri"
            && path.segments[1].ident == "command")
}

fn is_specta_attr(path: &syn::Path) -> bool {
    path.is_ident("specta")
        || (path.segments.len() == 2
            && path.segments[0].ident == "specta"
            && path.segments[1].ident == "specta")
}

fn collect_commands_from_source(
    source_file: &SourceFile,
    collect_all: bool,
) -> Vec<CollectedCommand> {
    let Ok(content) = fs::read_to_string(&source_file.path) else {
        return Vec::new();
    };
    if !content.contains("tauri::command") && !content.contains("auto_collect_command") {
        return Vec::new();
    }

    let Ok(ast) = parse_file(&content) else {
        return Vec::new();
    };

    let mut commands = Vec::new();

    for item in ast.items {
        let syn::Item::Fn(func) = item else {
            continue;
        };

        let has_tauri = func
            .attrs
            .iter()
            .any(|attr| is_tauri_command_attr(attr.path()));
        if !has_tauri {
            continue;
        }

        if !collect_all
            && !func
                .attrs
                .iter()
                .any(|attr| attr.path().is_ident("auto_collect_command"))
        {
            continue;
        }

        let specta = func
            .attrs
            .iter()
            .any(|attr| is_specta_attr(attr.path()));

        let fn_name = func.sig.ident.to_string();
        let path = if source_file.module_path.is_empty() {
            fn_name
        } else {
            format!("{}::{}", source_file.module_path, fn_name)
        };

        commands.push(CollectedCommand { path, specta });
    }

    commands
}

fn full_command_path(crate_name: &str, package_name: &str, command_path: &str) -> String {
    let crate_name = crate_name.replace('-', "_");
    let prefix = if crate_name == "src_tauri" {
        package_name.replace('-', "_")
    } else {
        crate_name
    };
    format!("{prefix}::{command_path}")
}

fn write_command_list(path: &Path, commands: &[String]) {
    let mut file = File::create(path).unwrap();
    for command in commands {
        writeln!(file, "{command}").unwrap();
    }
}

#[allow(clippy::needless_doctest_main)]
/// Scans the crate for functions annotated with `#[tauri::command]` and optionally `#[auto_collect_command]`,
/// then generates command list files in the `tauri_commands_list` folder.
///
/// This function is intended to be used in a `build.rs` script to automate the process of
/// collecting Tauri commands during the build process. It should be called before invoking
/// `tauri_build::build()` to ensure the command list is available for the Tauri application.
///
/// # Usage
///
/// Add the following to your `build.rs` file:
///
/// ```rust,ignore
/// fn main() {
///     // Generate the command file for Tauri
///     tauri_helper::generate_command_file(tauri_helper::TauriHelperOptions::default());
///
///     // Build the Tauri application
///     tauri_build::build();
/// }
/// ```
///
/// # Annotations
///
/// By default, this function looks for functions annotated with both `#[tauri::command]` and
/// `#[auto_collect_command]`. For example:
///
/// ```rust,ignore
/// #[tauri::command]
/// #[auto_collect_command]
/// fn my_command() {
///     println!("Some Command")
/// }
/// ```
///
/// These functions will be automatically collected and written to the `tauri_commands_list` folder.
///
/// If the `collect_all` option is set to `true`, the function will collect all `#[tauri::command]`
/// functions, regardless of whether they have the `#[auto_collect_command]` attribute. However,
/// this behavior is not recommended unless explicitly needed.
///
/// Commands annotated with `#[specta::specta]` are written to a separate `{crate}_specta.txt`
/// file and are used by [`specta_collect_commands!`].
///
/// # Output
///
/// For each workspace member, two files may be written under `target/tauri_commands_list/`:
///
/// - `{crate}.txt` — all collected commands (used by [`tauri_collect_commands!`])
/// - `{crate}_specta.txt` — commands that also have `#[specta::specta]` (used by [`specta_collect_commands!`])
///
/// Command paths include nested module paths derived from the source tree (for example
/// `my_crate::commands::greet`). Files referenced via `#[path = "..."]` are included.
///
/// Single-crate apps (empty `[workspace].members`) are supported automatically — no need to
/// pass `members: Some(vec![".".into()])`.
///
/// # Options
///
/// The behavior of this function can be customized using the `TauriHelperOptions` struct:
///
/// - **`collect_all`**: When `true`, collects all `#[tauri::command]` functions, even if they lack
///   the `#[auto_collect_command]` attribute. When `false` (default), only functions with both
///   `#[tauri::command]` and `#[auto_collect_command]` are collected.
///
///   **Recommendation**: Keep this option set to `false` to ensure explicit control over which
///   commands are included in your Tauri application.
///
/// # Notes
///
/// - This function should only be called once per build, typically in the `build.rs` script.
/// - More options are coming such as a list of explicit files that need to be scanned only, if you have any more ideas, please open an issue on Github.
///
/// # Example
///
/// ```rust,ignore
/// #[tauri::command]
/// #[auto_collect_command]
/// fn greet(name: String) -> String {
///     format!("Hello, {}!", name)
/// }
///
/// #[tauri::command]
/// fn calculate_sum(a: i32, b: i32) -> i32 {
///     a + b
/// }
/// ```
///
/// With `collect_all` set to `false` (default), only `greet` will be collected. With `collect_all`
/// set to `true`, both `greet` and `calculate_sum` will be collected.
///
/// # Panics
///
/// This function will panic if:
/// - The `tauri_commands_list` folder cannot be created or written to.
/// - No functions matching the criteria are found.
///
/// # Errors
///
/// If the function encounters an error during file generation, it will log the error and exit the
/// build process with a non-zero status code.
pub fn generate_command_file(options: TauriHelperOptions) {
    let workspace_root = find_workspace_dir(Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()));
    let commands_dir = workspace_root.join("target").join("tauri_commands_list");
    fs::create_dir_all(&commands_dir).unwrap();

    let workspace_members = options.members.clone().unwrap_or_else(|| {
        let members = get_workspace_members(&workspace_root);
        if members.is_empty() {
            vec![".".to_string()]
        } else {
            members
        }
    });

    let package_name = get_workspace().package.name.replace('-', "_");
    let collect_all = options.collect_all;

    for member in &workspace_members {
        println!("cargo:rerun-if-changed={}", member);
    }

    workspace_members.par_iter().for_each(|member| {
        let manifest_dir = workspace_root.join(member);
        let crate_name = manifest_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();

        let src_dir = manifest_dir.join("src");
        let mut source_files = Vec::new();
        collect_rs_files(&src_dir, &src_dir, &mut source_files);
        collect_path_module_files(&mut source_files);
        source_files.sort_by(|left, right| left.path.cmp(&right.path));

        let mut commands = Vec::new();
        for source_file in &source_files {
            println!("cargo:rerun-if-changed={}", source_file.path.display());
            commands.extend(collect_commands_from_source(source_file, collect_all));
        }

        if commands.is_empty() {
            return;
        }

        let mut tauri_commands: Vec<String> = commands
            .iter()
            .map(|command| full_command_path(&crate_name, &package_name, &command.path))
            .collect();
        tauri_commands.sort();
        tauri_commands.dedup();

        let mut specta_commands: Vec<String> = commands
            .iter()
            .filter(|command| command.specta)
            .map(|command| full_command_path(&crate_name, &package_name, &command.path))
            .collect();
        specta_commands.sort();
        specta_commands.dedup();

        write_command_list(
            &commands_dir.join(format!("{crate_name}.txt")),
            &tauri_commands,
        );

        if !specta_commands.is_empty() {
            write_command_list(
                &commands_dir.join(format!("{crate_name}_specta.txt")),
                &specta_commands,
            );
        }
    });
}
