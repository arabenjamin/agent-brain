use clap::{Parser, Subcommand};

/// Autonomous API Knowledge Graph - MCP Server
#[derive(Debug, Parser)]
#[command(name = "agent-api")]
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

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run as MCP server (stdio transport) - default mode
    Serve,

    /// Initialize the Neo4j database schema
    InitDb,

    /// Ingest an OpenAPI specification
    Ingest {
        /// Path or URL to OpenAPI spec (JSON or YAML)
        #[arg(value_name = "SPEC")]
        spec: String,
    },

    /// Query endpoints in the knowledge graph
    Query {
        /// Natural language query
        #[arg(value_name = "QUERY")]
        query: String,
    },

    /// Execute an HTTP request against an endpoint
    Execute {
        /// HTTP method (GET, POST, etc.)
        #[arg(short, long)]
        method: String,

        /// Target URL
        #[arg(value_name = "URL")]
        url: String,

        /// Request body (JSON)
        #[arg(short, long)]
        body: Option<String>,

        /// Headers in "Key: Value" format (can be repeated)
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
    },

    /// Show database statistics
    Stats,
}

impl Cli {
    pub fn parse_args() -> Self {
        Cli::parse()
    }
}
