use chrono::{DateTime, Utc};
use std::process::Command;
use ureq::{Agent, Error, Response};

/// Helper function converting an (optional) Unix timestamp to a
/// DateTime<Utc> object
pub fn convert_date(result: Result<i64, rusqlite::Error>) -> Option<DateTime<Utc>> {
    match result {
        Ok(timestamp) => DateTime::from_timestamp(timestamp, 0)
            .map(|ndt| DateTime::from_naive_utc_and_offset(ndt.naive_utc(), Utc)),
        Err(_) => None,
    }
}

pub fn evaluate_in_shell(value: &str) -> Option<String> {
    let res = Command::new("sh").arg("-c").arg(value).output();
    if let Ok(res) = res {
        Some(String::from_utf8_lossy(&res.stdout).to_string())
    } else {
        None
    }
}

pub fn execute_request_post(
    agent: &Agent, url: String, body: String, encoded_credentials: &String,
) -> Option<String> {
    let mut max_retries = 3;

    let request: Result<Response, ()> = loop {
        let response = agent
            .post(&url)
            .set("Authorization", &format!("Basic {}", encoded_credentials))
            .send_string(&body);

        match response {
            Ok(resp) => {
                //println!("Ok code: {:?}", resp);
                break Ok(resp);
            }
            Err(Error::Status(code, _error_response)) => {
                // Handle HTTP error statuses (e.g., 404, 500)
                println!("Error code: {}", code);
                max_retries -= 1;
                if max_retries == 0 {
                    break Err(());
                }
            }
            Err(_) => {
                max_retries -= 1;
                if max_retries == 0 {
                    break Err(());
                }
            }
        }
    };
    if let Ok(req) = request {
        req.into_string().ok()
    } else {
        None
    }
}

pub fn execute_request_get(
    agent: &Agent, url: String, params: Vec<(&str, &str)>, encoded_credentials: &String,
) -> Option<String> {
    let mut max_retries = 3;

    let request: Result<Response, ()> = loop {
        let response = agent
            .get(&url)
            .set("Authorization", &format!("Basic {}", encoded_credentials))
            .query_pairs(params.clone())
            .call();

        match response {
            Ok(resp) => {
                // println!("Ok code: {:?}", resp);
                break Ok(resp);
            }
            Err(Error::Status(code, _error_response)) => {
                println!("Error code: {}", code);
                max_retries -= 1;
                if max_retries == 0 {
                    break Err(());
                }
            }
            Err(_) => {
                max_retries -= 1;
                if max_retries == 0 {
                    break Err(());
                }
            }
        }
    };
    if let Ok(req) = request {
        req.into_string().ok()
    } else {
        None
    }
}
