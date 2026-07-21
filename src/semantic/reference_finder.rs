use crate::error::Result;
use crate::profiler::Profiler;
use crate::semantic::WorkspaceAnalyzer;
use crate::types::Reference;
use oxc_resolver::Resolver;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, warn};

/// Cross-file reference finder
pub struct ReferenceFinder<'a> {
  analyzer: &'a WorkspaceAnalyzer,
  resolver: Resolver,
  cwd: PathBuf,
  /// Resolution cache: (from_file, specifier) -> resolved_path
  /// Using RefCell for interior mutability since resolution is logically const
  /// Note: Not thread-safe. For future parallelization, migrate to DashMap or Arc<Mutex<>>
  resolution_cache: RefCell<FxHashMap<(PathBuf, String), Option<PathBuf>>>,
  /// Profiler for performance measurement
  profiler: Arc<Profiler>,
}

impl<'a> ReferenceFinder<'a> {
  pub fn new(analyzer: &'a WorkspaceAnalyzer, cwd: &Path, profiler: Arc<Profiler>) -> Self {
    Self {
      analyzer,
      resolver: Resolver::new(super::create_resolve_options(cwd, &analyzer.projects)),
      cwd: cwd.to_path_buf(),
      resolution_cache: RefCell::new(FxHashMap::default()),
      profiler,
    }
  }

  /// Find all files that import from the given file (regardless of what symbol)
  #[allow(dead_code)]
  pub fn find_files_importing_from(&self, file_path: &Path) -> Result<Vec<Reference>> {
    let mut importing_files = Vec::new();

    debug!("Finding all files importing from {:?}", file_path);

    for (importing_file, file_imports) in &self.analyzer.imports {
      for import in file_imports {
        // Resolve the import to see if it points to file_path
        let resolved = self.resolve_import(importing_file, &import.from_module);

        if let Some(resolved_path) = resolved {
          if self.paths_equal(&resolved_path, file_path) {
            debug!("Found import in {:?}", importing_file);
            importing_files.push(Reference {
              file_path: importing_file.clone(),
              line: 0,
              column: 0,
            });
            break; // Only add each file once
          }
        }
      }
    }

    Ok(importing_files)
  }

  /// Find all cross-file references to a symbol
  pub fn find_cross_file_references(
    &self,
    symbol_name: &str,
    declaring_file: &Path,
  ) -> Result<Vec<Reference>> {
    let mut all_refs = Vec::new();
    let mut visited = FxHashSet::default();

    self.find_refs_recursive(symbol_name, declaring_file, &mut all_refs, &mut visited)?;

    Ok(all_refs)
  }

