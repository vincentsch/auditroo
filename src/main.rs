#[tokio::main]
async fn main() {
    std::process::exit(auditroo::cli::run().await);
}
