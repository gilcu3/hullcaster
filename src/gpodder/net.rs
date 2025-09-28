use anyhow::{anyhow, Result};
use ureq::Agent;

pub fn execute_request_post(
    agent: &Agent, url: String, body: String, encoded_credentials: &String, max_retries: usize,
) -> Result<String> {
    let mut max_retries = max_retries;

    let request = loop {
        let response = agent
            .post(&url)
            .header("Authorization", &format!("Basic {encoded_credentials}"))
            .send(&body);

        match response {
            Ok(resp) => {
                break Ok(resp);
            }
            Err(ureq::Error::StatusCode(code)) => {
                // Handle HTTP error statuses (e.g., 404, 500)
                println!("Error code: {code}");
                max_retries -= 1;
                if max_retries == 0 {
                    break Err(anyhow!("Error code: {code}"));
                }
            }
            Err(_) => {
                max_retries -= 1;
                if max_retries == 0 {
                    break Err(anyhow!("Max retries exceeded"));
                }
            }
        }
    }?;
    Ok(request.into_body().read_to_string()?)
}

pub fn execute_request_get(
    agent: &Agent, url: String, params: Vec<(&str, &str)>, encoded_credentials: &String,
    max_retries: usize,
) -> Result<String> {
    let mut max_retries = max_retries;

    let request = loop {
        let response = agent
            .get(&url)
            .header("Authorization", &format!("Basic {encoded_credentials}"))
            .query_pairs(params.clone())
            .call();

        match response {
            Ok(resp) => {
                break Ok(resp);
            }
            Err(ureq::Error::StatusCode(code)) => {
                max_retries -= 1;
                if max_retries == 0 {
                    break Err(anyhow!("Error code: {code}"));
                }
            }
            Err(_) => {
                max_retries -= 1;
                if max_retries == 0 {
                    break Err(anyhow!("Max retries exceeded"));
                }
            }
        }
    }?;
    Ok(request.into_body().read_to_string()?)
}
