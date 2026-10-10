#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(orchestrator::resume_build::main_with(&args).await);
}
