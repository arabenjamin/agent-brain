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

    /// Repair notes that stored a query envelope instead of its content.
    ///
    /// One-off migration for notes written before the `extract_result_text`
    /// unwrap fix (2026-08-10). Rewrites content in place and rebuilds the
    /// embedding, similarity edges, chunks, and entity mentions. Idempotent.
    /// Run with the scheduler paused.
    RepairNotes {
        /// Report what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Fold entities that are one real thing into one node.
    ///
    /// Applies the curated seed (`entities/*.yaml`, ENTITY_ALIASES_DIR) and
    /// merges entities whose names differ only by case, punctuation, spacing
    /// or a leading title. A snapshot is taken before any merge; every merge
    /// leaves an EntityAlias and can be undone. `init-db` runs this on every
    /// start; run it by hand with `--dry-run` to see what it would do.
    ResolveEntities {
        /// Report the planned merges without writing anything.
        #[arg(long)]
        dry_run: bool,

        /// Undo one merge instead: bring this alias back as its own entity.
        #[arg(long, value_name = "ALIAS")]
        unmerge: Option<String>,
    },

    /// Rewrite stored claims so each stands alone (HBI plan Phase 9).
    ///
    /// Each source note's claims are rewritten in one call that sees the
    /// source; a rewrite replaces a claim only if it names what it is about
    /// and lost no number or name, else the claim is kept and marked
    /// `ambiguous` with the reason. `--dry-run` samples `--sample` claims
    /// across writers and writes nothing: read its report first. A real run
    /// takes a snapshot, keeps old text in `content_v1`, and resumes where a
    /// stopped run left off.
    RewriteClaims {
        /// Ask the model and report, writing nothing.
        #[arg(long)]
        dry_run: bool,

        /// Dry run: how many claims to sample.
        #[arg(long, default_value = "50")]
        sample: usize,

        /// Stop after this many source notes.
        #[arg(long)]
        limit_sources: Option<usize>,

        /// Only this source note (repeatable): a dry run on a chosen source.
        #[arg(long = "source-id")]
        source_ids: Vec<String>,

        /// Write the before/after report (markdown) here instead of stdout.
        #[arg(long)]
        report: Option<String>,
    },

    /// Restore a knowledge-graph snapshot (`.json.gz` from KNOWLEDGE_SNAPSHOT_DIR).
    ///
    /// MERGE semantics: safe on a non-empty graph; existing nodes are updated,
    /// nothing is deleted. `--dry-run` reports what the file holds.
    RestoreSnapshot {
        /// Path to the snapshot file.
        path: String,

        /// Count what would be restored; write nothing.
        #[arg(long)]
        dry_run: bool,
    },

    /// Score retrieval quality against a golden set (read-only).
    ///
    /// Runs each (query → expected note) case in the fixture through the real
    /// retrieval pipeline — non-perturbing, so it never bumps access counts —
    /// and reports recall@k and MRR. This is the number you watch move when you
    /// change a freshness weight, the RRF `k`, or the embedding model.
    ///
    /// `--bootstrap N` instead samples N notes from the graph and prints
    /// proposed cases to curate; redirect to a file, rewrite the queries into
    /// real questions, and append to the fixture.
    EvalRetrieval {
        /// Path to the golden-set YAML fixture.
        #[arg(long, default_value = "eval/retrieval_golden.yaml")]
        fixture: String,

        /// Top-k cutoff for both the search limit and the recall/MRR window.
        #[arg(long, default_value = "10")]
        k: usize,

        /// Instead of scoring, sample this many notes and print proposed cases.
        #[arg(long)]
        bootstrap: Option<usize>,
    },

    /// Manage todo items (requires server to be running)
    Todo {
        #[command(subcommand)]
        action: TodoAction,

        /// Agent Brain server URL
        #[arg(long, env = "BRAIN_URL", default_value = "http://localhost:3000")]
        url: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum TodoAction {
    /// Add a new todo
    Add {
        /// Title of the todo
        title: String,
        /// Optional description
        #[arg(long)]
        description: Option<String>,
        /// Priority: 0=urgent 1=high 2=normal 3=low
        #[arg(long, default_value = "2")]
        priority: i64,
        /// Due date (ISO-8601, e.g. 2026-05-01)
        #[arg(long)]
        due: Option<String>,
        /// Comma-separated tags
        #[arg(long)]
        tags: Option<String>,
    },
    /// List todos
    List {
        /// Filter by status: pending, in_progress, done
        #[arg(long)]
        status: Option<String>,
    },
    /// Mark a todo as done
    Done {
        /// Todo ID
        id: String,
    },
    /// Update a todo's status
    Status {
        /// Todo ID
        id: String,
        /// New status: pending, in_progress, done
        status: String,
    },
    /// Delete a todo
    Delete {
        /// Todo ID
        id: String,
    },
}

impl Cli {
    pub fn parse_args() -> Self {
        Cli::parse()
    }
}
