use super::resolve_options::{create_resolve_options, is_workspace_specifier};
use super::tsconfig_paths::TsconfigPaths;
use crate::types::Project;
use oxc_resolver::Resolver;
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

type TsconfigPathsCache = RwLock<FxHashMap<PathBuf, Option<Arc<TsconfigPaths>>>>;
type TsconfigCandidateCache = RwLock<FxHashMap<PathBuf, Vec<PathBuf>>>;

/// Import resolution shared by the import index, the re-export index and the
/// reference finder, so all three agree on what a specifier points at.
///
/// An import first goes through the `paths` of the nearest `tsconfig.json` above the
/// importing file, so aliases a project declares for itself (e.g. an app's `$pages/*`)
/// reach their target. `tsconfig.base.json` alone only knows the workspace-wide ones.
/// When no such alias matches or its targets do not exist, the import resolves through
/// `tsconfig.base.json` as before.
///
/// The nearest `tsconfig.json` stands in for the one that owns the file: its `include`
/// and `exclude` are not checked. A file a stricter config would leave out still sees
/// that config's aliases, which can only add import edges.
///
/// An Nx project is often built with `tsconfig.lib.json`, `tsconfig.app.json` or a build
/// target's `tsConfig` rather than a `tsconfig.json`, so the ancestor walk alone would miss
/// aliases declared only there. Those are tried after the ancestor's, never instead of it:
/// a specifier that resolves today keeps resolving through the same config.
///
/// `Sync`: the import index resolves in parallel, so both caches sit behind locks.
pub(crate) struct WorkspaceResolver {
  cwd: PathBuf,
  projects: Vec<Project>,
  root_path_prefixes: Vec<String>,
  resolver: Resolver,
  candidates_by_directory: TsconfigCandidateCache,
  paths_by_tsconfig: TsconfigPathsCache,
}

impl WorkspaceResolver {
  pub(crate) fn new(cwd: &Path, projects: Vec<Project>, root_path_prefixes: Vec<String>) -> Self {
    // Absolute once, here. `workspace_relative` strips this prefix off paths the resolver
    // returns, and those are absolute — so a relative `--cwd` made every strip fail and the
    // whole workspace resolve to nothing. Every other cwd comparison below assumes the same
    // footing.
    let cwd = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    Self {
      resolver: Resolver::new(create_resolve_options(&cwd, &projects)),
      cwd,
      projects,
      root_path_prefixes,
      candidates_by_directory: RwLock::default(),
      paths_by_tsconfig: RwLock::default(),
    }
  }

  /// Resolve `specifier` imported from the workspace-relative `importing_file` to a
  /// workspace-relative path. `None` when the import is external, unresolvable, or
  /// resolves outside the workspace.
  pub(crate) fn resolve(&self, importing_file: &Path, specifier: &str) -> Option<PathBuf> {
    let from_path = self.cwd.join(importing_file);
    let context = from_path.parent()?;

    if !is_relative_specifier(specifier) {
      for paths in self.tsconfig_paths_for(context) {
        let aliased = paths
          .candidates(specifier)
          .into_iter()
          .find_map(|candidate| {
            let resolution = self.resolver.resolve(context, candidate.to_str()?).ok()?;
            self.workspace_relative(resolution.path())
          });
        if aliased.is_some() {
          return aliased;
        }
      }
    }

    if !is_workspace_specifier(specifier, &self.projects, &self.root_path_prefixes) {
      return None;
    }

    match self.resolver.resolve(context, specifier) {
      Ok(resolution) => self.workspace_relative(resolution.path()),
      Err(_) => super::simple_resolve_relative(&self.cwd, context, specifier),
    }
  }

  fn workspace_relative(&self, path: &Path) -> Option<PathBuf> {
    path.strip_prefix(&self.cwd).ok().map(Path::to_path_buf)
  }

  /// The configs whose `paths` apply to a file in `directory`, nearest first.
  fn tsconfig_paths_for(&self, directory: &Path) -> Vec<Arc<TsconfigPaths>> {
    self
      .tsconfig_candidates(directory)
      .into_iter()
      .filter_map(|tsconfig| self.load_tsconfig_paths(tsconfig))
      .collect()
  }

