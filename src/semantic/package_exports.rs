//! Resolves bare package specifiers (`@scope/ui/styles/x.css`) to workspace
//! files via each workspace package's `package.json` `name` and `exports`.

use crate::types::Project;
use rustc_hash::FxHashSet;
use serde_json::Value;
use std::fs;
use std::path::{Component, Path, PathBuf};

struct WorkspacePackage {
  name: String,
  root: PathBuf,
  exports: Option<Value>,
}

#[derive(Default)]
pub struct PackageIndex {
  packages: Vec<WorkspacePackage>,
}

impl PackageIndex {
  pub fn from_projects(cwd: &Path, projects: &[Project]) -> Self {
    let mut seen = FxHashSet::default();
    let mut packages: Vec<WorkspacePackage> = projects
      .iter()
      .filter(|p| seen.insert(p.root.clone()))
      .filter_map(|p| {
        let manifest: Value =
          serde_json::from_str(&fs::read_to_string(cwd.join(&p.root).join("package.json")).ok()?)
            .ok()?;
        Some(WorkspacePackage {
          name: manifest.get("name")?.as_str()?.to_string(),
          root: p.root.clone(),
          exports: manifest.get("exports").cloned(),
        })
      })
      .collect();
    packages.sort_by_key(|p| std::cmp::Reverse(p.name.len()));
    Self { packages }
  }

  /// Workspace-relative candidate files for a bare `specifier`. Conditional
  /// exports yield every target since the consuming bundler's conditions are unknown.
  pub fn resolve(&self, specifier: &str) -> Vec<PathBuf> {
    let Some((pkg, subpath)) = self.packages.iter().find_map(|pkg| {
      let rest = specifier.strip_prefix(pkg.name.as_str())?;
      match rest {
        "" => Some((pkg, ".".to_string())),
        _ => rest.strip_prefix('/').map(|s| (pkg, format!("./{s}"))),
      }
    }) else {
      return Vec::new();
    };

    let targets = match &pkg.exports {
      None => vec![subpath],
      Some(exports) => export_targets(exports, &subpath),
    };
    targets
      .iter()
      .map(|t| normalize(&pkg.root.join(t)))
      .collect()
  }
}

fn export_targets(exports: &Value, subpath: &str) -> Vec<String> {
  let subpath_map = match exports {
    Value::Object(map) if map.keys().any(|k| k.starts_with('.')) => map,
    _ if subpath == "." => return collect_strings(exports, None),
    _ => return Vec::new(),
  };

  if let Some(target) = subpath_map.get(subpath) {
    return collect_strings(target, None);
  }
  subpath_map
    .iter()
    .filter_map(|(key, target)| {
      let (prefix, suffix) = key.split_once('*')?;
      let matched = subpath.strip_prefix(prefix)?.strip_suffix(suffix)?;
      Some((prefix.len(), target, matched))
    })
    .max_by_key(|(prefix_len, ..)| *prefix_len)
    .map(|(_, target, matched)| collect_strings(target, Some(matched)))
    .unwrap_or_default()
}

fn collect_strings(target: &Value, wildcard: Option<&str>) -> Vec<String> {
  match target {
    Value::String(s) => vec![match wildcard {
      Some(w) => s.replace('*', w),
      None => s.clone(),
    }],
    Value::Array(items) => items
      .iter()
      .flat_map(|v| collect_strings(v, wildcard))
      .collect(),
    Value::Object(map) => map
      .values()
      .flat_map(|v| collect_strings(v, wildcard))
      .collect(),
    _ => Vec::new(),
  }
}

pub(crate) fn normalize(path: &Path) -> PathBuf {
  let mut out = PathBuf::new();
  for component in path.components() {
    match component {
      Component::ParentDir => {
        out.pop();
      }
      Component::CurDir => {}
      other => out.push(other),
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  use tempfile::TempDir;

  fn index(manifests: &[(&str, Value)]) -> (TempDir, PackageIndex) {
    let tmp = TempDir::new().unwrap();
    let projects: Vec<Project> = manifests
      .iter()
      .map(|(root, manifest)| {
        fs::create_dir_all(tmp.path().join(root)).unwrap();
        fs::write(
          tmp.path().join(root).join("package.json"),
          manifest.to_string(),
        )
        .unwrap();
        Project {
          name: root.to_string(),
          root: PathBuf::from(root),
          source_root: PathBuf::from(root).join("src"),
          ts_config: None,
          implicit_dependencies: vec![],
          targets: vec![],
        }
      })
      .collect();
    let idx = PackageIndex::from_projects(tmp.path(), &projects);
    (tmp, idx)
  }

  #[test]
  fn exact_subpath_export() {
    let (_tmp, idx) = index(&[(
      "packages/ui",
      json!({"name": "@scope/ui", "exports": {"./styles/x.css": "./src/styles/x.css"}}),
    )]);
    assert_eq!(
      idx.resolve("@scope/ui/styles/x.css"),
      vec![PathBuf::from("packages/ui/src/styles/x.css")]
    );
    assert!(idx.resolve("@scope/ui/styles/y.css").is_empty());
  }

  #[test]
  fn wildcard_and_conditional_exports() {
    let (_tmp, idx) = index(&[(
      "packages/ui",
      json!({"name": "@scope/ui", "exports": {
        ".": {"import": "./src/index.ts", "default": "./dist/index.js"},
        "./*": "./src/*",
        "./styles/*.css": {"style": "./src/styles/*.css"}
      }}),
    )]);
    assert_eq!(
      idx.resolve("@scope/ui/styles/x.css"),
      vec![PathBuf::from("packages/ui/src/styles/x.css")]
    );
    assert_eq!(
      idx.resolve("@scope/ui/theme.css"),
      vec![PathBuf::from("packages/ui/src/theme.css")]
    );
    let mut root = idx.resolve("@scope/ui");
    root.sort();
    assert_eq!(
      root,
      vec![
        PathBuf::from("packages/ui/dist/index.js"),
        PathBuf::from("packages/ui/src/index.ts"),
      ]
    );
  }

  #[test]
  fn no_exports_falls_back_to_package_root() {
    let (_tmp, idx) = index(&[("packages/ui", json!({"name": "@scope/ui"}))]);
    assert_eq!(
      idx.resolve("@scope/ui/src/styles/x.css"),
      vec![PathBuf::from("packages/ui/src/styles/x.css")]
    );
  }

  #[test]
  fn longest_name_wins_and_prefix_must_end_at_segment() {
    let (_tmp, idx) = index(&[
      ("packages/ui", json!({"name": "@scope/ui"})),
      ("packages/ui-kit", json!({"name": "@scope/ui-kit"})),
    ]);
    assert_eq!(
      idx.resolve("@scope/ui-kit/a.css"),
      vec![PathBuf::from("packages/ui-kit/a.css")]
    );
    assert!(idx.resolve("@scope/uix/a.css").is_empty());
  }
}
