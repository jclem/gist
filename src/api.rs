use serde::{Deserialize, Serialize};

use crate::error::CliError;

const BASE_URL: &str = "https://api.github.com";

pub fn new_client() -> reqwest::Client {
    let version = env!("CARGO_PKG_VERSION");
    reqwest::Client::builder()
        .user_agent(format!("gist-cli-{version}"))
        .build()
        .expect("failed to build HTTP client")
}

fn resolve_token() -> Result<String, CliError> {
    if let Ok(output) = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
    {
        if output.status.success() {
            let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !token.is_empty() {
                return Ok(token);
            }
        }
    }

    std::env::var("GH_GIST_TOKEN")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            CliError::auth_with_hint(
                "no GitHub token found",
                "authenticate with `gh auth login` or set GH_GIST_TOKEN",
            )
        })
}

async fn check_status(resp: reqwest::Response, context: &str) -> Result<reqwest::Response, CliError> {
    if resp.status().is_success() {
        return Ok(resp);
    }

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED
        || resp.status() == reqwest::StatusCode::FORBIDDEN
    {
        return Err(CliError::auth_with_hint(
            format!("{context}: unauthorized"),
            "check that your `gh` token or GH_GIST_TOKEN is valid and has the gist scope",
        ));
    }

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(CliError::api(format!("{context}: not found")));
    }

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("message")?.as_str().map(String::from))
        .unwrap_or_else(|| format!("{context}: {status}"));

    Err(CliError::api(message))
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Gist {
    pub id: String,
    pub html_url: String,
    pub description: Option<String>,
    pub public: bool,
    pub created_at: String,
    pub updated_at: String,
    pub files: std::collections::HashMap<String, GistFile>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GistFile {
    pub filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreateGistRequest {
    description: String,
    public: bool,
    files: std::collections::HashMap<String, CreateGistFile>,
}

#[derive(Debug, Serialize)]
struct CreateGistFile {
    content: String,
}

pub async fn create_gist(
    client: &reqwest::Client,
    filename: &str,
    content: &str,
    public: bool,
    description: &str,
) -> Result<Gist, CliError> {
    let token = resolve_token()?;

    let mut files = std::collections::HashMap::new();
    files.insert(
        filename.to_string(),
        CreateGistFile {
            content: content.to_string(),
        },
    );

    let body = CreateGistRequest {
        description: description.to_string(),
        public,
        files,
    };

    let resp = client
        .post(format!("{BASE_URL}/gists"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .json(&body)
        .send()
        .await
        .map_err(|e| CliError::Http {
            context: "failed to create gist".into(),
            hint: None,
            source: e,
        })?;

    let resp = check_status(resp, "failed to create gist").await?;

    resp.json::<Gist>().await.map_err(|e| CliError::Http {
        context: "failed to parse create gist response".into(),
        hint: None,
        source: e,
    })
}

pub async fn list_gists(client: &reqwest::Client) -> Result<Vec<Gist>, CliError> {
    let token = resolve_token()?;

    let resp = client
        .get(format!("{BASE_URL}/gists"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .query(&[("per_page", "100")])
        .send()
        .await
        .map_err(|e| CliError::Http {
            context: "failed to list gists".into(),
            hint: None,
            source: e,
        })?;

    let resp = check_status(resp, "failed to list gists").await?;

    resp.json::<Vec<Gist>>()
        .await
        .map_err(|e| CliError::Http {
            context: "failed to parse list gists response".into(),
            hint: None,
            source: e,
        })
}

pub async fn get_gist(client: &reqwest::Client, gist_id: &str) -> Result<Gist, CliError> {
    let token = resolve_token()?;

    let resp = client
        .get(format!("{BASE_URL}/gists/{gist_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| CliError::Http {
            context: "failed to get gist".into(),
            hint: None,
            source: e,
        })?;

    let resp = check_status(resp, "failed to get gist").await?;

    resp.json::<Gist>().await.map_err(|e| CliError::Http {
        context: "failed to parse get gist response".into(),
        hint: None,
        source: e,
    })
}

pub async fn delete_gist(client: &reqwest::Client, gist_id: &str) -> Result<(), CliError> {
    let token = resolve_token()?;

    let resp = client
        .delete(format!("{BASE_URL}/gists/{gist_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| CliError::Http {
            context: "failed to delete gist".into(),
            hint: None,
            source: e,
        })?;

    check_status(resp, "failed to delete gist").await?;

    Ok(())
}
