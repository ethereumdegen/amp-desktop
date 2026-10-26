//! Real opt-in Native/Spice evaluations. `--help` documents the live boundary;
//! ordinary cargo tests contain only offline validator tests and never spawn a
//! NativeClient, access Keychain, launch Chrome, or submit a provider request.
mod adapter;
mod fixtures;
mod judge;
mod png;
mod privacy;
mod scenarios;

use anyhow::{bail, Context, Result};
use native_agent::{default_providers, validate_config, ProviderConfig};
use serde_json::json;
use spice_framework::{suite, Runner, RunnerConfig};
use std::{
    env, fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::{atomic::AtomicUsize, Arc},
    time::Duration,
};

const HELP: &str = "Native Spice: REAL paid/provider-backed evaluations, never mocks.

Usage:
  cargo run -p native-spice -- --live --output /tmp/native-spice-unique
  cargo run -p native-spice -- --live --suite real --workspace ~/ai --output /tmp/native-spice-real-unique
  cargo run -p native-spice -- --live --suite all --output /tmp/native-spice-all-unique

Options:
  --live              REQUIRED explicit permission for real LLM calls and native saved credentials/Keychain
  --output DIR        REQUIRED new, caller-selected output directory; must not exist or be inside the supplied project
  --suite NAME        fixtures (default), real, or all
  --workspace DIR     Real discovery workspace; default $HOME/ai, supplied checkout must be its harbor-tasks child
  --case ID           Run exactly one selected case (no substring ambiguity)
  --timeout SECONDS   Native actor deadline, default 240, range 30..600; shutdown adds at most 30 seconds
  --help              Show this without credentials, browser, or network access

Environment:
  NATIVE_SPICE_PROVIDER       Native provider id (default openai; also anthropic/openrouter)
  NATIVE_SPICE_PROVIDER_CONFIG JSON file containing a native ProviderConfig for another provider/endpoint
  NATIVE_SPICE_MODEL          Actor model (default gpt-6.1-sol)
  NATIVE_SPICE_JUDGE_MODEL    Tool-free native judge model (default actor model)
  OMP_DESKTOP_BROWSER         Explicit installed Chrome/Chromium executable; otherwise common macOS locations

The native provider's existing saved OpenAI OAuth is preferred by the runtime;
API-key env/Keychain configuration uses exactly the native provider's own rules.
No credentials are copied to reports. Missing credentials/browser are failures,
not skips. Every bash/write/edit approval is DENIED, even in the mutation case.
render_preview is static HTML/CSS output only; no app server or URL browsing.
Each case runs once, concurrency=1, no retries/consensus/retry-until-green.
Reports, redacted live events, real tool traces, PNGs, approval decisions and
actual judge responses are retained. Source excerpts may be private: output
root is mode0700. Sensitive real-workspace reads trigger abort + trace omission.
Real source snapshots exclude dependencies/builds/VCS and credential-like paths.
Judge is a real one-shot native provider request with NO tools or filesystem.
Using the same model for actor/judge risks correlated judgments; a passing sample
is not proof that all future outputs are correct. No billed token/cost estimate
is invented. Published spice-framework is pinned to 0.2.0; no sibling checkout
is needed. Benchmark prompts and fixture content are synthetic.
";

