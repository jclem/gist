use crate::api;
use crate::error::{self, CliError};

pub async fn execute(client: &reqwest::Client, url: &str) -> Result<(), CliError> {
    let gist_id = parse_gist_id(url)?;

    let spinner = error::new_spinner("Deleting gist...");
    api::delete_gist(client, &gist_id).await?;
    spinner.finish_and_clear();

    error::print_success("gist deleted");

    Ok(())
}

fn parse_gist_id(url: &str) -> Result<String, CliError> {
    if !url.contains('/') {
        return Ok(url.to_string());
    }

    url.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| CliError::api(format!("could not parse gist ID from: {url}")))
}