  fn find_refs_recursive(
    &self,
    symbol_name: &str,
    current_file: &Path,
    all_refs: &mut Vec<Reference>,
    visited: &mut FxHashSet<(PathBuf, String)>,
  ) -> Result<()> {
    let key = (current_file.to_path_buf(), symbol_name.to_string());
    if !visited.insert(key.clone()) {
      return Ok(()); // Already processed
    }

    debug!(
      "Finding references to '{}' from {:?}",
      symbol_name, current_file
    );

    // Record reference lookup
    self.profiler.record_reference_lookup();

    // Use the import index to find direct imports of this symbol
    if let Some(importers) = self.analyzer.import_index.get(&key) {
      for (importing_file, local_name, _from_module, _is_dynamic) in importers {
        debug!(
          "Found import of '{}' in {:?} as '{}'",
          symbol_name, importing_file, local_name
        );

        // Find all references to the local name in the importing file
        match self
          .analyzer
          .find_local_references(importing_file, local_name)
        {
          Ok(local_refs) => {
            all_refs.extend(local_refs);
          }
          Err(e) => {
            warn!("Error finding local references: {}", e);
          }
        }

        // Check if it's re-exported
        if self.is_re_exported(importing_file, local_name) {
          debug!(
            "Symbol '{}' is re-exported from {:?}",
            local_name, importing_file
          );
          // Recursively find references to the re-export
          self.find_refs_recursive(local_name, importing_file, all_refs, visited)?;
        } else {
          // Symbol is used but not re-exported
          // The references found via find_local_references above are sufficient
          // The cascade will happen naturally in core.rs when processing
          // the container symbols that actually use this symbol
          debug!(
            "Symbol '{}' is used in {:?} (not re-exported)",
            local_name, importing_file
          );
        }
      }
    }

    // Also check for namespace imports (import * as foo)
    let namespace_key = (current_file.to_path_buf(), "*".to_string());
    if let Some(importers) = self.analyzer.import_index.get(&namespace_key) {
      // Is the symbol being propagated the module's default export? A default
      // export is recorded either under the name "default" (anonymous default,
      // container-symbol resolution) or under its local identifier with
      // exported_name == "default" (`export default Page`).
      let is_default_export_symbol = symbol_name == "default"
        || self
          .analyzer
          .exports
          .get(current_file)
          .is_some_and(|exports| {
            exports
              .iter()
              .any(|e| e.exported_name == "default" && e.local_name.as_deref() == Some(symbol_name))
          });

      for (importing_file, local_name, _from_module, is_dynamic) in importers {
        debug!(
          "Found {} namespace import in {:?} as '{}' (checking for {}.{})",
          if *is_dynamic { "dynamic" } else { "static" },
          importing_file,
          local_name,
          local_name,
          symbol_name
        );

        if *is_dynamic {
          // The local binding of a dynamic import is synthetic ("__dynamic_import_N",
          // see DynamicImportVisitor) — it never appears as an identifier in the
          // source, so the member-access analysis below can never attribute usage
          // to it: requiring member access makes dynamic imports propagate nothing
          // at all. But `React.lazy(() => import('./Page'))` DOES consume the
          // module's default export — React reads `.default` at runtime, invisibly
          // to static analysis. So cascade exactly that: when the propagated symbol
          // is the module's default export, conservatively mark the importing file
          // affected (line=0/column=0 sentinel — "entire file affected", core.rs).
          // Named exports keep the lazy-boundary isolation introduced in #69:
          // accessing them requires a visible binding (`.then(m => m.foo)`,
          // `await import()`) that this analysis does not track.
          if is_default_export_symbol {
            debug!(
              "Dynamic import of {:?} in {:?} consumes its default export ('{}') — \
               marking importing file conservatively affected",
              current_file, importing_file, symbol_name
            );
            all_refs.push(Reference {
              file_path: importing_file.clone(),
              line: 0,
              column: 0,
            });
          } else {
            debug!(
              "No cascade of non-default symbol '{}' through dynamic import in {:?} \
               (lazy boundary isolates named exports)",
              symbol_name, importing_file
            );
          }
          continue;
        }

        // For static namespace imports, we need to find references to namespace.symbol
        // specifically (e.g., utils.formatDate, not just any reference to utils)
        match self
          .analyzer
          .find_namespace_member_access(importing_file, local_name, symbol_name)
        {
          Ok(member_refs) => {
            if !member_refs.is_empty() {
              // Found actual references to namespace.symbol - these files are definitely affected
              debug!(
                "Found {} references to {}.{} in {:?}",
                member_refs.len(),
                local_name,
                symbol_name,
                importing_file
              );
              all_refs.extend(member_refs);
            }
            // If we don't find any references to 'namespace.symbol', we don't mark
            // the file as affected (strict behavior) since the namespace either
            // doesn't use this specific symbol or is dead code.
          }
          Err(e) => {
            // Propagate the error instead of silently marking as affected
            // This ensures bugs in reference finding don't hide real issues
            return Err(e);
          }
        }
      }
    }

    // Check for re-exports from the same package (barrel files)
    // We need to check files in the same package that might re-export this symbol
    if let Some(exports) = self.analyzer.exports.get(current_file) {
      for export in exports {
        // Skip if not our symbol
        if export.exported_name != symbol_name && export.local_name.as_deref() != Some(symbol_name)
        {
          continue;
        }

        // If this is a re-export from elsewhere, follow it
        if let Some(ref from_module) = export.re_export_from {
          if let Some(resolved) = self.resolve_import(current_file, from_module) {
            debug!(
              "Following re-export of '{}' from {:?} to {:?}",
              symbol_name, current_file, resolved
            );
            self.find_refs_recursive(symbol_name, &resolved, all_refs, visited)?;
          }
        }
      }
    }

    // REVERSE: Find files that re-export FROM the current file (barrel files like index.ts)
    // For example, if clients.module.ts exports ClientsModule, and index.ts re-exports it,
    // we need to look for imports of index.ts
    for (reexporting_file, file_exports) in &self.analyzer.exports {
      for export in file_exports {
        // Check if this export is a re-export from our current_file
        if let Some(ref from_module) = export.re_export_from {
          if let Some(resolved) = self.resolve_import(reexporting_file, from_module) {
            if self.paths_equal(&resolved, current_file) {
              // Handle wildcard re-exports: export * from '...'
              if export.exported_name == "*" {
                debug!(
                  "Found barrel file {:?} with wildcard re-export from {:?}",
                  reexporting_file, current_file
                );
                // Recursively look for imports of the re-exporting file
                // The symbol name stays the same through wildcard re-exports
                self.find_refs_recursive(symbol_name, reexporting_file, all_refs, visited)?;
              } else {
                // Named re-export: export { X } from '...' or export { X as Y } from '...'
                let exported_symbol = export
                  .local_name
                  .as_deref()
                  .unwrap_or(&export.exported_name);
                if exported_symbol == symbol_name {
                  debug!(
                    "Found barrel file {:?} re-exporting '{}' from {:?}",
                    reexporting_file, export.exported_name, current_file
                  );
                  // Recursively look for imports of the re-exporting file
                  self.find_refs_recursive(
                    &export.exported_name,
                    reexporting_file,
                    all_refs,
                    visited,
                  )?;
                }
              }
            }
          }
        }
      }
    }

    Ok(())
  }