  fn tsconfig_candidates(&self, directory: &Path) -> Vec<PathBuf> {
    if let Some(cached) = read_candidate_cache(&self.candidates_by_directory, directory) {
      return cached;
    }

    let ancestor = directory
      .ancestors()
      .take_while(|ancestor| ancestor.starts_with(&self.cwd))
      .map(|ancestor| ancestor.join("tsconfig.json"))
      .find(|tsconfig| tsconfig.is_file());

    let configured = self
      .owning_project_tsconfig(directory)
      .filter(|tsconfig| tsconfig.is_file() && Some(tsconfig) != ancestor.as_ref());

    // Most specific first, by the directory each config sits in. On a tie the project's
    // configured tsconfig goes first: a `tsconfig.json` beside `tsconfig.lib.json` is the
    // ancestor walk's guess, while the configured one is what Nx actually builds the
    // project with, so it decides an alias the two define differently.
    let mut candidates: Vec<PathBuf> = [configured, ancestor].into_iter().flatten().collect();
    candidates.sort_by_key(|tsconfig| std::cmp::Reverse(config_depth(tsconfig)));

    write_candidate_cache(
      &self.candidates_by_directory,
      directory.to_path_buf(),
      candidates.clone(),
    );
    candidates
  }

  /// The `tsConfig` recorded for the innermost project containing `directory`. Nx records
  /// the config the project is actually built with, which the ancestor walk cannot infer
  /// from the filename alone.
  fn owning_project_tsconfig(&self, directory: &Path) -> Option<PathBuf> {
    let relative = directory.strip_prefix(&self.cwd).ok()?;
    let ts_config = self
      .projects
      .iter()
      .filter(|project| project_contains(&project.root, relative))
      .max_by_key(|project| project_root_depth(&project.root))?
      .ts_config
      .as_ref()?;

    // `Project::ts_config` has no single representation: the Nx loader stores `cwd.join(..)`
    // — which is still syntactically relative when `--cwd` itself is — while the Node API
    // supplies a workspace-relative path. Testing `is_absolute` picks wrong for the first,
    // joining `cwd` twice and losing the config, so try both readings and take the one that
    // is actually a file rather than inferring which kind it is.
    [ts_config.clone(), self.cwd.join(ts_config)]
      .into_iter()
      .find(|candidate| candidate.is_file())
      // Absolute, because its directory becomes the base for `paths` targets and the
      // resolver reads a relative-looking candidate as relative to the importing file.
      .map(|candidate| std::path::absolute(&candidate).unwrap_or(candidate))
  }

  fn load_tsconfig_paths(&self, tsconfig: PathBuf) -> Option<Arc<TsconfigPaths>> {
    if let Some(cached) = read_cache(&self.paths_by_tsconfig, &tsconfig) {
      return cached;
    }
    let paths = TsconfigPaths::load(&tsconfig, &self.cwd, &self.projects).map(Arc::new);
    write_cache(&self.paths_by_tsconfig, tsconfig, paths.clone());
    paths
  }
}

/// TypeScript's `pathIsRelative`: `./x`, `../x`, `.` and `..` resolve from the importing
/// file and never through `paths`, even a catch-all `"*"` key.
fn is_relative_specifier(specifier: &str) -> bool {
  specifier
    .strip_prefix("..")
    .or_else(|| specifier.strip_prefix('.'))
    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'))
}

/// A workspace-root project has an empty root (the Nx loader's `strip_prefix(cwd)`) or a
/// literal `"."`, and contains every file. Unlike the sourceRoot fallback index in
/// `utils`, ranking by depth means it is only ever the last resort, so including it costs
/// nothing and is the only way a root-level app's configured tsconfig is ever consulted.
fn project_contains(root: &Path, relative: &Path) -> bool {
  is_workspace_root(root) || relative.starts_with(root)
}

fn project_root_depth(root: &Path) -> usize {
  if is_workspace_root(root) {
    0
  } else {
    root.components().count()
  }
}

fn config_depth(tsconfig: &Path) -> usize {
  tsconfig.parent().map_or(0, |dir| dir.components().count())
}

