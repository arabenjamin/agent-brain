use std::sync::Once;

static INIT: Once = Once::new();

/// Initialize test environment (logging, etc.)
pub fn init_test_env() {
    INIT.call_once(|| {
        // Set up test logging
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::DEBUG)
            .try_init();
    });
}

/// Get test Neo4j configuration from environment
pub fn neo4j_test_config() -> (String, String, String) {
    let uri = std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://localhost:7687".to_string());
    let user = std::env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".to_string());
    let password = std::env::var("NEO4J_PASSWORD").unwrap_or_else(|_| "testpassword".to_string());
    (uri, user, password)
}