struct Options {
    output: PathBuf,
    suite: String,
    workspace: PathBuf,
    case: Option<String>,
    timeout: Duration,
}
fn parse() -> Result<Option<Options>> {
    let mut args = env::args().skip(1);
    let mut live = false;
    let mut output = None;
    let mut suite = "fixtures".to_string();
    let mut workspace = env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is unavailable; supply a configured environment")?
        .join("ai");
    let mut case = None;
    let mut seconds = 240u64;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(None);
            }
            "--live" => live = true,
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().context("--output requires a directory")?,
                ))
            }
            "--suite" => {
                suite = args
                    .next()
                    .context("--suite requires fixtures, real, or all")?
            }
            "--workspace" => {
                workspace = PathBuf::from(args.next().context("--workspace requires a directory")?)
            }
            "--case" => case = Some(args.next().context("--case requires a case id")?),
            "--timeout" => {
                seconds = args
                    .next()
                    .context("--timeout requires seconds")?
                    .parse()
                    .context("--timeout must be an integer")?
            }
            other => bail!("unknown option {other:?}; use --help"),
        }
    }
    if !live {
        bail!("refusing live evaluation without --live; ordinary tests never access real credentials or spend money");
    }
    if !matches!(suite.as_str(), "fixtures" | "real" | "all") {
        bail!("--suite must be fixtures, real, or all");
    }
    if !(30..=600).contains(&seconds) {
        bail!("--timeout must be within 30..600 seconds");
    }
    Ok(Some(Options {
        output: output.context("--output is required; choose a NEW directory outside source")?,
        suite,
        workspace,
        case,
        timeout: Duration::from_secs(seconds),
    }))
}
fn provider() -> Result<ProviderConfig> {
    let config = if let Some(path) = env::var_os("NATIVE_SPICE_PROVIDER_CONFIG") {
        serde_json::from_slice(
            &fs::read(path).context("cannot read NATIVE_SPICE_PROVIDER_CONFIG")?,
        )?
    } else {
        let id = env::var("NATIVE_SPICE_PROVIDER").unwrap_or_else(|_| "openai".into());
        default_providers().into_iter().find(|config| config.id == id)
            .context("unknown NATIVE_SPICE_PROVIDER; use openai/anthropic/openrouter or NATIVE_SPICE_PROVIDER_CONFIG")?
    };
    validate_config(&config)?;
    Ok(config)
}
fn browser() -> Result<PathBuf> {
    let candidates = if let Some(path) = env::var_os("OMP_DESKTOP_BROWSER") {
        vec![PathBuf::from(path)]
    } else {
        let mut paths = vec![
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
        ];
        if let Some(home) = env::var_os("HOME") {
            paths.push(
                PathBuf::from(&home)
                    .join("Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            );
            paths.push(
                PathBuf::from(home).join("Applications/Chromium.app/Contents/MacOS/Chromium"),
            );
        }
        paths
    };
    candidates.into_iter().find(|path| fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0))
        .context("render_preview requires installed Chrome/Chromium; set OMP_DESKTOP_BROWSER to its executable. No fake preview/skip/browser download is permitted")
}
// Resolve an output path before creating it, including existing ancestor
// symlinks. Reject .. rather than accidentally creating a directory in source.
fn destination(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        env::current_dir()?.join(path)
    };
    if absolute
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        bail!("--output may not contain ..");
    }
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        suffix.push(
            ancestor
                .file_name()
                .context("output path has no existing ancestor")?
                .to_owned(),
        );
        ancestor = ancestor
            .parent()
            .context("output path has no existing ancestor")?;
    }
    let mut result = fs::canonicalize(ancestor)?;
    for part in suffix.into_iter().rev() {
        result.push(part);
    }
    Ok(result)
}

