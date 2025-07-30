use anyhow::Context;
use chrono::{DateTime, FixedOffset};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Client;
use serde::Deserialize;
use tracing::{debug, error};

#[derive(Deserialize)]
struct GithubCommitInfo {
    sha: String,
}

#[derive(Deserialize)]
struct GithubBranchInfo {
    name: String,
    commit: GithubCommitInfo,
}

pub enum ApiBranchResponse {
    NotModified,
    Ok {
        last_modified: Option<DateTime<FixedOffset>>,
        sha: String,
        etag: Option<String>,
    },
}

pub async fn fetch_latest_rev(
    owner: &str,
    name: &str,
    branch: &str,
    last_modified: Option<DateTime<FixedOffset>>,
    etag: Option<String>,
) -> anyhow::Result<ApiBranchResponse> {
    let client = Client::builder()
        .build()
        .expect("could not build request client");

    let url = format!(
        "https://api.github.com/repos/{}/{}/branches/{}",
        owner, name, branch
    );

    let mut headers = HeaderMap::new();
    headers.append(
        "Accept",
        HeaderValue::from_str("application/json").expect("valid string"),
    );
    headers.append(
        "User-Agent",
        HeaderValue::from_str("fc-search").expect("valid string"),
    );

    if let Some(last_mod) = last_modified {
        headers.append(
            "If-Modified-Since",
            HeaderValue::from_str(&last_mod.to_rfc2822()).expect("rfc2822 should be valid string"),
        );
    }

    if let Some(etag) = etag {
        headers.append(
            "if-none-match",
            HeaderValue::from_str(&etag).expect("etag should be valid string"),
        );
    }

    let response = client
        .get(url)
        .headers(headers)
        .send()
        .await
        .context("unable to fetch repository info")?;

    // return early on "not modified"
    if response.status().as_u16() == 304 {
        return Ok(ApiBranchResponse::NotModified);
    }

    anyhow::ensure!(
        response.status().is_success(),
        "response from github was not successful: {}",
        response
            .status()
            .canonical_reason()
            .unwrap_or("(no canonical reason)")
    );

    let etag = response.headers().get("etag");

    let last_modified = response.headers().get("last-modified").and_then(|x| {
        let content = x
            .to_str()
            .expect("last-modified header value should be a string");
        match chrono::DateTime::parse_from_rfc2822(content) {
            Ok(t) => Some(t),
            Err(e) => {
                error!(
                    "failed to parse the given last-modified header with rfc2822: '{}' / '{}'",
                    content, e
                );
                None
            }
        }
    });

    let response_text = response
        .text()
        .await
        .context("expected to get text for api response from github")?;

    let ghinfo: GithubBranchInfo = match serde_json::from_str(&response_text) {
        Ok(s) => s,
        Err(e) => {
            error!(
                "did not get json in the expected format from the github api '{}' '{}'",
                response_text, e
            );
            anyhow::bail!("invalid json");
        }
    };

    anyhow::ensure!(
        ghinfo.name.eq(branch),
        "got an api response for branch '{}' when it was requested for branch '{}'",
        ghinfo.name,
        branch
    );
    debug!("latest rev is '{}'", ghinfo.commit.sha);

    Ok(ApiBranchResponse::Ok {
        last_modified,
        sha: ghinfo.commit.sha,
    })
}
