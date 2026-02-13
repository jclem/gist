use std::io::{IsTerminal, Read};

use crate::api;
use crate::error::{self, CliError};

pub async fn execute(
    client: &reqwest::Client,
    name: Option<String>,
    public: bool,
    editor: bool,
) -> Result<(), CliError> {
    let content = if editor {
        read_from_editor()?
    } else {
        read_from_stdin()?
    };

    if content.trim().is_empty() {
        return Err(CliError::api("no content provided"));
    }

    let filename = name.unwrap_or_else(|| "gistfile1.txt".to_string());
    let spinner = error::new_spinner("Creating gist...");
    let gist = api::create_gist(client, &filename, &content, public, "").await?;
    spinner.finish_and_clear();

    error::print_success(&gist.html_url);

    Ok(())
}

fn read_from_stdin() -> Result<String, CliError> {
    let stdin = std::io::stdin();

    if stdin.is_terminal() {
        use std::io::Write;
        let mut stderr = anstream::stderr();
        let dimmed = anstyle::Style::new().dimmed();
        let _ = writeln!(
            stderr,
            "{dimmed}Enter content, then press Ctrl-D to save:{dimmed:#}"
        );
    }

    let mut buf = String::new();
    stdin.lock().read_to_string(&mut buf).map_err(|e| CliError::Io {
        context: "failed to read from stdin".into(),
        source: e,
    })?;

    Ok(buf)
}

fn read_from_editor() -> Result<String, CliError> {
    let editor = std::env::var("EDITOR")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| CliError::api("EDITOR is not set"))?;

    let tmp = std::env::temp_dir().join(format!("gist-{}.txt", std::process::id()));

    std::fs::write(&tmp, "").map_err(|e| CliError::Io {
        context: "failed to create temp file".into(),
        source: e,
    })?;

    let status = std::process::Command::new(&editor)
        .arg(&tmp)
        .status()
        .map_err(|e| CliError::Io {
            context: format!("failed to launch editor ({editor})"),
            source: e,
        })?;

    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(CliError::api("editor exited with non-zero status"));
    }

    let content = std::fs::read_to_string(&tmp).map_err(|e| CliError::Io {
        context: "failed to read temp file".into(),
        source: e,
    })?;

    let _ = std::fs::remove_file(&tmp);

    Ok(content)
}
