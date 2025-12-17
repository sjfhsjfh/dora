use clap::Args;
use communication_layer_request_reply::TcpRequestReplyConnection;
use dora_message::{cli_to_coordinator::ControlRequest, coordinator_to_cli::ControlRequestReply};
use eyre::{Context, Ok, bail};

use crate::{
    command::{Executable, topic::selector::DataflowSelector},
    common::CoordinatorOptions,
    formatting::OutputFormat,
};

/// Show information about a dataflow node.
///
/// Examples:
///   dora node info -d my-dataflow node_id
///   dora node info -d my-dataflow --format json node_id
#[derive(Debug, Args)]
#[clap(verbatim_doc_comment)]
pub struct Info {
    #[clap(flatten)]
    selector: DataflowSelector,

    node_id: String,

    /// Output format
    #[clap(long, value_name = "FORMAT", default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,

    #[clap(flatten)]
    coordinator: CoordinatorOptions,
}

impl Executable for Info {
    fn execute(self) -> eyre::Result<()> {
        info(self.coordinator.connect()?, self.node_id)
    }
}

fn info(mut session: Box<TcpRequestReplyConnection>, node: String) -> eyre::Result<()> {
    let reply_raw = session
        .request(&serde_json::to_vec(&ControlRequest::SystemInfo { node }).unwrap())
        .wrap_err("failed to send list message")?;
    let reply: ControlRequestReply =
        serde_json::from_slice(&reply_raw).wrap_err("failed to parse reply")?;
    match reply {
        ControlRequestReply::NodeSystemInfo { node, cpu, memory } => {
            println!("Node: {}", node);
            println!("CPU Usage: {:.2}%", cpu);
            println!("Memory Usage: {} bytes", memory);
        }
        ControlRequestReply::Error(err) => bail!("{err}"),
        other => bail!("unexpected list dataflow reply: {other:?}"),
    };
    Ok(())
}
