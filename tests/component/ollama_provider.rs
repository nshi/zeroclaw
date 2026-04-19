/// Live integration tests for the Ollama provider.
///
/// These tests require a running Ollama instance and are marked `#[ignore]`
/// so they don't run in CI. Run manually with:
///   cargo test --test test_component -- ollama_provider --ignored
#[allow(unused_imports)]
#[ignore]
#[tokio::test]
async fn ollama_round_trip_chat() {
    use mentat::providers::{Provider, create_provider};

    let provider = create_provider("ollama", None).expect("Ollama provider should resolve");
    provider.warmup().await.expect("Ollama should be reachable");

    let response = provider
        .chat_with_system(
            Some("You are a test assistant. Reply with exactly: PONG"),
            "PING",
            "llama3.3",
            0.0,
        )
        .await
        .expect("chat_with_system should succeed");

    assert!(!response.is_empty(), "Response should not be empty");
}
