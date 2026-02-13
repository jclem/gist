#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum CliError {
    #[error("{message}")]
    Auth {
        message: String,
        hint: Option<String>,
    },

    #[error("{message}")]
    Api {
        message: String,
        hint: Option<String>,
    },

    #[error("{context}")]
    Http {
        context: String,
        hint: Option<String>,
        #[source]
        source: reqwest::Error,
    },

    #[error("{context}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{context}")]
    Json {
        context: String,
        #[source]
        source: serde_json::Error,
    },
}

#[allow(dead_code)]
impl CliError {
    pub fn auth(message: impl Into<String>) -> Self {
        Self::Auth {
            message: message.into(),
            hint: None,
        }
    }

    pub fn auth_with_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::Auth {
            message: message.into(),
            hint: Some(hint.into()),
        }
    }

    pub fn api(message: impl Into<String>) -> Self {
        Self::Api {
            message: message.into(),
            hint: None,
        }
    }

    fn hint(&self) -> Option<&str> {
        match self {
            Self::Auth { hint, .. } | Self::Api { hint, .. } | Self::Http { hint, .. } => {
                hint.as_deref()
            }
            Self::Io { .. } | Self::Json { .. } => None,
        }
    }
}

pub fn exit_code(e: &CliError) -> i32 {
    match e {
        CliError::Auth { .. } => 4,
        CliError::Api { .. } | CliError::Http { .. } => 5,
        CliError::Io { .. } | CliError::Json { .. } => 1,
    }
}

pub fn print_success(message: &str) {
    use std::io::Write;

    let mut stderr = anstream::stderr();
    let green =
        anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Green)));
    let _ = writeln!(stderr, "{green}\u{2714}{green:#} {message}");
}

pub fn new_spinner(msg: &str) -> indicatif::ProgressBar {
    let spinner = indicatif::ProgressBar::new_spinner();
    spinner.set_draw_target(indicatif::ProgressDrawTarget::stderr());
    spinner.set_style(
        indicatif::ProgressStyle::default_spinner()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✔"])
            .template("{spinner:.green} {msg}")
            .unwrap(),
    );
    spinner.set_message(msg.to_string());
    spinner.enable_steady_tick(std::time::Duration::from_millis(80));
    spinner
}

pub fn print_error(e: &CliError, verbose: bool) {
    use std::error::Error;
    use std::io::Write;

    let mut stderr = anstream::stderr();
    let bold_red = anstyle::Style::new()
        .bold()
        .fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Red)));
    let dimmed = anstyle::Style::new().dimmed();

    let _ = writeln!(stderr, "{bold_red}error:{bold_red:#} {e}");

    if let Some(hint) = e.hint() {
        let cyan = anstyle::Style::new()
            .bold()
            .fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Cyan)));
        let _ = writeln!(stderr, "  {cyan}hint:{cyan:#} {dimmed}{hint}{dimmed:#}");
    }

    if verbose {
        let mut source = e.source();
        while let Some(cause) = source {
            let _ = writeln!(stderr, "  {dimmed}caused by:{dimmed:#} {cause}");
            source = cause.source();
        }
    }
}
