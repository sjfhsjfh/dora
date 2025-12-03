use std::io::Write;

use clap::Args;
use serde::Serialize;
use tabwriter::TabWriter;

use crate::{
    command::{default_tracing, Executable, topic::selector::DataflowSelector},
    common::CoordinatorOptions,
    formatting::OutputFormat,
};

/// List nodes of a running dataflow.
///
/// Examples:
///   dora node list -d my-dataflow
///   dora node list -d my-dataflow --format json
#[derive(Debug, Args)]
#[clap(verbatim_doc_comment)]
pub struct List {
    #[clap(flatten)]
    selector: DataflowSelector,

    /// Output format
    #[clap(long, value_name = "FORMAT", default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,

    #[clap(flatten)]
    coordinator: CoordinatorOptions,
}

impl Executable for List {
    fn execute(self) -> eyre::Result<()> {
        default_tracing()?;

        list(self.coordinator, self.selector, self.format)
    }
}

#[derive(Serialize)]
struct OutputEntry {
    node: String,
    status: String,
    pid: Option<u32>,
    cpu: Option<String>,
    memory: Option<String>,
}

fn list(
    coordinator: CoordinatorOptions,
    selector: DataflowSelector,
    format: OutputFormat,
) -> eyre::Result<()> {
    // NOTE: Coordinator currently exposes no API for per-node PID/CPU/MEM.
    // We therefore report node presence (Running) from descriptor only and
    // leave resource fields empty for now.
    let mut session = coordinator.connect()?;
    let (_dataflow_id, descriptor) = selector.resolve(session.as_mut())?;

    let entries: Vec<OutputEntry> = descriptor
        .nodes
        .into_iter()
        .map(|n| OutputEntry {
            node: n.id.to_string(),
            status: "Running".to_string(),
            pid: None,
            cpu: None,
            memory: None,
        })
        .collect();

    match format {
        OutputFormat::Table => {
            let mut tw = TabWriter::new(std::io::stdout().lock());
            tw.write_all(b"NODE\tSTATUS\tPID\tCPU\tMEMORY\n")?;
            for e in entries {
                tw.write_all(
                    format!(
                        "{}\t{}\t{}\t{}\t{}\n",
                        e.node,
                        e.status,
                        e.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                        e.cpu.unwrap_or_else(|| "-".into()),
                        e.memory.unwrap_or_else(|| "-".into()),
                    )
                    .as_bytes(),
                )?;
            }
            tw.flush()?;
        }
        OutputFormat::Json => {
            for e in entries {
                println!("{}", serde_json::to_string(&e)?);
            }
        }
    }

    Ok(())
}

