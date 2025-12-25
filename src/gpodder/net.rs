use anyhow::{Result, anyhow};

pub async fn execute_request_post(
    client: &reqwest::Client, url: String, body: String, encoded_credentials: &String,
    max_retries: usize,
) -> Result<String> {
    let mut max_retries = max_retries;
    log::debug!("execute_request_get: {url} {body:?}");

    let response = loop {
        let response = client
            .post(&url)
            .header("Authorization", &format!("Basic {encoded_credentials}"))
            .body(body.clone())
            .send()
            .await;

        match response {
            Ok(resp) => {
                if resp.status().is_success() {
                    break Ok(resp);
                } else {
                    max_retries -= 1;
                    if max_retries == 0 {
                        let status_code = resp.status();
                        break Err(anyhow!("Error code: {status_code}"));
                    }
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
    Ok(response.text().await?)
}

pub async fn execute_request_get(
    client: &reqwest::Client, url: String, params: Vec<(&str, &str)>, encoded_credentials: &String,
    max_retries: usize,
) -> Result<String> {
    let mut max_retries = max_retries;
    log::debug!("execute_request_get: {url} {params:?}");

    let response = loop {
        let response = client
            .get(&url)
            .header("Authorization", &format!("Basic {encoded_credentials}"))
            .query(&params)
            .send()
            .await;

        match response {
            Ok(resp) => {
                if resp.status().is_success() {
                    break Ok(resp);
                } else {
                    max_retries -= 1;
                    if max_retries == 0 {
                        let status_code = resp.status();
                        break Err(anyhow!("Error code: {status_code}"));
                    }
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
    Ok(response.text().await?)
}