  /// Resolve an import specifier to a file path (with caching)
  fn resolve_import(&self, from_file: &Path, specifier: &str) -> Option<PathBuf> {
    let start = if self.profiler.is_enabled() {
      Some(Instant::now())
    } else {
      None
    };

    let cache_key = (from_file.to_path_buf(), specifier.to_string());

    // Check cache first
    {
      let cache = self.resolution_cache.borrow();
      if let Some(cached) = cache.get(&cache_key) {
        if let Some(start_time) = start {
          self
            .profiler
            .record_resolution(true, start_time.elapsed().as_nanos() as u64);
        }
        return cached.clone();
      }
    }

    if !super::is_workspace_specifier(
      specifier,
      &self.analyzer.projects,
      &self.analyzer.tsconfig_path_prefixes,
    ) {
      self.resolution_cache.borrow_mut().insert(cache_key, None);
      if let Some(start_time) = start {
        self
          .profiler
          .record_resolution(false, start_time.elapsed().as_nanos() as u64);
      }
      return None;
    }

    // Not in cache, resolve it
    let from_path = self.cwd.join(from_file);
    let context = from_path.parent()?;

    let resolved = match self.resolver.resolve(context, specifier) {
      Ok(resolution) => {
        let resolved = resolution.path();
        resolved
          .strip_prefix(&self.cwd)
          .ok()
          .map(|p| p.to_path_buf())
      }
      Err(_) => {
        // Try simple relative resolution as fallback
        self.simple_resolve(context, specifier)
      }
    };

    // Cache the result (even if None)
    self
      .resolution_cache
      .borrow_mut()
      .insert(cache_key, resolved.clone());

    if let Some(start_time) = start {
      self
        .profiler
        .record_resolution(false, start_time.elapsed().as_nanos() as u64);
    }

    resolved
  }

  /// Simple fallback resolution for relative imports.
  /// Delegates to the shared free function in `semantic::simple_resolve_relative`.
  fn simple_resolve(&self, context: &Path, specifier: &str) -> Option<PathBuf> {
    super::simple_resolve_relative(&self.cwd, context, specifier)
  }

  /// Check if a symbol is re-exported from a file
  fn is_re_exported(&self, file: &Path, symbol_name: &str) -> bool {
    if let Some(exports) = self.analyzer.exports.get(file) {
      exports.iter().any(|export| {
        export.local_name.as_deref() == Some(symbol_name)
          || (export.exported_name == symbol_name && export.local_name.is_none())
      })
    } else {
      false
    }
  }