fn is_workspace_root(root: &Path) -> bool {
  root.as_os_str().is_empty() || root == Path::new(".")
}

/// A poisoned lock only means another thread panicked mid-insert; the map itself is
/// still a valid cache, so keep using it.
fn read_cache(cache: &TsconfigPathsCache, key: &Path) -> Option<Option<Arc<TsconfigPaths>>> {
  cache
    .read()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .get(key)
    .cloned()
}

fn write_cache(cache: &TsconfigPathsCache, key: PathBuf, value: Option<Arc<TsconfigPaths>>) {
  cache
    .write()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .insert(key, value);
}

fn read_candidate_cache(cache: &TsconfigCandidateCache, key: &Path) -> Option<Vec<PathBuf>> {
  cache
    .read()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .get(key)
    .cloned()
}

fn write_candidate_cache(cache: &TsconfigCandidateCache, key: PathBuf, value: Vec<PathBuf>) {
  cache
    .write()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .insert(key, value);
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use tempfile::TempDir;

  #[test]
  fn test_paths_skip_relative_specifiers() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(cwd.join("src")).unwrap();
    fs::create_dir_all(cwd.join("types")).unwrap();
    fs::write(
      cwd.join("tsconfig.json"),
      r#"{ "compilerOptions": { "paths": { "*": ["./types/*"] } } }"#,
    )
    .unwrap();
    fs::write(cwd.join("src/foo.ts"), "export const foo = 1;").unwrap();
    fs::write(cwd.join("types/foo.ts"), "export const foo = 2;").unwrap();
    fs::write(cwd.join("types/react.ts"), "export const react = 1;").unwrap();

    let resolver = WorkspaceResolver::new(&cwd, vec![], vec![]);

    assert_eq!(
      resolver.resolve(Path::new("src/app.ts"), "./foo"),
      Some(PathBuf::from("src/foo.ts")),
      "a relative import resolves from the importing file, never through `paths`"
    );
    assert_eq!(
      resolver.resolve(Path::new("src/app.ts"), "react"),
      Some(PathBuf::from("types/react.ts")),
      "a bare specifier still goes through `paths`"
    );
  }

  /// An Nx library declares its aliases in `tsconfig.lib.json`, the config Nx records in
  /// `Project::ts_config`. No `tsconfig.json` anywhere declares them, so the ancestor walk
  /// alone resolves nothing and the importing project is never marked affected.
  #[test]
  fn test_paths_from_project_configured_tsconfig() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(cwd.join("libs/ui/src")).unwrap();
    fs::write(
      cwd.join("libs/ui/tsconfig.lib.json"),
      r#"{ "compilerOptions": { "paths": { "$ui/*": ["./src/*"] } } }"#,
    )
    .unwrap();
    fs::write(
      cwd.join("libs/ui/src/button.ts"),
      "export const button = 1;",
    )
    .unwrap();

    let project = Project {
      name: "ui".to_string(),
      root: PathBuf::from("libs/ui"),
      source_root: PathBuf::from("libs/ui/src"),
      ts_config: Some(PathBuf::from("libs/ui/tsconfig.lib.json")),
      implicit_dependencies: vec![],
      targets: vec![],
    };

    let without_project = WorkspaceResolver::new(&cwd, vec![], vec![]);
    assert_eq!(
      without_project.resolve(Path::new("libs/ui/src/app.ts"), "$ui/button"),
      None,
      "no tsconfig.json declares the alias, so the ancestor walk cannot find it"
    );

    let resolver = WorkspaceResolver::new(&cwd, vec![project], vec![]);
    assert_eq!(
      resolver.resolve(Path::new("libs/ui/src/app.ts"), "$ui/button"),
      Some(PathBuf::from("libs/ui/src/button.ts")),
      "the project's configured tsconfig supplies the alias"
    );
  }

  /// When both sit in the project directory, the config Nx builds with decides an alias
  /// the two define differently — the sibling `tsconfig.json` is only the ancestor walk's
  /// guess at which config owns the file.
  #[test]
  fn test_configured_tsconfig_wins_over_sibling_ancestor() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(cwd.join("libs/ui/src")).unwrap();
    fs::create_dir_all(cwd.join("libs/ui/shared")).unwrap();
    fs::write(
      cwd.join("libs/ui/tsconfig.json"),
      r#"{ "compilerOptions": { "paths": { "$ui/*": ["./shared/*"] } } }"#,
    )
    .unwrap();
    fs::write(
      cwd.join("libs/ui/tsconfig.lib.json"),
      r#"{ "compilerOptions": { "paths": { "$ui/*": ["./src/*"] } } }"#,
    )
    .unwrap();
    fs::write(cwd.join("libs/ui/shared/button.ts"), "export const a = 1;").unwrap();
    fs::write(cwd.join("libs/ui/src/button.ts"), "export const b = 2;").unwrap();

    let project = Project {
      name: "ui".to_string(),
      root: PathBuf::from("libs/ui"),
      source_root: PathBuf::from("libs/ui/src"),
      ts_config: Some(PathBuf::from("libs/ui/tsconfig.lib.json")),
      implicit_dependencies: vec![],
      targets: vec![],
    };

    let resolver = WorkspaceResolver::new(&cwd, vec![project], vec![]);
    assert_eq!(
      resolver.resolve(Path::new("libs/ui/src/app.ts"), "$ui/button"),
      Some(PathBuf::from("libs/ui/src/button.ts")),
      "the project's configured tsconfig decides, not the sibling tsconfig.json"
    );
  }

  /// Specificity still orders the candidates: a `tsconfig.json` deeper than the project
  /// root is closer to the file than the project's configured config, so it goes first.
  #[test]
  fn test_deeper_ancestor_tsconfig_wins_over_configured() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(cwd.join("libs/ui/src/nested")).unwrap();
    fs::create_dir_all(cwd.join("libs/ui/lib-target")).unwrap();
    fs::write(
      cwd.join("libs/ui/tsconfig.lib.json"),
      r#"{ "compilerOptions": { "paths": { "$ui/*": ["./lib-target/*"] } } }"#,
    )
    .unwrap();
    fs::write(
      cwd.join("libs/ui/src/nested/tsconfig.json"),
      r#"{ "compilerOptions": { "paths": { "$ui/*": ["./deep/*"] } } }"#,
    )
    .unwrap();
    fs::create_dir_all(cwd.join("libs/ui/src/nested/deep")).unwrap();
    fs::write(
      cwd.join("libs/ui/lib-target/button.ts"),
      "export const a = 1;",
    )
    .unwrap();
    fs::write(
      cwd.join("libs/ui/src/nested/deep/button.ts"),
      "export const b = 2;",
    )
    .unwrap();

    let project = Project {
      name: "ui".to_string(),
      root: PathBuf::from("libs/ui"),
      source_root: PathBuf::from("libs/ui/src"),
      ts_config: Some(PathBuf::from("libs/ui/tsconfig.lib.json")),
      implicit_dependencies: vec![],
      targets: vec![],
    };

    let resolver = WorkspaceResolver::new(&cwd, vec![project], vec![]);
    assert_eq!(
      resolver.resolve(Path::new("libs/ui/src/nested/app.ts"), "$ui/button"),
      Some(PathBuf::from("libs/ui/src/nested/deep/button.ts")),
      "a tsconfig.json nearer the file outranks the project's configured one"
    );
  }

  /// The Nx loader stores `cwd.join(tsConfig)`, which is still syntactically relative when
  /// `--cwd` is. Treating "not absolute" as "workspace-relative" joined `cwd` twice and the
  /// configured config vanished, taking its aliases with it.
  #[test]
  fn test_configured_tsconfig_with_relative_cwd() {
    // Relative to the process directory, so `cwd` itself is a relative path.
    let tmp = TempDir::new_in(".").unwrap();
    let cwd = PathBuf::from(tmp.path().file_name().unwrap());
    fs::create_dir_all(cwd.join("apps/web/src")).unwrap();
    fs::write(
      cwd.join("apps/web/tsconfig.app.json"),
      r#"{ "compilerOptions": { "paths": { "$app/*": ["./src/*"] } } }"#,
    )
    .unwrap();
    fs::write(cwd.join("apps/web/src/home.ts"), "export const home = 1;").unwrap();

    let project = Project {
      name: "web".to_string(),
      root: PathBuf::from("apps/web"),
      source_root: PathBuf::from("apps/web/src"),
      // As the Nx loader stores it: cwd.join(..), relative because cwd is.
      ts_config: Some(cwd.join("apps/web/tsconfig.app.json")),
      implicit_dependencies: vec![],
      targets: vec![],
    };

    let resolver = WorkspaceResolver::new(&cwd, vec![project], vec![]);
    assert_eq!(
      resolver.resolve(Path::new("apps/web/src/main.ts"), "$app/home"),
      Some(PathBuf::from("apps/web/src/home.ts")),
      "a cwd-joined relative tsConfig must not be joined to cwd a second time"
    );
  }

  /// A root-level Nx app has an empty `root` and is built with `tsconfig.app.json`. It is
  /// the least specific match, so it must still be consulted when nothing deeper owns the
  /// file — the sourceRoot index in `utils` excludes it for a different reason.
  #[test]
  fn test_paths_from_workspace_root_project() {
    for root in ["", "."] {
      let tmp = TempDir::new().unwrap();
      let cwd = tmp.path().canonicalize().unwrap();
      fs::create_dir_all(cwd.join("src")).unwrap();
      fs::write(
        cwd.join("tsconfig.app.json"),
        r#"{ "compilerOptions": { "paths": { "$app/*": ["./src/*"] } } }"#,
      )
      .unwrap();
      fs::write(cwd.join("src/home.ts"), "export const home = 1;").unwrap();

      let project = Project {
        name: "app".to_string(),
        root: PathBuf::from(root),
        source_root: PathBuf::from("src"),
        ts_config: Some(PathBuf::from("tsconfig.app.json")),
        implicit_dependencies: vec![],
        targets: vec![],
      };

      let resolver = WorkspaceResolver::new(&cwd, vec![project], vec![]);
      assert_eq!(
        resolver.resolve(Path::new("src/main.ts"), "$app/home"),
        Some(PathBuf::from("src/home.ts")),
        "root project with root={root:?} should supply its configured aliases"
      );
    }
  }

  /// A deeper project still outranks the workspace-root one.
  #[test]
  fn test_deeper_project_outranks_workspace_root() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(cwd.join("libs/ui/src")).unwrap();
    fs::create_dir_all(cwd.join("root-src")).unwrap();
    fs::write(
      cwd.join("tsconfig.app.json"),
      r#"{ "compilerOptions": { "paths": { "$x/*": ["./root-src/*"] } } }"#,
    )
    .unwrap();
    fs::write(
      cwd.join("libs/ui/tsconfig.lib.json"),
      r#"{ "compilerOptions": { "paths": { "$x/*": ["./src/*"] } } }"#,
    )
    .unwrap();
    fs::write(cwd.join("root-src/thing.ts"), "export const a = 1;").unwrap();
    fs::write(cwd.join("libs/ui/src/thing.ts"), "export const b = 2;").unwrap();

    let projects = vec![
      Project {
        name: "app".to_string(),
        root: PathBuf::from(""),
        source_root: PathBuf::from("root-src"),
        ts_config: Some(PathBuf::from("tsconfig.app.json")),
        implicit_dependencies: vec![],
        targets: vec![],
      },
      Project {
        name: "ui".to_string(),
        root: PathBuf::from("libs/ui"),
        source_root: PathBuf::from("libs/ui/src"),
        ts_config: Some(PathBuf::from("libs/ui/tsconfig.lib.json")),
        implicit_dependencies: vec![],
        targets: vec![],
      },
    ];

    let resolver = WorkspaceResolver::new(&cwd, projects, vec![]);
    assert_eq!(
      resolver.resolve(Path::new("libs/ui/src/app.ts"), "$x/thing"),
      Some(PathBuf::from("libs/ui/src/thing.ts")),
      "the innermost project's tsconfig wins over the workspace-root one"
    );
  }
}
