//! Generates the third-party notices shown in the About dialog: every crate
//! linked into the app (normal dependencies for the target, not dev or build
//! tools), its license expression, and the license/notice files it ships.
//! Crates with identical license texts share one copy.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest_dir.join("../..");
    println!("cargo:rerun-if-changed={}", workspace.join("Cargo.lock").display());
    println!("cargo:rerun-if-changed=build.rs");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("notices.txt");
    let notices = generate(&workspace).unwrap_or_else(|error| {
        println!("cargo:warning=could not generate third-party notices: {error}");
        format!("Third-party notices could not be generated for this build ({error}).\n")
    });
    std::fs::write(out, notices).unwrap();
}

fn generate(workspace: &Path) -> Result<String, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let target = std::env::var("TARGET").map_err(|e| e.to_string())?;
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked", "--offline", "--filter-platform", &target])
        .arg("--manifest-path")
        .arg(workspace.join("Cargo.toml"))
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let metadata: Value = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;

    let packages: HashMap<&str, &Value> = metadata["packages"]
        .as_array()
        .ok_or("no packages")?
        .iter()
        .filter_map(|p| Some((p["id"].as_str()?, p)))
        .collect();
    let members: BTreeSet<&str> =
        metadata["workspace_members"].as_array().ok_or("no members")?.iter().filter_map(Value::as_str).collect();
    let nodes: HashMap<&str, &Value> = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or("no resolve graph")?
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n)))
        .collect();
    let root = packages
        .values()
        .find(|p| p["name"] == "signal-gpui" && members.contains(p["id"].as_str().unwrap_or("")))
        .and_then(|p| p["id"].as_str())
        .ok_or("signal-gpui not found")?;

    // Everything reachable through normal (shipped) dependency edges.
    let mut linked = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !linked.insert(id) {
            continue;
        }
        for dep in nodes.get(id).and_then(|n| n["deps"].as_array()).into_iter().flatten() {
            let normal = dep["dep_kinds"].as_array().into_iter().flatten().any(|k| k["kind"].is_null());
            if normal && let Some(next) = dep["pkg"].as_str() {
                stack.push(next);
            }
        }
    }

    let mut listing = Vec::new();
    // license text -> crates using it
    let mut texts: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for id in linked.iter().filter(|id| !members.contains(*id)) {
        let package = packages[id];
        let name = format!(
            "{} {}",
            package["name"].as_str().unwrap_or("?"),
            package["version"].as_str().unwrap_or("?")
        );
        let license = package["license"].as_str().unwrap_or("see license file");
        listing.push(format!("{name} — {license}"));
        let dir = Path::new(package["manifest_path"].as_str().unwrap_or("")).parent().map(Path::to_path_buf);
        for text in dir.map(|d| license_files(&d, package)).unwrap_or_default() {
            texts.entry(text).or_default().insert(name.clone());
        }
    }
    listing.sort_by_key(|line| line.to_lowercase());

    let mut out = String::new();
    out.push_str(&format!("This build links {} third-party crates:\n\n", listing.len()));
    for line in &listing {
        out.push_str(line);
        out.push('\n');
    }
    for (text, users) in &texts {
        out.push_str("\n\n");
        out.push_str(&"=".repeat(72));
        out.push_str("\nUsed by: ");
        out.push_str(&users.iter().cloned().collect::<Vec<_>>().join(", "));
        out.push('\n');
        out.push_str(&"=".repeat(72));
        out.push_str("\n\n");
        out.push_str(text.trim());
        out.push('\n');
    }
    Ok(out)
}

/// The texts of a crate's LICENSE*/COPYING*/NOTICE* files (and its declared
/// `license-file`).
fn license_files(dir: &Path, package: &Value) -> Vec<String> {
    let mut paths = BTreeSet::new();
    if let Some(file) = package["license_file"].as_str() {
        paths.insert(dir.join(file));
    }
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_uppercase();
        let is_notice = ["LICENSE", "LICENCE", "COPYING", "NOTICE"].iter().any(|p| name.starts_with(p));
        if is_notice && entry.path().is_file() {
            paths.insert(entry.path());
        }
    }
    paths.into_iter().filter_map(|p| std::fs::read_to_string(p).ok()).filter(|t| !t.trim().is_empty()).collect()
}
