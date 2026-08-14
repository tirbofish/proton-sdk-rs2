use reqwest_middleware::ClientWithMiddleware;

pub fn create_client_with_timeout(timeout_seconds: f64) -> ClientWithMiddleware {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs_f64(timeout_seconds))
        .build()
        .expect("failed to build HTTP client");

    reqwest_middleware::ClientBuilder::new(client).build()
}
