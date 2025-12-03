use crate::command::Executable;

mod list;

/// Manage and inspect dataflow nodes.
#[derive(Debug, clap::Subcommand)]
pub enum Node {
    List(list::List),
}

impl Executable for Node {
    fn execute(self) -> eyre::Result<()> {
        match self {
            Node::List(cmd) => cmd.execute(),
        }
    }
}
