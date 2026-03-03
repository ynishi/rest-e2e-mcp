use clap::Parser;

#[derive(Parser)]
#[command(name = "rest-e2e-mcp", version, about = "API verification MCP server")]
struct Cli {
    /// Run as MCP server (stdio transport)
    #[arg(long)]
    mcp: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.mcp {
        rest_e2e_mcp::mcp::run().await
    } else {
        eprintln!("rest-e2e-mcp: use --mcp to start as MCP server");
        eprintln!("CLI mode is not yet implemented.");
        std::process::exit(1);
    }
}
