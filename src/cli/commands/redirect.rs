//! CLI adapter for safe worktree redirect management.

use crate::cli::RedirectSetArgs;
use crate::error::Result;
use crate::output::{OutputContext, OutputMode};
use crate::redirect::{RedirectDisposition, RedirectReceipt, RedirectTargetMode};
use std::path::Path;

const fn created_qualifier(mode: RedirectTargetMode) -> &'static str {
    match mode {
        RedirectTargetMode::Explicit => "Created",
        RedirectTargetMode::Automatic => "Automatically created",
    }
}

/// Execute `br redirect set`.
///
/// # Errors
///
/// Returns an error when source/target validation or safe publication fails.
pub fn execute_set(args: &RedirectSetArgs, ctx: &OutputContext) -> Result<()> {
    let receipt =
        crate::redirect::set_redirect(Path::new("."), args.target.as_deref(), args.allow_existing)?;
    render_receipt(&receipt, ctx)
}

/// Render a redirect receipt for init or redirect-management commands.
///
/// # Errors
///
/// Returns an error if JSON serialization fails.
pub(crate) fn render_receipt(receipt: &RedirectReceipt, ctx: &OutputContext) -> Result<()> {
    match ctx.mode() {
        OutputMode::Json => println!("{}", serde_json::to_string_pretty(receipt)?),
        OutputMode::Quiet => {}
        OutputMode::Rich | OutputMode::Plain | OutputMode::Toon => match receipt.disposition {
            RedirectDisposition::Created => {
                let qualifier = created_qualifier(receipt.target_mode);
                println!(
                    "{qualifier} worktree redirect: {} -> {}",
                    receipt.source_workspace.display(),
                    receipt.final_target.display()
                );
                if receipt.existing_state_acknowledged {
                    println!(
                        "Acknowledged {} preserved dormant local artifact(s)",
                        receipt.dormant_artifacts.len()
                    );
                    for artifact in &receipt.dormant_artifacts {
                        println!("  {}", artifact.display());
                    }
                }
            }
            RedirectDisposition::Unchanged => println!(
                "Worktree redirect already configured: {} -> {}",
                receipt.source_workspace.display(),
                receipt.final_target.display()
            ),
            RedirectDisposition::PrimaryOwner => println!(
                "Primary worktree already owns beads workspace: {}",
                receipt.final_target.display()
            ),
            RedirectDisposition::Refused => println!(
                "Worktree redirect refused without changes: {}",
                receipt.source_workspace.display()
            ),
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_qualifier_distinguishes_explicit_and_automatic_targets() {
        assert_eq!(created_qualifier(RedirectTargetMode::Explicit), "Created");
        assert_eq!(
            created_qualifier(RedirectTargetMode::Automatic),
            "Automatically created"
        );
    }
}
