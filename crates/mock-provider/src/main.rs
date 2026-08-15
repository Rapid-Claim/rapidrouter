//! Run the mock provider standalone for manual gateway testing.

#[tokio::main]
async fn main() {
    let mock = mock_provider::MockProvider::spawn().await;
    println!("mock provider listening on {}", mock.base_url());
    std::future::pending::<()>().await;
}
