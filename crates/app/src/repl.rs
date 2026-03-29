//! Interactive terminal REPL backed by ChatService.

use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};
use uuid::Uuid;

use crate::services::chat::ChatEvent;
use crate::services::{
    ChatService,
    chat::{ChatHistoryMessage, ChatRequest},
};

/// Start the interactive REPL.
///
/// `chat` must be pre-built (call `server.initialize().await` first).
/// `llm_desc` and `neo4j_uri` are shown in the welcome banner.
pub async fn run(
    chat: Arc<ChatService>,
    llm_desc: &str,
    neo4j_uri: &str,
    initial_profile: Option<String>,
    initial_session: Option<String>,
) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");

    println!();
    println!("Agent Brain v{version}");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("LLM:       {llm_desc}");
    println!("Connected: {neo4j_uri}");
    println!();
    println!("Type anything to interact. /help for commands, /quit to exit.");

    let mut history: Vec<ChatHistoryMessage> = Vec::new();
    let mut session_id = initial_session.unwrap_or_else(|| Uuid::new_v4().to_string());
    let mut current_profile = initial_profile;

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        print!("\n> ");
        // flush stdout so the prompt appears before we block on input
        use std::io::Write as _;
        let _ = std::io::stdout().flush();

        line.clear();

        let read = tokio::select! {
            r = reader.read_line(&mut line) => r,
            _ = tokio::signal::ctrl_c() => {
                println!("\nGoodbye.");
                break;
            }
        };

        match read {
            Ok(0) => {
                // EOF
                println!("\nGoodbye.");
                break;
            }
            Err(e) => {
                eprintln!("\x1b[31m[error] reading input: {e}\x1b[0m");
                break;
            }
            Ok(_) => {}
        }

        let input = line.trim().to_string();
        if input.is_empty() {
            continue;
        }

        // Handle meta-commands
        if input.starts_with('/') {
            let parts: Vec<&str> = input
                .strip_prefix('/')
                .unwrap_or("")
                .splitn(2, ' ')
                .collect();
            match parts[0] {
                "quit" | "exit" => {
                    println!("Goodbye.");
                    break;
                }
                "clear" => {
                    history.clear();
                    println!("History cleared.");
                    continue;
                }
                "new" => {
                    history.clear();
                    session_id = Uuid::new_v4().to_string();
                    println!("New session started: {session_id}");
                    continue;
                }
                "profile" => {
                    if let Some(name) = parts.get(1) {
                        current_profile = Some(name.to_string());
                        println!("Profile set to: {name}");
                    } else {
                        println!("Usage: /profile <name>  (e.g. knowledge-worker, task-manager)");
                    }
                    continue;
                }
                "status" => {
                    let profile_str = current_profile.as_deref().unwrap_or("none");
                    println!(
                        "Session: {session_id} | History: {} turns | Profile: {profile_str}",
                        history.len() / 2
                    );
                    continue;
                }
                "help" => {
                    print_help();
                    continue;
                }
                other => {
                    println!("Unknown command: /{other}. Type /help for available commands.");
                    continue;
                }
            }
        }

        // Regular message — send to ChatService
        history.push(ChatHistoryMessage {
            role: "user".to_string(),
            content: input.clone(),
        });

        let req = ChatRequest {
            message: input.clone(),
            history: history[..history.len().saturating_sub(1)].to_vec(),
            session_id: Some(session_id.clone()),
            tools: None,
            context_profile: current_profile.clone(),
            synthesis_provider: None,
            synthesis_model: None,
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel::<ChatEvent>(32);
        let chat_clone = Arc::clone(&chat);
        tokio::spawn(async move {
            chat_clone.run(req, tx).await;
        });

        let mut response_content = String::new();
        let mut first_thinking = true;

        while let Some(event) = rx.recv().await {
            match event {
                ChatEvent::Thinking { content } => {
                    if first_thinking {
                        print!("\x1b[2m");
                        first_thinking = false;
                    }
                    print!("{content}");
                    let _ = std::io::stdout().flush();
                }
                ChatEvent::ToolCall { tool, args } => {
                    if !first_thinking {
                        println!("\x1b[0m"); // end dim
                        first_thinking = true;
                    }
                    let args_preview = if args.is_null()
                        || args == serde_json::Value::Object(Default::default())
                    {
                        String::new()
                    } else {
                        let s = args.to_string();
                        if s.len() > 80 {
                            format!("{}…", &s[..80])
                        } else {
                            s
                        }
                    };
                    println!("\x1b[36m⚡  {tool}\x1b[0m {args_preview}");
                }
                ChatEvent::ToolResult {
                    tool,
                    success,
                    preview,
                } => {
                    let icon = if success {
                        "\x1b[32m✓\x1b[0m"
                    } else {
                        "\x1b[31m✗\x1b[0m"
                    };
                    let preview_trimmed = if preview.len() > 120 {
                        format!("{}…", &preview[..120])
                    } else {
                        preview
                    };
                    println!("   {icon} {tool}: {preview_trimmed}");
                }
                ChatEvent::Token { content } => {
                    if !first_thinking {
                        print!("\x1b[0m");
                        first_thinking = true;
                    }
                    print!("{content}");
                    let _ = std::io::stdout().flush();
                    response_content.push_str(&content);
                }
                ChatEvent::Message { content } => {
                    if !first_thinking {
                        println!("\x1b[0m");
                        first_thinking = true;
                    }
                    println!("\n\x1b[1mAgent Brain:\x1b[0m {content}");
                    response_content = content;
                }
                ChatEvent::Error { message } => {
                    println!("\x1b[31m[error] {message}\x1b[0m");
                }
                ChatEvent::Done => {
                    println!();
                    break;
                }
            }
        }

        if !response_content.is_empty() {
            history.push(ChatHistoryMessage {
                role: "assistant".to_string(),
                content: response_content,
            });
        }
    }

    Ok(())
}

fn print_help() {
    println!();
    println!("Meta-commands:");
    println!("  /quit, /exit   — Exit the REPL");
    println!("  /clear         — Clear conversation history");
    println!("  /new           — Start a new session (new ID, cleared history)");
    println!("  /profile <n>   — Set context profile (e.g. knowledge-worker)");
    println!("  /status        — Show session ID, history length, active profile");
    println!("  /help          — Show this message");
    println!();
    println!("All 90 brain tools are available. Just describe what you want to do.");
    println!("Examples:");
    println!("  Store a note about the project architecture");
    println!("  Search my notes for anything about authentication");
    println!("  Create a task to implement dark mode");
    println!("  What do I know about Neo4j indexing strategies?");
    println!();
}
