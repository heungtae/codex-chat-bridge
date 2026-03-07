#[tokio::main]
async fn main() -> anyhow::Result<()> {
    codex_chat_bridge::run().await
}
