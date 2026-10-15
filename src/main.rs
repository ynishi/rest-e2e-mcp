use clap::Parser;

#[derive(Parser)]
#[command(name = "rest-e2e-mcp", version, about = "API verification MCP server")]
struct Cli {
    /// Run as MCP server (stdio transport)
    #[arg(long)]
    mcp: bool,

    /// Allow resolving secret references matching PATTERN (repeatable).
    /// PATTERN is an exact reference or uses `*` as a wildcard, e.g.
    /// 'keychain://my-api-tests/*' or 'op://Dev/*'. References not matching
    /// any pattern are never resolved, whatever their source.
    #[arg(long = "secret-ref-allow", value_name = "PATTERN")]
    secret_ref_allow: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.mcp {
        rest_e2e_mcp::mcp::run(cli.secret_ref_allow).await
    } else {
        eprintln!("rest-e2e-mcp: use --mcp to start as MCP server");
        eprintln!("CLI mode is not yet implemented.");
        std::process::exit(1);
    }
}
