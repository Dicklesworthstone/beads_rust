use crate::model::Issue;
use crate::output::Theme;
use rich_rust::prelude::*;

/// Renders a dependency tree for an issue.
pub struct DependencyTree<'a> {
    root_issue: &'a Issue,
    all_issues: &'a [Issue],
    theme: &'a Theme,
    max_depth: usize,
}

impl<'a> DependencyTree<'a> {
    #[must_use]
    pub fn new(root: &'a Issue, all: &'a [Issue], theme: &'a Theme) -> Self {
        Self {
            root_issue: root,
            all_issues: all,
            theme,
            max_depth: 10,
        }
    }

    #[must_use]
    pub fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }

    #[must_use]
    pub fn build(&self) -> Tree {
        let root_node = self.build_node(self.root_issue, 0);

        Tree::new(root_node)
            .guides(TreeGuides::Rounded)
            .guide_style(self.theme.dimmed.clone())
    }

    fn build_node(&self, issue: &Issue, depth: usize) -> TreeNode {
        // Create label with ID, status, and title
        let label = format!(
            "{} [{}] {}",
            issue.id,
            format!("{}", issue.status).chars().next().unwrap_or('?'),
            truncate(&issue.title, 40)
        );

        let mut node = TreeNode::new(Text::new(label));

        // Recursively add dependencies (if not too deep)
        if depth < self.max_depth {
            for dep in &issue.dependencies {
                if let Some(dep_issue) = self.find_issue(&dep.depends_on_id) {
                    let child = self.build_node(dep_issue, depth + 1);
                    node = node.child(child);
                } else {
                    // Dependency not found (external or deleted)
                    let missing =
                        TreeNode::new(Text::new(format!("{} [?] (not found)", dep.depends_on_id)));
                    node = node.child(missing);
                }
            }
        }

        node
    }

    fn find_issue(&self, id: &str) -> Option<&Issue> {
        self.all_issues.iter().find(|i| i.id == id)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut truncated: String = s.chars().take(max.saturating_sub(3)).collect();
        truncated.push_str("...");
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dep_tree_truncation_safe() {
        // String with emojis (multi-byte)
        let title = "😊".repeat(20); // 80 bytes, 20 chars.
        // Max 10 chars.
        let truncated = truncate(&title, 10);

        assert_eq!(truncated.chars().count(), 10);
        assert!(truncated.starts_with("😊"));
        assert!(truncated.ends_with("..."));
    }
}