fn safe_destination(path: &Path, workspace: &Path) -> Result<PathBuf> {
    let output = destination(path)?;
    // Protect real source even when only temporary fixture cases are selected.
    match fs::canonicalize(workspace.join("harbor-tasks")) {
        Ok(project) if output.starts_with(&project) => {
            bail!("--output must not be inside the supplied project source; no directories were created")
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot resolve protected project source"),
    }
    Ok(output)
}

async fn run(options: Options) -> Result<bool> {
    let real_requested = options.suite != "fixtures";
    let workspace = if real_requested {
        fs::canonicalize(&options.workspace).context("cannot open explicit real workspace")?
    } else {
        options.workspace.clone()
    };
    let project = if real_requested {
        Some(scenarios::real_project(&workspace)?)
    } else {
        None
    };
    let output = safe_destination(&options.output, &workspace)?;
    if output.exists() {
        bail!("--output must be a NEW directory; refusing to overwrite/replay prior evidence");
    }
    let config = provider()?;
    let model = env::var("NATIVE_SPICE_MODEL").unwrap_or_else(|_| "gpt-6.1-sol".into());
    let judge_model = env::var("NATIVE_SPICE_JUDGE_MODEL").unwrap_or_else(|_| model.clone());
    if model.trim().is_empty() || judge_model.trim().is_empty() {
        bail!("actor/judge model may not be empty");
    }
    // Build fixtures only for selected fixture suites; they contain no real
    // credentials and are kept alive until all actor/judge calls have finished.
    let fixtures = if options.suite != "real" {
        Some(fixtures::Fixtures::create()?)
    } else {
        None
    };
    let mut cases = fixtures
        .as_ref()
        .map(|fixtures| scenarios::fixture_cases(fixtures, &output))
        .unwrap_or_default();
    if let Some(project) = &project {
        cases.push(scenarios::real_case(&workspace, project, &output));
    }
    if let Some(filter) = &options.case {
        cases.retain(|case| &case.id == filter);
    }
    if cases.is_empty() {
        bail!("--case did not match a case in the selected suite; no provider request was made");
    }
    let needs_browser = cases.iter().any(|case| {
        matches!(
            case.id.as_str(),
            "discover-plan-preview" | "real-harbor-tasks-readonly"
        )
    });
    let browser_path = if needs_browser {
        Some(browser()?)
    } else {
        None
    };
    // Parent must already exist. A single new mode0700 root contains all private
    // traces/artifacts; no silent fallback to the cwd or to an existing report.
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&output)
        .context("cannot create new output directory (its parent must exist)")?;
    fs::create_dir(output.join("traces"))?;
    fs::write(
        output.join("run.json"),
        serde_json::to_vec_pretty(&json!({
            "live":true, "suite":options.suite, "cases":cases.iter().map(|case|&case.id).collect::<Vec<_>>(),
            "provider":{"id":config.id,"kind":config.kind,"base_url":config.base_url},
            "actor_model":model,"judge_model":judge_model,"same_model_correlation_risk":model == judge_model,
            "concurrency":1,"retries":0,"consensus_runs":1,"actor_timeout_seconds":options.timeout.as_secs(),
            "judge_timeout_seconds":90,"approval_policy":"deny every request, no bash/write/edit exceptions",
            "browser":browser_path,"real_prompt":if real_requested { Some(scenarios::DESIGN_PROMPT) } else { None },
            "limitations":["One actual sample per case; not a proof about all possible model outputs", "Judge scores may correlate with actor errors", "No invented billed token/cost metrics", "Real discovery sibling names and sensitive paths/bodies are redacted; sensitive reads abort", "Source snapshots exclude generated/VCS/dependency and credential-like paths"],
            "spice_framework":"=0.2.0 (published; no optional OpenAI/reqwest feature)"
        }))?,
    )?;
    let agent = Arc::new(adapter::NativeAdapter {
        provider: config.clone(),
        model,
        output_dir: output.clone(),
        timeout: options.timeout,
    });
    let judge = Arc::new(judge::NativeJudge {
        provider: config,
        model: judge_model,
        timeout: Duration::from_secs(90),
        output_dir: output.clone(),
        calls: AtomicUsize::new(0),
    });
    let runner = Runner::new(RunnerConfig {
        concurrency: 1,
        // Adapter owns its shorter timeout and awaits real shutdown; the outer
        // Runner deadline is only a backstop, never a retry trigger.
        default_timeout: options.timeout + Duration::from_secs(30),
        trace_dir: Some(output.join("traces")),
        report_path: Some(output.join("report.json")),
        ..Default::default()
    })
    .with_judge(judge);
    let report = runner
        .run(
            suite("Native grounded planning and real static previews", cases),
            agent,
        )
        .await;
    // Spice logs report/trace write errors without failing the run; the CLI must
    // not silently claim evidence exists. Save once more with propagated errors.
    report.save_to_file(&output.join("report.json"))?;
    for test in &report.tests {
        if test.error.is_none()
            && !output
                .join("traces")
                .join(format!("{}_attempt0.json", test.test_id))
                .is_file()
        {
            bail!("Spice failed to retain actual trace for {}", test.test_id);
        }
    }
    println!("Evidence: {}", output.display());
    println!("Real provider/judge calls: no retries, deny-all approvals. Same-model judge correlation and single-sample limits apply.");
    Ok(report.failed == 0 && report.total > 0)
}

#[tokio::main]
async fn main() {
    let result = match parse() {
        Ok(None) => return,
        Ok(Some(options)) => run(options).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("Native Spice failed: {error:#}\nUse --help for explicit live prerequisites; missing credentials/browser are failures, never skips.");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_destination_rejects_project_and_symlink_alias_without_writes() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("harbor-tasks");
        fs::create_dir(&project).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&project, &alias).unwrap();
        for path in [project.join("evidence"), alias.join("evidence")] {
            assert!(safe_destination(&path, root.path()).is_err());
        }
        assert!(!project.join("evidence").exists());
        let outside = root.path().join("outside");
        assert_eq!(
            safe_destination(&outside, root.path()).unwrap(),
            destination(&outside).unwrap()
        );
        assert!(!outside.exists());
    }
}