  /// Compare two paths for equality (handling relative vs absolute)
  fn paths_equal(&self, path1: &Path, path2: &Path) -> bool {
    // Normalize both paths
    let p1 = if path1.is_absolute() {
      path1.strip_prefix(&self.cwd).unwrap_or(path1)
    } else {
      path1
    };

    let p2 = if path2.is_absolute() {
      path2.strip_prefix(&self.cwd).unwrap_or(path2)
    } else {
      path2
    };

    p1 == p2
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::profiler::Profiler;
  use crate::semantic::WorkspaceAnalyzer;
  use std::fs;
  use tempfile::TempDir;

  #[test]
  fn test_simple_resolve_appends_extensions() {
    // Test that simple_resolve appends extensions instead of replacing them
    // This is important for patterns like colors.css -> colors.css.ts

    // Create a temporary directory with test files
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let cwd = temp_dir.path();

    // Create a test file: libs/theme/colors.css.ts
    let theme_dir = cwd.join("libs").join("theme");
    fs::create_dir_all(&theme_dir).expect("Failed to create theme dir");
    let css_ts_file = theme_dir.join("colors.css.ts");
    fs::write(&css_ts_file, "export const red = '#ff0000';").expect("Failed to write test file");

    // Create analyzer and reference finder
    let profiler = Arc::new(Profiler::new(false));
    let analyzer =
      WorkspaceAnalyzer::new(vec![], cwd, profiler.clone()).expect("Failed to create analyzer");
    let reference_finder = ReferenceFinder::new(&analyzer, cwd, profiler);

    // Test: resolve "./colors.css" from libs/theme directory
    // Should find colors.css.ts by appending .ts
    let context = theme_dir.as_path();
    let specifier = "./colors.css";
    let resolved = reference_finder.simple_resolve(context, specifier);

    assert!(
      resolved.is_some(),
      "Expected to resolve colors.css to colors.css.ts"
    );
    let resolved_path = resolved.unwrap();
    assert_eq!(
      resolved_path,
      PathBuf::from("libs/theme/colors.css.ts"),
      "Expected to resolve to colors.css.ts (extension appended)"
    );
  }

  #[test]
  fn test_simple_resolve_standard_extensions() {
    // Test that simple_resolve still works for standard TypeScript imports

    // Create a temporary directory with test files
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let cwd = temp_dir.path();

    // Create a test file: src/utils.ts
    let src_dir = cwd.join("src");
    fs::create_dir_all(&src_dir).expect("Failed to create src dir");
    let utils_file = src_dir.join("utils.ts");
    fs::write(&utils_file, "export function helper() {}").expect("Failed to write test file");

    // Create analyzer and reference finder
    let profiler = Arc::new(Profiler::new(false));
    let analyzer =
      WorkspaceAnalyzer::new(vec![], cwd, profiler.clone()).expect("Failed to create analyzer");
    let reference_finder = ReferenceFinder::new(&analyzer, cwd, profiler);

    // Test: resolve "./utils" from src directory
    // Should find utils.ts by appending .ts
    let context = src_dir.as_path();
    let specifier = "./utils";
    let resolved = reference_finder.simple_resolve(context, specifier);

    assert!(resolved.is_some(), "Expected to resolve utils to utils.ts");
    let resolved_path = resolved.unwrap();
    assert_eq!(
      resolved_path,
      PathBuf::from("src/utils.ts"),
      "Expected to resolve to utils.ts"
    );
  }

  #[test]
  fn test_simple_resolve_index_files() {
    // Test that simple_resolve can find index.ts files in directories

    // Create a temporary directory with test files
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let cwd = temp_dir.path();

    // Create a test file: src/components/index.ts
    let components_dir = cwd.join("src").join("components");
    fs::create_dir_all(&components_dir).expect("Failed to create components dir");
    let index_file = components_dir.join("index.ts");
    fs::write(&index_file, "export * from './Button';").expect("Failed to write test file");

    // Create analyzer and reference finder
    let profiler = Arc::new(Profiler::new(false));
    let analyzer =
      WorkspaceAnalyzer::new(vec![], cwd, profiler.clone()).expect("Failed to create analyzer");
    let reference_finder = ReferenceFinder::new(&analyzer, cwd, profiler);

    // Test: resolve "./components" from src directory
    // Should find components/index.ts
    let context = cwd.join("src");
    let specifier = "./components";
    let resolved = reference_finder.simple_resolve(context.as_path(), specifier);

    assert!(
      resolved.is_some(),
      "Expected to resolve components to components/index.ts"
    );
    let resolved_path = resolved.unwrap();
    assert_eq!(
      resolved_path,
      PathBuf::from("src/components/index.ts"),
      "Expected to resolve to components/index.ts"
    );
  }

  #[test]
  fn test_simple_resolve_js_to_ts_remapping() {
    // Test that imports with .js extensions resolve to .ts files
    // This is common in ESM projects where TS files import with .js extensions
    // e.g., import { foo } from './bar.js' where the actual file is bar.ts

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let cwd = temp_dir.path();

    // Create a test file: src/utils.ts (but NOT src/utils.js)
    let src_dir = cwd.join("src");
    fs::create_dir_all(&src_dir).expect("Failed to create src dir");
    let utils_file = src_dir.join("utils.ts");
    fs::write(&utils_file, "export function helper() {}").expect("Failed to write test file");

    let profiler = Arc::new(Profiler::new(false));
    let analyzer =
      WorkspaceAnalyzer::new(vec![], cwd, profiler.clone()).expect("Failed to create analyzer");
    let reference_finder = ReferenceFinder::new(&analyzer, cwd, profiler);

    // Test: resolve "./utils.js" from src directory
    // Should find utils.ts by stripping .js and trying .ts
    let context = src_dir.as_path();
    let specifier = "./utils.js";
    let resolved = reference_finder.simple_resolve(context, specifier);

    assert!(
      resolved.is_some(),
      "Expected to resolve utils.js to utils.ts"
    );
    let resolved_path = resolved.unwrap();
    assert_eq!(
      resolved_path,
      PathBuf::from("src/utils.ts"),
      "Expected to resolve ./utils.js to utils.ts"
    );
  }

  #[test]
  fn test_simple_resolve_mjs_to_mts_remapping() {
    // TypeScript ESM emits .mjs specifiers for .mts sources, e.g.
    // `export * from './contract.mjs'` where the actual file is contract.mts.
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let cwd = temp_dir.path();

    let src_dir = cwd.join("src");
    fs::create_dir_all(&src_dir).expect("Failed to create src dir");
    fs::write(src_dir.join("contract.mts"), "export const schema = 1;")
      .expect("Failed to write test file");

    let profiler = Arc::new(Profiler::new(false));
    let analyzer =
      WorkspaceAnalyzer::new(vec![], cwd, profiler.clone()).expect("Failed to create analyzer");
    let reference_finder = ReferenceFinder::new(&analyzer, cwd, profiler);

    let resolved = reference_finder.simple_resolve(src_dir.as_path(), "./contract.mjs");
    assert_eq!(
      resolved,
      Some(PathBuf::from("src/contract.mts")),
      "Expected ./contract.mjs to resolve to contract.mts"
    );
  }

  #[test]
  fn test_simple_resolve_cjs_to_cts_remapping() {
    // CommonJS TS counterpart: .cjs specifiers resolve to .cts sources.
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let cwd = temp_dir.path();

    let src_dir = cwd.join("src");
    fs::create_dir_all(&src_dir).expect("Failed to create src dir");
    fs::write(src_dir.join("legacy.cts"), "export const schema = 1;")
      .expect("Failed to write test file");

    let profiler = Arc::new(Profiler::new(false));
    let analyzer =
      WorkspaceAnalyzer::new(vec![], cwd, profiler.clone()).expect("Failed to create analyzer");
    let reference_finder = ReferenceFinder::new(&analyzer, cwd, profiler);

    let resolved = reference_finder.simple_resolve(src_dir.as_path(), "./legacy.cjs");
    assert_eq!(
      resolved,
      Some(PathBuf::from("src/legacy.cts")),
      "Expected ./legacy.cjs to resolve to legacy.cts"
    );
  }

  #[test]
  fn test_simple_resolve_index_mts() {
    // A bare directory specifier should find index.mts (ESM package entry).
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let cwd = temp_dir.path();

    let pkg_src = cwd.join("packages").join("contract").join("src");
    fs::create_dir_all(&pkg_src).expect("Failed to create pkg dir");
    fs::write(pkg_src.join("index.mts"), "export const x = 1;").expect("Failed to write test file");

    let profiler = Arc::new(Profiler::new(false));
    let analyzer =
      WorkspaceAnalyzer::new(vec![], cwd, profiler.clone()).expect("Failed to create analyzer");
    let reference_finder = ReferenceFinder::new(&analyzer, cwd, profiler);

    let context = cwd.join("packages").join("contract");
    let resolved = reference_finder.simple_resolve(context.as_path(), "./src");
    assert_eq!(
      resolved,
      Some(PathBuf::from("packages/contract/src/index.mts")),
      "Expected ./src to resolve to src/index.mts"
    );
  }

  #[test]
  fn test_simple_resolve_js_to_tsx_remapping() {
    // Test that .js imports can resolve to .tsx files

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let cwd = temp_dir.path();

    let src_dir = cwd.join("src");
    fs::create_dir_all(&src_dir).expect("Failed to create src dir");
    let component_file = src_dir.join("Button.tsx");
    fs::write(&component_file, "export const Button = () => <button/>;")
      .expect("Failed to write test file");

    let profiler = Arc::new(Profiler::new(false));
    let analyzer =
      WorkspaceAnalyzer::new(vec![], cwd, profiler.clone()).expect("Failed to create analyzer");
    let reference_finder = ReferenceFinder::new(&analyzer, cwd, profiler);

    let context = src_dir.as_path();
    let specifier = "./Button.js";
    let resolved = reference_finder.simple_resolve(context, specifier);

    assert!(
      resolved.is_some(),
      "Expected to resolve Button.js to Button.tsx"
    );
    let resolved_path = resolved.unwrap();
    assert_eq!(
      resolved_path,
      PathBuf::from("src/Button.tsx"),
      "Expected to resolve ./Button.js to Button.tsx"
    );
  }

  #[test]
  fn test_simple_resolve_index_js_to_index_ts() {
    // Test that ./foo/index.js resolves to ./foo/index.ts

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let cwd = temp_dir.path();

    let src_dir = cwd.join("src");
    let models_dir = src_dir.join("models");
    fs::create_dir_all(&models_dir).expect("Failed to create models dir");
    let index_file = models_dir.join("index.ts");
    fs::write(&index_file, "export * from './User';").expect("Failed to write test file");

    let profiler = Arc::new(Profiler::new(false));
    let analyzer =
      WorkspaceAnalyzer::new(vec![], cwd, profiler.clone()).expect("Failed to create analyzer");
    let reference_finder = ReferenceFinder::new(&analyzer, cwd, profiler);

    let context = src_dir.as_path();
    let specifier = "./models/index.js";
    let resolved = reference_finder.simple_resolve(context, specifier);

    assert!(
      resolved.is_some(),
      "Expected to resolve models/index.js to models/index.ts"
    );
    let resolved_path = resolved.unwrap();
    assert_eq!(
      resolved_path,
      PathBuf::from("src/models/index.ts"),
      "Expected to resolve ./models/index.js to models/index.ts"
    );
  }

  #[test]
  fn test_dynamic_import_cascades_default_export_only() {
    // React.lazy(() => import('./page')) consumes the module's default export
    // through a synthetic binding member-access analysis can never see.
    // The default export must cascade (entire-file sentinel); named exports
    // keep the lazy-boundary isolation.
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    // Canonicalize: on macOS TempDir lives under /var -> /private/var; the
    // resolver returns canonical paths and strip_prefix(cwd) needs them to match.
    let cwd = temp_dir
      .path()
      .canonicalize()
      .expect("Failed to canonicalize temp dir");
    let cwd = cwd.as_path();

    let app_dir = cwd.join("app");
    fs::create_dir_all(&app_dir).expect("Failed to create app dir");
    fs::write(
      app_dir.join("page.tsx"),
      "export default function Page() { return null; }\n\
       export function helper() { return 1; }\n",
    )
    .expect("Failed to write page.tsx");
    fs::write(
      app_dir.join("router.tsx"),
      "import { lazy } from 'react';\n\
       const Page = lazy(() => import('./page'));\n\
       export const routes = [Page];\n",
    )
    .expect("Failed to write router.tsx");

    let profiler = Arc::new(Profiler::new(false));
    let projects = vec![crate::types::Project {
      name: "app".to_string(),
      root: PathBuf::from("app"),
      source_root: PathBuf::from("app"),
      ts_config: None,
      implicit_dependencies: vec![],
      targets: vec![],
    }];
    let analyzer =
      WorkspaceAnalyzer::new(projects, cwd, profiler.clone()).expect("Failed to create analyzer");
    let reference_finder = ReferenceFinder::new(&analyzer, cwd, profiler);

    let page = PathBuf::from("app/page.tsx");
    let router = PathBuf::from("app/router.tsx");

    let default_refs = reference_finder
      .find_cross_file_references("default", &page)
      .expect("find_cross_file_references failed for 'default'");
    assert!(
      default_refs
        .iter()
        .any(|r| r.file_path == router && r.line == 0 && r.column == 0),
      "Dynamic importer should be conservatively affected (sentinel) by a \
       default-export change, got: {:?}",
      default_refs
    );

    let helper_refs = reference_finder
      .find_cross_file_references("helper", &page)
      .expect("find_cross_file_references failed for 'helper'");
    assert!(
      !helper_refs.iter().any(|r| r.file_path == router),
      "Named export must not cascade through the dynamic import (lazy-boundary \
       isolation), got: {:?}",
      helper_refs
    );
  }
}
