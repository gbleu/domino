use crate::types::Project;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use tracing::warn;

/// The effective `compilerOptions.paths` of one tsconfig after its `extends` chain,
/// with the directory its targets resolve against.
///
/// Loaded by domino itself rather than by oxc_resolver: oxc_resolver fails the whole
/// config when an `extends` names a package it cannot find in `node_modules`, and CI
/// runs often install only the root package. A bare `extends` resolves through the
/// workspace project list first, so `@scope/ts-config/tsconfig.front.json` reaches
/// the project's sources without an install.
#[derive(Debug)]
pub(crate) struct TsconfigPaths {
  base_dir: PathBuf,
  patterns: PathPatterns,
  /// `compilerOptions.baseUrl`, kept separately from `base_dir` because it is also a
  /// resolution root in its own right: TypeScript resolves a non-relative specifier no
  /// `paths` key matches against it. A project declaring only `baseUrl` has no patterns
  /// at all, and dropping it would leave every one of its imports outside the graph.
  base_url: Option<PathBuf>,
}

/// `compilerOptions.paths` entries: a key with at most one `*`, and its target patterns.
type PathPatterns = Vec<(String, Vec<String>)>;

impl TsconfigPaths {
  /// `None` when the chain declares neither `paths` nor `baseUrl`.
  pub(crate) fn load(tsconfig: &Path, cwd: &Path, projects: &[Project]) -> Option<Self> {
    // TypeScript 5.5+ expands `${configDir}` against the directory of the config the
    // compiler was pointed at, not the one declaring it, so a shared base config can say
    // `baseUrl: "${configDir}"` and mean each consumer's own directory.
    let config_dir = tsconfig.parent()?.to_path_buf();
    let effective = load_chain(tsconfig, cwd, projects, &mut Vec::new(), &config_dir)?;
    let (patterns, paths_dir) = match effective.paths {
      Some((patterns, paths_dir)) => (patterns, Some(paths_dir)),
      None => (Vec::new(), None),
    };
    let base_dir = effective.base_url.clone().or(paths_dir)?;
    Some(Self {
      base_dir,
      patterns,
      base_url: effective.base_url,
    })
  }

  /// Absolute paths `specifier` maps to, in the order TypeScript tries them. Only the
  /// best pattern counts: an exact key, else the wildcard key with the longest prefix.
  pub(crate) fn candidates(&self, specifier: &str) -> Vec<PathBuf> {
    let exact = self
      .patterns
      .iter()
      .find(|(key, _)| !key.contains('*') && key == specifier)
      .map(|(_, targets)| (targets, ""));

    let wildcard = || {
      self
        .patterns
        .iter()
        .filter_map(|(key, targets)| {
          let (prefix, suffix) = key.split_once('*')?;
          let captured = specifier.strip_prefix(prefix)?.strip_suffix(suffix)?;
          Some((prefix.len(), targets, captured))
        })
        // Longest prefix wins; on a tie TypeScript keeps the first declared pattern, so
        // `reduce` must hold onto `best` unless `cur` is strictly longer. `max_by_key`
        // returns the *last* maximum and would pick the wrong one.
        .reduce(|best, cur| if cur.0 > best.0 { cur } else { best })
        .map(|(_, targets, captured)| (targets, captured))
    };

    // TypeScript falls back to `baseUrl` both when no key matches and when the matched
    // key's targets do not exist, so this is appended to the pattern's targets rather than
    // replacing them: `paths: {"*": ["generated/*"]}` with `baseUrl: "src"` must still find
    // `src/features/router` once the generated target misses.
    let base_url_candidate = self
      .base_url
      .as_ref()
      .map(|base_url| normalize(&base_url.join(specifier)));

    exact
      .or_else(wildcard)
      .map(|(targets, captured)| {
        targets
          .iter()
          .map(|target| normalize(&self.base_dir.join(target.replacen('*', captured, 1))))
          .collect::<Vec<_>>()
      })
      .unwrap_or_default()
      .into_iter()
      .chain(base_url_candidate)
      .collect()
  }
}

#[derive(Deserialize)]
struct TsconfigJson {
  extends: Option<Extends>,
  #[serde(rename = "compilerOptions")]
  compiler_options: Option<CompilerOptions>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Extends {
  Single(String),
  Multiple(Vec<String>),
}

#[derive(Deserialize)]
struct CompilerOptions {
  #[serde(rename = "baseUrl")]
  base_url: Option<String>,
  paths: Option<OrderedPaths>,
}

/// `paths` keys in the order the config declares them. TypeScript resolves equal-length
/// prefix matches first-declared-wins, so a `HashMap` here makes the winner depend on the
/// process's hash seed: the same import can resolve to a different project between runs.
struct OrderedPaths(PathPatterns);

impl<'de> Deserialize<'de> for OrderedPaths {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    struct OrderedPathsVisitor;

