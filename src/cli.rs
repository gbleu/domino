use crate::core;
use crate::error::Result;
use crate::profiler::Profiler;
use crate::types::{AffectedReport, LockfileStrategy, RootTsconfigStrategy, TrueAffectedConfig};
use crate::workspace;
use clap::{Parser, Subcommand, ValueEnum};
use colored::Colorize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::debug;

/// Format of the file written by `--report`
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ReportFormat {
  /// Interactive HTML report with a dependency graph
  Html,
  /// The report data as JSON
  Json,
}

#[derive(Parser)]
#[command(name = "domino")]
#[command(about = "True Affected - Semantic change detection for monorepos", long_about = None)]
#[command(version)]
struct Cli {
  #[command(subcommand)]
  command: Commands,

  /// Enable debug logging
  #[arg(short, long, global = true)]
  debug: bool,

  /// CI mode: suppress all logs, only output results
  #[arg(long, global = true)]
  ci: bool,
}

#[derive(Subcommand)]
enum Commands {
  /// Find affected projects
  Affected {
    /// Base branch to compare against
    #[arg(short, long, default_value = "origin/main")]
    base: String,

    /// Current working directory
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Output as JSON
    #[arg(long)]
    json: bool,

    /// Show all projects regardless of changes (no analysis, so no --report)
    #[arg(long, conflicts_with = "report")]
    all: bool,

    /// Enable performance profiling (also: DOMINO_PROFILE=1)
    #[arg(long)]
    profile: bool,

    /// Generate a report of why each project is affected (see --report-format)
    #[arg(long)]
    report: Option<PathBuf>,

    /// Format of the --report file: html (interactive dependency graph) or json
    /// (the report data the HTML embeds as #domino-meta)
    #[arg(long, value_enum, default_value_t = ReportFormat::Html, requires = "report")]
    report_format: ReportFormat,

    /// Head commit to compare (defaults to working tree)
    #[arg(long)]
    head: Option<String>,

    /// Lockfile change detection strategy: none, direct, full
    #[arg(long, default_value = "direct")]
    lockfile_strategy: LockfileStrategy,

    /// How a tsconfig.base.json change affects projects: all, aliases, none
    #[arg(long, default_value = "aliases")]
    root_tsconfig_strategy: RootTsconfigStrategy,
  },
}

pub fn run() -> Result<()> {
  let cli = Cli::parse();

  // Check if json mode is enabled (need to extract from command)
  let json_mode = matches!(&cli.command, Commands::Affected { json: true, .. });

  // Setup logging with cleaner formatting
  // In JSON mode, suppress warnings to ensure clean JSON output
  let log_level = if cli.ci || json_mode {
    "error" // CI/JSON mode: only errors
  } else if cli.debug {
    "debug" // Debug mode: show debug and warnings
  } else {
    "warn" // Default: show warnings
  };
  tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| format!("domino={}", log_level).into()),
    )
    .without_time() // Remove timestamps
    .with_target(false) // Remove module names
    .init();

  match cli.command {
    Commands::Affected {
      base,
      head,
      cwd,
      json,
      all,
      profile,
      report,
      report_format,
      lockfile_strategy,
      root_tsconfig_strategy,
    } => {
      let cwd = match cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir()?,
      };
      let cwd = crate::utils::canonical_workspace_root(&cwd)?;

      // Enable profiling via --profile flag or DOMINO_PROFILE env var
      let enable_profiling = profile || std::env::var("DOMINO_PROFILE").is_ok();
      if enable_profiling && !json {
        eprintln!("📊 Performance profiling enabled");
      }

      // Auto-detect default branch if using the default value
      let base = if base == "origin/main" {
        crate::git::detect_default_branch(&cwd)
      } else {
        base
      };

      debug!("Discovering projects in {:?}", cwd);
      debug!("Head: {:?}", head);

      // Discover projects
      let projects = workspace::discover_projects(&cwd)?;

      if projects.is_empty() {
        if !json {
          eprintln!("{}", "No projects found in workspace".red());
        }
        if let Some(report_path) = &report {
          write_report(
            &AffectedReport::empty(unix_now()),
            report_path,
            report_format,
            json,
          )?;
        }
        return Ok(());
      }

      debug!("Found {} projects", projects.len());

      if all {
        // Show all projects
        let all_projects: Vec<String> = projects.iter().map(|p| p.name.clone()).collect();

        if json {
          println!("{}", serde_json::to_string(&all_projects).unwrap());
        } else {
          println!("{}", "All projects:".bold().blue());
          for project in &all_projects {
            println!("  {} {}", "•".blue(), project);
          }
          println!("\n{} {} projects", "Total:".bold(), all_projects.len());
        }

        return Ok(());
      }

      // Create profiler
      let profiler = Arc::new(Profiler::new(enable_profiling));

      // Run true-affected analysis
      let config = TrueAffectedConfig {
        cwd: cwd.clone(),
        base,
        head,
        projects,
        lockfile_strategy,
        root_tsconfig_strategy,
      };

      // Use the report-generating version if --report is specified
      let result = if report.is_some() {
        core::find_affected_with_report(config, profiler)?
      } else {
        core::find_affected(config, profiler)?
      };

      // Generate the report if requested
      if let Some(report_path) = &report {
        if let Some(report_data) = &result.report {
          write_report(report_data, report_path, report_format, json)?;
        } else if !json {
          eprintln!("{} {}", "⚠".yellow(), "No report data available".yellow());
        }
      }

      if json {
        println!(
          "{}",
          serde_json::to_string(&result.affected_projects).unwrap()
        );
      } else if result.affected_projects.is_empty() {
        println!("{}", "No affected projects".yellow());
      } else {
        println!("{}", "Affected projects:".bold().green());
        for project in &result.affected_projects {
          println!("  {} {}", "•".green(), project);
        }
        println!(
          "\n{} {} affected project{}",
          "Total:".bold(),
          result.affected_projects.len(),
          if result.affected_projects.len() == 1 {
            ""
          } else {
            "s"
          }
        );
      }

      Ok(())
    }
  }
}

/// Write the `--report` file in the requested format.
fn write_report(
  report: &AffectedReport,
  path: &Path,
  format: ReportFormat,
  json: bool,
) -> Result<()> {
  let kind = match format {
    ReportFormat::Html => {
      crate::report::generate_html_report(report, path)?;
      "HTML"
    }
    ReportFormat::Json => {
      crate::report::generate_json_report(report, path)?;
      "JSON"
    }
  };
  if !json {
    eprintln!(
      "{} {}",
      "✓".green(),
      format!("{kind} report generated: {}", path.display()).bold()
    );
  }
  Ok(())
}

fn unix_now() -> i64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_secs() as i64)
    .unwrap_or(0)
}
