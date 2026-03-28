use clap::{Parser, Subcommand, ValueEnum};

/// Autonomous Agent Brain — MCP Server
#[derive(Debug, Parser)]
#[command(name = "agent-brain")]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Neo4j connection URI
    #[arg(long, env = "NEO4J_URI", default_value = "bolt://localhost:7687")]
    pub neo4j_uri: String,

    /// Neo4j username
    #[arg(long, env = "NEO4J_USER", default_value = "neo4j")]
    pub neo4j_user: String,

    /// Neo4j password
    #[arg(long, env = "NEO4J_PASSWORD")]
    pub neo4j_password: Option<String>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Log format (pretty, json)
    #[arg(long, env = "LOG_FORMAT", default_value = "pretty")]
    pub log_format: String,
}

/// Transport type for the MCP server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum TransportType {
    /// Standard input/output transport (default, for local CLI usage)
    #[default]
    Stdio,
    /// HTTP transport with SSE (for remote/cloud deployment)
    Http,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run as MCP server
    Serve {
        /// Transport type to use
        #[arg(long, env = "MCP_TRANSPORT", default_value = "stdio")]
        transport: TransportType,

        /// HTTP bind address (only used with --transport http)
        #[arg(long, env = "MCP_HTTP_BIND", default_value = "127.0.0.1:3000")]
        bind: String,

        /// API key for authentication (only used with --transport http)
        #[arg(long, env = "MCP_API_KEY")]
        api_key: Option<String>,
    },

    /// Initialize the Neo4j database schema
    InitDb,
}

impl Cli {
    pub fn parse_args() -> Self {
        Cli::parse()
    }
}