    impl<'de> Visitor<'de> for OrderedPathsVisitor {
      type Value = PathPatterns;

      fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a map of tsconfig path patterns")
      }

      fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
      where
        A: MapAccess<'de>,
      {
        let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
        while let Some(entry) = map.next_entry::<String, Vec<String>>()? {
          entries.push(entry);
        }
        Ok(entries)
      }
    }

    deserializer.deserialize_map(OrderedPathsVisitor).map(Self)
  }
}

#[derive(Default)]
struct EffectiveConfig {
  base_url: Option<PathBuf>,
  paths: Option<(PathPatterns, PathBuf)>,
}

/// Safety net for `extends` cycles the canonical-path check cannot see.
const MAX_EXTENDS_DEPTH: usize = 64;

fn load_chain(
  tsconfig: &Path,
  cwd: &Path,
  projects: &[Project],
  chain: &mut Vec<PathBuf>,
  config_dir: &Path,
) -> Option<EffectiveConfig> {
  let canonical = tsconfig
    .canonicalize()
    .unwrap_or_else(|_| tsconfig.to_path_buf());
  if chain.contains(&canonical) || chain.len() >= MAX_EXTENDS_DEPTH {
    warn!("Circular tsconfig extends at {}", tsconfig.display());
    return None;
  }

  let json = read_tsconfig(tsconfig)?;
  let dir = tsconfig.parent()?.to_path_buf();

  chain.push(canonical);
  // TypeScript applies `extends` entries in order, each overriding the previous one,
  // then the config's own options override them all. `paths` is replaced, never merged.
  let mut effective = EffectiveConfig::default();
  let parents = match json.extends {
    Some(Extends::Single(specifier)) => vec![specifier],
    Some(Extends::Multiple(specifiers)) => specifiers,
    None => vec![],
  };
  for specifier in parents {
    let Some(parent) = resolve_extends(&dir, &specifier, cwd, projects) else {
      warn!(
        "Cannot resolve tsconfig extends '{}' from {}",
        specifier,
        tsconfig.display()
      );
      continue;
    };
    if let Some(inherited) = load_chain(&parent, cwd, projects, chain, config_dir) {
      effective.base_url = inherited.base_url.or(effective.base_url);
      effective.paths = inherited.paths.or(effective.paths);
    }
  }
  chain.pop();

  if let Some(options) = json.compiler_options {
    if let Some(base_url) = options.base_url {
      effective.base_url = Some(dir.join(expand_config_dir(&base_url, config_dir)));
    }
    if let Some(paths) = options.paths {
      let patterns = paths
        .0
        .into_iter()
        .map(|(key, targets)| {
          let targets = targets
            .iter()
            .map(|target| expand_config_dir(target, config_dir))
            .collect();
          (key, targets)
        })
        .collect();
      effective.paths = Some((patterns, dir));
    }
  }
  Some(effective)
}

/// Substitutes TypeScript's `${configDir}` template. The result is absolute, so a later
/// `join` onto the declaring config's directory yields the consumer's path rather than the
/// shared config's.
fn expand_config_dir(value: &str, config_dir: &Path) -> String {
  const TEMPLATE: &str = "${configDir}";
  if !value.contains(TEMPLATE) {
    return value.to_string();
  }
  value.replace(TEMPLATE, &config_dir.to_string_lossy())
}

/// The part of `specifier` after `name`, as a path inside that package. `None` when the
/// specifier names a different package.
fn strip_package_prefix(specifier: &str, name: &str, root: &Path) -> Option<String> {
  let subpath = specifier.strip_prefix(name)?;
  match subpath.strip_prefix('/') {
    Some(subpath) => Some(subpath.to_string()),
    // A bare package name means the config the package points at, which TypeScript takes
    // from its `tsconfig` field before falling back to `tsconfig.json` — a config package
    // may ship only `base.json`.
    None if subpath.is_empty() => Some(
      package_manifest(root)
        .and_then(|manifest| manifest.tsconfig)
        .unwrap_or_else(|| "tsconfig.json".to_string()),
    ),
    None => None,
  }
}

