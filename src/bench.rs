use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::agent::{Agent, RunOptions, RunResult};
use crate::cache::ResponseCache;
use crate::config::Config;
use crate::model::create_model;
use crate::ui::TerminalUi;

#[derive(Clone, Debug)]
pub struct BenchOptions {
    pub manifest: PathBuf,
    pub output: PathBuf,
    pub default_workspace: PathBuf,
    pub config: Config,
    pub keep_going: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BenchTask {
    pub id: String,
    pub task: String,
    pub workspace: Option<PathBuf>,
    pub verify: Option<String>,
    pub max_steps: Option<usize>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum BenchRecord {
    Completed {
        id: String,
        #[serde(flatten)]
        result: RunResult,
    },
    Error {
        id: String,
        error: String,
    },
}

pub async fn run_manifest(options: BenchOptions) -> Result<()> {
    let contents = tokio::fs::read_to_string(&options.manifest)
        .await
        .with_context(|| format!("failed to read manifest {}", options.manifest.display()))?;
    let tasks = parse_manifest(&contents)?;
    if let Some(parent) = options.output.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut output = tokio::fs::File::create(&options.output)
        .await
        .with_context(|| format!("failed to create {}", options.output.display()))?;
    let manifest_dir = options.manifest.parent().unwrap_or_else(|| Path::new("."));

    for task in tasks {
        let workspace = resolve_workspace(
            task.workspace.as_deref(),
            manifest_dir,
            &options.default_workspace,
        )?;
        let mut config = options.config.clone();
        if let Some(max_steps) = task.max_steps {
            config.agent.max_steps = max_steps;
        }
        let cache = ResponseCache::new(config.cache.clone())?;
        let model = create_model(&config.model, config.api_key()?, cache)?;
        let mut agent = Agent::new(config, model, Box::new(TerminalUi::benchmark()), workspace);
        let record = match agent
            .run(
                &task.task,
                RunOptions {
                    verify: task.verify,
                    task_id: Some(task.id.clone()),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(result) => BenchRecord::Completed {
                id: task.id,
                result,
            },
            Err(error) => {
                let record = BenchRecord::Error {
                    id: task.id,
                    error: format!("{error:#}"),
                };
                write_record(&mut output, &record).await?;
                if !options.keep_going {
                    anyhow::bail!(
                        "benchmark task failed; partial results are in {}",
                        options.output.display()
                    );
                }
                continue;
            }
        };
        write_record(&mut output, &record).await?;
    }
    output.flush().await?;
    Ok(())
}

fn parse_manifest(contents: &str) -> Result<Vec<BenchTask>> {
    contents
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("invalid JSONL manifest record at line {}", index + 1))
        })
        .collect()
}

fn resolve_workspace(
    task_workspace: Option<&Path>,
    manifest_dir: &Path,
    default_workspace: &Path,
) -> Result<PathBuf> {
    let path = match task_workspace {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => manifest_dir.join(path),
        None => default_workspace.to_path_buf(),
    };
    path.canonicalize()
        .with_context(|| format!("task workspace {} does not exist", path.display()))
}

async fn write_record(file: &mut tokio::fs::File, record: &BenchRecord) -> Result<()> {
    file.write_all(&serde_json::to_vec(record)?).await?;
    file.write_all(b"\n").await?;
    file.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonl_manifest() {
        let tasks = parse_manifest(
            r#"{"id":"one","task":"fix it"}
{"id":"two","task":"test it","max_steps":12}"#,
        )
        .unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].max_steps, Some(12));
    }
}
