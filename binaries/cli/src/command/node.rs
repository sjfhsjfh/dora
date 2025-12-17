use crate::command::Executable;

mod info;
mod list;

/// Manage and inspect dataflow nodes.
#[derive(Debug, clap::Subcommand)]
pub enum Node {
    List(list::List),
    Info(info::Info),
}

impl Executable for Node {
    fn execute(self) -> eyre::Result<()> {
        match self {
            Node::List(cmd) => cmd.execute(),
            Node::Info(cmd) => cmd.execute(),
        }
    }
}
