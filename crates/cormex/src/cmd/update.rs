//! `cormex update` — the command, and what it says.
//!
//! The narration (downloading, running the new binary) goes to stderr from
//! `update::run`; what this module prints is the **outcome**, on stdout, in one
//! shape per case. A script that wants "did it change" reads the same three
//! first words either way.

use anyhow::Result;

use crate::client::ExitCode;
use crate::update::{self, Action, Options, Outcome};

/// Run the update and print what happened.
///
/// # Errors
/// As [`update::run`].
pub async fn run(opts: Options) -> Result<ExitCode> {
    let outcome = update::run(&opts).await?;
    print!("{}", render(&outcome));
    Ok(ExitCode::Ok)
}

/// The three outcomes, rendered.
#[must_use]
pub fn render(outcome: &Outcome) -> String {
    match &outcome.action {
        Action::UpToDate => format!("cormex {} is the newest release\n", outcome.current),
        Action::Available => format!(
            "cormex {} is available (this binary is {})\n  {} ({})\n\
             run `cormex update` to install it\n",
            outcome.target,
            outcome.current,
            outcome.asset,
            human_bytes(outcome.bytes)
        ),
        Action::Installed { path } => format!(
            "updated cormex {} -> {}\n  {}\n  {} ({})\n",
            outcome.current,
            outcome.target,
            path.display(),
            outcome.asset,
            human_bytes(outcome.bytes)
        ),
    }
}

/// Decimal megabytes, to one place.
///
/// Decimal rather than binary because the number comes from GitHub's API, which
/// counts in decimal — showing `4.0 MB` for a file the release page calls
/// `4.2 MB` reads as a different file.
fn human_bytes(bytes: u64) -> String {
    const MB: f64 = 1_000_000.0;
    const KB: f64 = 1_000.0;
    let bytes = bytes as f64;
    if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.0} kB", bytes / KB)
    } else {
        format!("{bytes:.0} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::Version;

    fn outcome(action: Action) -> Outcome {
        Outcome {
            current: Version::parse("0.1.13").unwrap(),
            target: Version::parse("0.1.14").unwrap(),
            asset: "cormex-0.1.14-x86_64-unknown-linux-musl.tar.gz".to_owned(),
            bytes: 4_200_000,
            action,
        }
    }

    #[test]
    fn an_update_says_what_it_replaced_and_where() {
        let text = render(&outcome(Action::Installed {
            path: std::path::PathBuf::from("/home/a/.local/bin/cormex"),
        }));
        assert!(
            text.starts_with("updated cormex 0.1.13 -> 0.1.14"),
            "{text}"
        );
        assert!(text.contains("/home/a/.local/bin/cormex"), "{text}");
        assert!(text.contains("4.2 MB"), "{text}");
    }

    #[test]
    fn a_check_says_what_is_available_and_how_to_get_it() {
        let text = render(&outcome(Action::Available));
        assert!(text.contains("0.1.14 is available"), "{text}");
        assert!(text.contains("this binary is 0.1.13"), "{text}");
        assert!(text.contains("cormex update"), "{text}");
    }

    #[test]
    fn being_current_is_one_line_and_not_an_error() {
        let text = render(&outcome(Action::UpToDate));
        assert_eq!(text, "cormex 0.1.13 is the newest release\n");
    }

    #[test]
    fn sizes_are_decimal_because_the_api_counts_in_decimal() {
        assert_eq!(human_bytes(4_200_000), "4.2 MB");
        assert_eq!(human_bytes(999), "999 bytes");
        assert_eq!(human_bytes(12_000), "12 kB");
    }
}
