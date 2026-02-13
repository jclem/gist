use crate::api;
use crate::error::{self, CliError};

pub async fn execute(client: &reqwest::Client, url: &str) -> Result<(), CliError> {
    let gist_id = parse_gist_id(url)?;

    let spinner = error::new_spinner("Fetching gist...");
    let gist = api::get_gist(client, &gist_id).await?;
    spinner.finish_and_clear();

    for (filename, file) in &gist.files {
        if gist.files.len() > 1 {
            let dimmed = anstyle::Style::new().dimmed();
            use std::io::Write;
            let mut stderr = anstream::stderr();
            let _ = writeln!(stderr, "{dimmed}--- {filename} ---{dimmed:#}");
        }

        if let Some(content) = &file.content {
            print!("{content}");
        }
    }

    Ok(())
}

fn parse_gist_id(url: &str) -> Result<String, CliError> {
    // Accept raw ID or full URL
    if !url.contains('/') {
        return Ok(url.to_string());
    }

    url.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| CliError::api(format!("could not parse gist ID from: {url}")))
}
