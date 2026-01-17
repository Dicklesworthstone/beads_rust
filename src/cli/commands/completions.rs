//! Shell completions generation command.
//!
//! Generates shell completion scripts for bash, zsh, fish, `PowerShell`, and elvish.
//!
//! # Usage
//!
//! ```bash
//! # Generate bash completions to stdout
//! br completions bash
//!
//! # Generate zsh completions to a file
//! br completions zsh -o ~/.zsh/completions/_br
//!
//! # Generate fish completions
//! br completions fish > ~/.config/fish/completions/br.fish
//! ```

use crate::cli::{Cli, CompletionsArgs, ShellType};
use crate::error::Result;
use clap::CommandFactory;
use clap_complete::{generate, Shell};
use std::io;
use tracing::info;

/// Execute the completions command.
///
/// # Errors
///
/// Returns an error if file I/O fails.
pub fn execute(args: &CompletionsArgs) -> Result<()> {
    info!(shell = ?args.shell, output = ?args.output, "Generating shell completions");

    let mut cmd = Cli::command();
    let shell = convert_shell_type(args.shell);

    if let Some(output_path) = &args.output {
        // Generate to file
        let mut file = std::fs::File::create(output_path)?;
        generate(shell, &mut cmd, "br", &mut file);
        info!(path = %output_path.display(), "Wrote completion script");
        eprintln!("Generated {} completions to {}", shell_name(args.shell), output_path.display());
    } else {
        // Generate to stdout
        generate(shell, &mut cmd, "br", &mut io::stdout());
    }

    Ok(())
}

/// Convert our `ShellType` enum to `clap_complete`'s Shell enum.
const fn convert_shell_type(shell: ShellType) -> Shell {
    match shell {
        ShellType::Bash => Shell::Bash,
        ShellType::Zsh => Shell::Zsh,
        ShellType::Fish => Shell::Fish,
        ShellType::PowerShell => Shell::PowerShell,
        ShellType::Elvish => Shell::Elvish,
    }
}

/// Get human-readable shell name.
const fn shell_name(shell: ShellType) -> &'static str {
    match shell {
        ShellType::Bash => "bash",
        ShellType::Zsh => "zsh",
        ShellType::Fish => "fish",
        ShellType::PowerShell => "PowerShell",
        ShellType::Elvish => "elvish",
    }
}

/// Print installation instructions for the generated completions.
pub fn print_install_instructions(shell: ShellType) {
    match shell {
        ShellType::Bash => {
            eprintln!("\n# Installation instructions for bash:");
            eprintln!("# Option 1: User installation");
            eprintln!("mkdir -p ~/.local/share/bash-completion/completions");
            eprintln!("br completions bash > ~/.local/share/bash-completion/completions/br");
            eprintln!("\n# Option 2: System-wide (requires sudo)");
            eprintln!("sudo br completions bash > /etc/bash_completion.d/br");
            eprintln!("\n# Then restart your shell or run: source ~/.bashrc");
        }
        ShellType::Zsh => {
            eprintln!("\n# Installation instructions for zsh:");
            eprintln!("# Option 1: User installation");
            eprintln!("mkdir -p ~/.zsh/completions");
            eprintln!("br completions zsh > ~/.zsh/completions/_br");
            eprintln!("# Add to ~/.zshrc: fpath=(~/.zsh/completions $fpath)");
            eprintln!("\n# Option 2: Oh My Zsh");
            eprintln!("br completions zsh > ~/.oh-my-zsh/completions/_br");
            eprintln!("\n# Then restart your shell or run: exec zsh");
        }
        ShellType::Fish => {
            eprintln!("\n# Installation instructions for fish:");
            eprintln!("mkdir -p ~/.config/fish/completions");
            eprintln!("br completions fish > ~/.config/fish/completions/br.fish");
            eprintln!("\n# Completions are loaded automatically on next fish session");
        }
        ShellType::PowerShell => {
            eprintln!("\n# Installation instructions for PowerShell:");
            eprintln!("# Add to your PowerShell profile ($PROFILE):");
            eprintln!("br completions powershell | Out-String | Invoke-Expression");
        }
        ShellType::Elvish => {
            eprintln!("\n# Installation instructions for elvish:");
            eprintln!("mkdir -p ~/.elvish/lib");
            eprintln!("br completions elvish > ~/.elvish/lib/br.elv");
            eprintln!("# Add to ~/.elvish/rc.elv: use br");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_shell_type() {
        assert_eq!(convert_shell_type(ShellType::Bash), Shell::Bash);
        assert_eq!(convert_shell_type(ShellType::Zsh), Shell::Zsh);
        assert_eq!(convert_shell_type(ShellType::Fish), Shell::Fish);
        assert_eq!(convert_shell_type(ShellType::PowerShell), Shell::PowerShell);
        assert_eq!(convert_shell_type(ShellType::Elvish), Shell::Elvish);
    }

    #[test]
    fn test_bash_completion_generation() {
        let mut cmd = Cli::command();
        let mut output = Vec::new();
        generate(Shell::Bash, &mut cmd, "br", &mut output);
        let script = String::from_utf8(output).unwrap();

        // Verify basic structure
        assert!(script.contains("complete"), "should contain complete command");
        assert!(script.contains("br"), "should reference br command");
        assert!(script.contains("_br"), "should define _br function");
    }

    #[test]
    fn test_zsh_completion_generation() {
        let mut cmd = Cli::command();
        let mut output = Vec::new();
        generate(Shell::Zsh, &mut cmd, "br", &mut output);
        let script = String::from_utf8(output).unwrap();

        assert!(script.contains("#compdef"), "should start with #compdef");
        assert!(script.contains("br"), "should reference br command");
    }

    #[test]
    fn test_fish_completion_generation() {
        let mut cmd = Cli::command();
        let mut output = Vec::new();
        generate(Shell::Fish, &mut cmd, "br", &mut output);
        let script = String::from_utf8(output).unwrap();

        assert!(script.contains("complete -c br"), "should use fish complete syntax");
    }

    #[test]
    fn test_completion_contains_commands() {
        let mut cmd = Cli::command();
        let mut output = Vec::new();
        generate(Shell::Bash, &mut cmd, "br", &mut output);
        let script = String::from_utf8(output).unwrap();

        // Verify common commands are in completions
        assert!(script.contains("create"), "should include create command");
        assert!(script.contains("list"), "should include list command");
        assert!(script.contains("show"), "should include show command");
        assert!(script.contains("update"), "should include update command");
        assert!(script.contains("close"), "should include close command");
    }

    #[test]
    fn test_completion_contains_global_flags() {
        let mut cmd = Cli::command();
        let mut output = Vec::new();
        generate(Shell::Bash, &mut cmd, "br", &mut output);
        let script = String::from_utf8(output).unwrap();

        // Verify global flags are in completions
        assert!(script.contains("--json"), "should include --json flag");
        assert!(script.contains("--verbose"), "should include --verbose flag");
    }
}