#[derive(Deserialize)]
struct PackageManifest {
  name: Option<String>,
  tsconfig: Option<String>,
}

fn package_manifest(root: &Path) -> Option<PackageManifest> {
  let contents = std::fs::read_to_string(root.join("package.json")).ok()?;
  serde_json::from_str::<PackageManifest>(&contents).ok()
}

fn resolve_extends(
  dir: &Path,
  specifier: &str,
  cwd: &Path,
  projects: &[Project],
) -> Option<PathBuf> {
  if specifier.starts_with('.') || Path::new(specifier).is_absolute() {
    return existing_config(dir.join(specifier));
  }

  let workspace_project = projects
    .iter()
    .filter_map(|project| {
      let root = if project.root.is_absolute() {
        project.root.clone()
      } else {
        cwd.join(&project.root)
      };
      // An Nx project name routinely differs from the npm package name the specifier
      // uses (`ui-widgets` vs `@acme/shared-ui-widgets`), so match on either. Without
      // the package name a bare `extends` only resolves via node_modules, which is
      // absent on a fresh CI checkout.
      let subpath = strip_package_prefix(specifier, &project.name, &root).or_else(|| {
        package_manifest(&root)
          .and_then(|manifest| manifest.name)
          .and_then(|name| strip_package_prefix(specifier, &name, &root))
      })?;
      existing_config(root.join(subpath))
    })
    .next();

  workspace_project.or_else(|| {
    dir
      .ancestors()
      .take_while(|ancestor| ancestor.starts_with(cwd))
      .find_map(|ancestor| {
        let package = ancestor.join("node_modules").join(specifier);
        if package.is_dir() {
          // Same as a workspace package: the manifest's `tsconfig` field names the config
          // when the package ships one under another name.
          let entry = package_manifest(&package)
            .and_then(|manifest| manifest.tsconfig)
            .unwrap_or_else(|| "tsconfig.json".to_string());
          existing_config(package.join(entry))
        } else {
          existing_config(package)
        }
      })
  })
}

/// Drop `.` and fold `..` segments without touching the filesystem. Canonicalizing
/// would also resolve symlinks and move paths out from under the workspace root.
fn normalize(path: &Path) -> PathBuf {
  let mut normalized = PathBuf::new();
  for component in path.components() {
    match component {
      Component::CurDir => {}
      Component::ParentDir => {
        normalized.pop();
      }
      other => normalized.push(other),
    }
  }
  normalized
}

/// TypeScript accepts an `extends` target with or without its `.json` extension.
fn existing_config(path: PathBuf) -> Option<PathBuf> {
  if path.is_file() {
    return Some(path);
  }
  let mut with_extension = path.into_os_string();
  with_extension.push(".json");
  let with_extension = PathBuf::from(with_extension);
  with_extension.is_file().then_some(with_extension)
}

