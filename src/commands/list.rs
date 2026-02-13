use crate::api;
use crate::error::{self, CliError};

pub async fn execute(client: &reqwest::Client) -> Result<(), CliError> {
    let spinner = error::new_spinner("Fetching gists...");
    let gists = api::list_gists(client).await?;
    spinner.finish_and_clear();

    if gists.is_empty() {
        println!("No gists found.");
        return Ok(());
    }

    let dimmed = anstyle::Style::new().dimmed();

    for gist in &gists {
        let first_file = gist
            .files
            .keys()
            .next()
            .map(|s| s.as_str())
            .unwrap_or("(no files)");

        let visibility = if gist.public { " " } else { " (secret)" };

        use std::io::Write;
        let mut stderr = anstream::stdout();
        let _ = writeln!(
            stderr,
            "{first_file}{dimmed}{visibility} {}{dimmed:#}",
            &gist.html_url
        );
    }

    Ok(())
}
