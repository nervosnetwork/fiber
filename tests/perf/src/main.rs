use fiber_e2e_tests::run_integration_test;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match run_integration_test().await {
        Ok(_) => println!("All integration tests passed!"),
        Err(e) => {
            eprintln!("Integration test failed: {}", e);
            std::process::exit(1);
        }
    }

    Ok(())
}