/// tsconfig files are JSONC: comments and trailing commas are both valid. The buffer
/// form of `json_strip_comments` removes both, the streaming reader only comments.
fn read_tsconfig(path: &Path) -> Option<TsconfigJson> {
  let mut content = std::fs::read_to_string(path)
    .map_err(|e| warn!("Failed to read {}: {}", path.display(), e))
    .ok()?;
  json_strip_comments::strip(&mut content)
    .map_err(|e| warn!("Failed to strip comments from {}: {}", path.display(), e))
    .ok()?;
  serde_json::from_str(&content)
    .map_err(|e| warn!("Failed to parse {}: {}", path.display(), e))
    .ok()
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use tempfile::TempDir;

  fn project(name: &str, root: &str) -> Project {
    Project {
      name: name.to_string(),
      root: PathBuf::from(root),
      source_root: PathBuf::from(format!("{root}/src")),
      ts_config: None,
      implicit_dependencies: vec![],
      targets: vec![],
    }
  }

  #[test]
  fn test_bare_extends_resolves_through_workspace_project() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join("tools/ts-config")).unwrap();
    fs::create_dir_all(cwd.join("apps/web")).unwrap();
    fs::write(
      cwd.join("tools/ts-config/tsconfig.front.json"),
      r#"{ "extends": "./tsconfig.base", "compilerOptions": { "jsx": "react-jsx" } }"#,
    )
    .unwrap();
    fs::write(
      cwd.join("tools/ts-config/tsconfig.base.json"),
      r#"{ "compilerOptions": { "paths": { "@shared/*": ["shared/*"] } } }"#,
    )
    .unwrap();
    fs::write(
      cwd.join("apps/web/tsconfig.json"),
      r#"{
  // no node_modules in this workspace
  "extends": "@scope/ts-config/tsconfig.front.json",
  "compilerOptions": { "paths": { "$pages/*": ["./src/pages/*"] } }
}"#,
    )
    .unwrap();

    let projects = vec![project("@scope/ts-config", "tools/ts-config")];
    let paths = TsconfigPaths::load(&cwd.join("apps/web/tsconfig.json"), cwd, &projects).unwrap();

    assert_eq!(
      paths.candidates("$pages/Fiche/index"),
      vec![cwd.join("apps/web/src/pages/Fiche/index")]
    );
    assert!(
      paths.candidates("@shared/x").is_empty(),
      "own `paths` replaces the inherited map"
    );
  }

  #[test]
  fn test_inherited_paths_resolve_against_declaring_config() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join("apps/web")).unwrap();
    fs::write(
      cwd.join("tsconfig.shared.json"),
      r#"{ "compilerOptions": { "paths": { "@shared/*": ["libs/shared/*"] } } }"#,
    )
    .unwrap();
    fs::write(
      cwd.join("apps/web/tsconfig.json"),
      r#"{ "extends": "../../tsconfig.shared.json" }"#,
    )
    .unwrap();

    let paths = TsconfigPaths::load(&cwd.join("apps/web/tsconfig.json"), cwd, &[]).unwrap();

    assert_eq!(
      paths.candidates("@shared/button"),
      vec![cwd.join("libs/shared/button")]
    );
  }

  #[test]
  fn test_base_url_overrides_paths_directory() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::write(
      cwd.join("tsconfig.json"),
      r#"{ "compilerOptions": { "baseUrl": "src", "paths": { "~/*": ["*"] } } }"#,
    )
    .unwrap();

    let paths = TsconfigPaths::load(&cwd.join("tsconfig.json"), cwd, &[]).unwrap();

    // A baseUrl candidate is appended after the pattern's target, so assert the winner
    // rather than the whole list.
    assert_eq!(
      paths.candidates("~/utils").first(),
      Some(&cwd.join("src/utils"))
    );
  }

  #[test]
  fn test_candidates_prefer_exact_then_longest_prefix() {
    let paths = TsconfigPaths {
      base_dir: PathBuf::from("/ws"),
      base_url: None,
      patterns: vec![
        ("$i18n".to_string(), vec!["i18n/index".to_string()]),
        ("$i18n/*".to_string(), vec!["i18n/*".to_string()]),
        ("*".to_string(), vec!["types/*".to_string()]),
        (
          "$i18n/locales/*.json".to_string(),
          vec!["locales/*.json".to_string()],
        ),
      ],
    };

    assert_eq!(
      paths.candidates("$i18n"),
      vec![PathBuf::from("/ws/i18n/index")]
    );
    assert_eq!(
      paths.candidates("$i18n/fr"),
      vec![PathBuf::from("/ws/i18n/fr")]
    );
    assert_eq!(
      paths.candidates("$i18n/locales/fr.json"),
      vec![PathBuf::from("/ws/locales/fr.json")]
    );
    assert_eq!(
      paths.candidates("react"),
      vec![PathBuf::from("/ws/types/react")]
    );
  }

  #[test]
  fn test_trailing_commas_and_comments() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::write(
      cwd.join("tsconfig.json"),
      r#"{
  // JSONC, as TypeScript accepts it
  "compilerOptions": {
    "paths": { "$pages/*": ["./src/pages/*",], },
  },
}"#,
    )
    .unwrap();

    let paths = TsconfigPaths::load(&cwd.join("tsconfig.json"), cwd, &[]).unwrap();

    assert_eq!(
      paths.candidates("$pages/Home"),
      vec![cwd.join("src/pages/Home")]
    );
  }

  /// TypeScript's `getPathsBasePath` is `options.baseUrl ?? options.pathsBasePath`: an
  /// effective `baseUrl` wins even when `paths` was declared by a parent config.
  #[test]
  fn test_child_base_url_applies_to_inherited_paths() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join("apps/web")).unwrap();
    fs::write(
      cwd.join("tsconfig.shared.json"),
      r#"{ "compilerOptions": { "paths": { "@lib/*": ["lib/*"] } } }"#,
    )
    .unwrap();
    fs::write(
      cwd.join("apps/web/tsconfig.json"),
      r#"{ "extends": "../../tsconfig.shared.json", "compilerOptions": { "baseUrl": "." } }"#,
    )
    .unwrap();

    let paths = TsconfigPaths::load(&cwd.join("apps/web/tsconfig.json"), cwd, &[]).unwrap();

    assert_eq!(
      paths.candidates("@lib/button").first(),
      Some(&cwd.join("apps/web/lib/button"))
    );
  }

  #[test]
  fn test_no_paths_in_chain() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::write(
      cwd.join("tsconfig.json"),
      r#"{ "extends": "@missing/config" }"#,
    )
    .unwrap();

    assert!(TsconfigPaths::load(&cwd.join("tsconfig.json"), cwd, &[]).is_none());
  }

  /// A project with `baseUrl` and no `paths` still resolves its non-relative imports.
  /// Discarding the config left them matching neither a project name nor a root alias, so
  /// they never entered the import graph at all.
  #[test]
  fn test_base_url_without_paths_resolves_bare_specifier() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(cwd.join("src/features")).unwrap();
    fs::write(
      cwd.join("tsconfig.json"),
      r#"{ "compilerOptions": { "baseUrl": "./src" } }"#,
    )
    .unwrap();
    fs::write(cwd.join("src/features/router.ts"), "export const r = 1;").unwrap();

    let paths = TsconfigPaths::load(&cwd.join("tsconfig.json"), &cwd, &[]).unwrap();
    assert_eq!(
      paths.candidates("features/router"),
      vec![cwd.join("src/features/router")],
      "a bare specifier resolves against baseUrl when no paths key matches"
    );
  }

  /// A matched `paths` key whose targets do not exist is not the end of resolution:
  /// TypeScript then tries `baseUrl`. Returning only the missing target let the import
  /// drop out of the graph.
  #[test]
  fn test_base_url_tried_after_failed_path_targets() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(cwd.join("src/features")).unwrap();
    fs::write(
      cwd.join("tsconfig.json"),
      r#"{ "compilerOptions": { "baseUrl": "src", "paths": { "*": ["generated/*"] } } }"#,
    )
    .unwrap();
    fs::write(cwd.join("src/features/router.ts"), "export const r = 1;").unwrap();

    let paths = TsconfigPaths::load(&cwd.join("tsconfig.json"), &cwd, &[]).unwrap();
    let candidates = paths.candidates("features/router");

    assert_eq!(
      candidates.last(),
      Some(&cwd.join("src/features/router")),
      "baseUrl must be tried after the pattern's targets, got {candidates:?}"
    );
    assert!(
      candidates.len() > 1,
      "the pattern's own target must still come first, got {candidates:?}"
    );
  }

  /// A config package that ships only `base.json` points at it from package.json's
  /// `tsconfig` field; TypeScript accepts `extends: "@scope/config"` for it.
  #[test]
  fn test_bare_extends_uses_package_tsconfig_field() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(cwd.join("libs/config")).unwrap();
    fs::create_dir_all(cwd.join("apps/web")).unwrap();
    fs::write(
      cwd.join("libs/config/package.json"),
      r#"{ "name": "@acme/config", "tsconfig": "base.json" }"#,
    )
    .unwrap();
    fs::write(
      cwd.join("libs/config/base.json"),
      r#"{ "compilerOptions": { "paths": { "$shared/*": ["../shared/*"] } } }"#,
    )
    .unwrap();
    fs::write(
      cwd.join("apps/web/tsconfig.json"),
      r#"{ "extends": "@acme/config" }"#,
    )
    .unwrap();

    let project = Project {
      name: "config".to_string(),
      root: PathBuf::from("libs/config"),
      source_root: PathBuf::from("libs/config"),
      ts_config: None,
      implicit_dependencies: vec![],
      targets: vec![],
    };

    assert!(
      TsconfigPaths::load(&cwd.join("apps/web/tsconfig.json"), &cwd, &[project]).is_some(),
      "a bare extends must follow package.json's tsconfig field, not assume tsconfig.json"
    );
  }

  /// An installed package can name its config through `package.json`'s `tsconfig` field
  /// too, so the node_modules fallback must not assume `tsconfig.json` either.
  #[test]
  fn test_node_modules_extends_uses_package_tsconfig_field() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    let package = cwd.join("node_modules/@acme/config");
    fs::create_dir_all(&package).unwrap();
    fs::write(
      package.join("package.json"),
      r#"{ "name": "@acme/config", "tsconfig": "base.json" }"#,
    )
    .unwrap();
    fs::write(
      package.join("base.json"),
      r#"{ "compilerOptions": { "paths": { "$shared/*": ["shared/*"] } } }"#,
    )
    .unwrap();
    fs::write(
      cwd.join("tsconfig.json"),
      r#"{ "extends": "@acme/config" }"#,
    )
    .unwrap();

    assert!(
      TsconfigPaths::load(&cwd.join("tsconfig.json"), &cwd, &[]).is_some(),
      "an installed package's tsconfig field must be followed, not assumed"
    );
  }

  /// `${configDir}` resolves against the consuming config's directory, so a shared base
  /// config can be reused by several projects. Left literal it produced a path under the
  /// shared config instead, and every alias built on it resolved to nothing.
  #[test]
  fn test_config_dir_expands_to_the_consuming_config() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(cwd.join("tools")).unwrap();
    fs::create_dir_all(cwd.join("apps/web/src")).unwrap();
    fs::write(
      cwd.join("tools/base.json"),
      r#"{ "compilerOptions": { "paths": { "$app/*": ["${configDir}/src/*"] } } }"#,
    )
    .unwrap();
    fs::write(
      cwd.join("apps/web/tsconfig.json"),
      r#"{ "extends": "../../tools/base.json" }"#,
    )
    .unwrap();

    let paths = TsconfigPaths::load(&cwd.join("apps/web/tsconfig.json"), &cwd, &[]).unwrap();
    assert_eq!(
      paths.candidates("$app/home"),
      vec![cwd.join("apps/web/src/home")],
      "the configDir template must expand to the consuming project's directory"
    );
  }

  /// The Nx project name and the npm package name differ, which CLAUDE.md flags as the
  /// workspace pitfall. A bare `extends` uses the package name, so matching only on the
  /// project name leaves it to node_modules — absent on a fresh checkout.
  #[test]
  fn test_bare_extends_resolves_through_package_name() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(cwd.join("libs/ts-config")).unwrap();
    fs::create_dir_all(cwd.join("apps/web")).unwrap();
    fs::write(
      cwd.join("libs/ts-config/package.json"),
      r#"{ "name": "@acme/ts-config" }"#,
    )
    .unwrap();
    fs::write(
      cwd.join("libs/ts-config/base.json"),
      r#"{ "compilerOptions": { "paths": { "$shared/*": ["../../libs/shared/*"] } } }"#,
    )
    .unwrap();
    fs::write(
      cwd.join("apps/web/tsconfig.json"),
      r#"{ "extends": "@acme/ts-config/base.json" }"#,
    )
    .unwrap();

    let project = Project {
      name: "ts-config".to_string(),
      root: PathBuf::from("libs/ts-config"),
      source_root: PathBuf::from("libs/ts-config"),
      ts_config: None,
      implicit_dependencies: vec![],
      targets: vec![],
    };

    assert!(
      TsconfigPaths::load(&cwd.join("apps/web/tsconfig.json"), &cwd, &[project]).is_some(),
      "extends through the npm package name must resolve without node_modules"
    );
  }

  /// `@pkg/*` and `@pkg/*/index` both match `@pkg/foo/index` with a 5-character prefix.
  /// TypeScript keeps the first declared, so the winner must follow declaration order
  /// rather than the hash seed. Run both orders: a fixed tie-break that ignored order
  /// would pass one and fail the other.
  #[test]
  fn test_equal_prefix_patterns_resolve_first_declared() {
    // Whichever pattern is declared first maps to ./first/*, so the expected winner is
    // always "first"; running both orders is what proves the result follows declaration
    // order rather than the pattern text or the hash seed.
    for (first, second) in [("@pkg/*", "@pkg/*/index"), ("@pkg/*/index", "@pkg/*")] {
      let tmp = TempDir::new().unwrap();
      let cwd = tmp.path().canonicalize().unwrap();
      fs::write(
        cwd.join("tsconfig.json"),
        format!(
          r#"{{ "compilerOptions": {{ "paths": {{ "{first}": ["./first/*"], "{second}": ["./second/*"] }} }} }}"#
        ),
      )
      .unwrap();

      let paths = TsconfigPaths::load(&cwd.join("tsconfig.json"), &cwd, &[]).unwrap();
      let candidates = paths.candidates("@pkg/foo/index");

      assert!(
        candidates
          .first()
          .is_some_and(|candidate| candidate.starts_with(cwd.join("first"))),
        "with {first:?} declared before {second:?} its ./first/* target must win, got {candidates:?}"
      );
    }
  }
}
