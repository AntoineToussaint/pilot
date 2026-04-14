/// An item in the picker overlay.
#[derive(Debug, Clone)]
pub struct PickerItem {
    pub login: String,
    pub selected: bool,
    pub was_selected: bool,
}

/// State for the multi-select picker overlay (reviewers/assignees).
#[derive(Debug, Clone)]
pub struct PickerState {
    pub kind: crate::action::PickerKind,
    pub items: Vec<PickerItem>,
    pub cursor: usize,
    pub filter: String,
    pub session_key: String,
    pub repo: String,
    pub pr_number: String,
}

impl PickerState {
    pub fn filtered_indices(&self) -> Vec<usize> {
        self.items.iter().enumerate()
            .filter(|(_, item)| {
                self.filter.is_empty()
                    || item.login.to_lowercase().contains(&self.filter.to_lowercase())
            })
            .map(|(i, _)| i)
            .collect()
    }
}

pub(crate) fn build_picker_items(collaborators: &[String], current: &[String]) -> Vec<PickerItem> {
    let mut items: Vec<PickerItem> = collaborators.iter().map(|login| {
        let is_current = current.iter().any(|c| c == login);
        PickerItem {
            login: login.clone(),
            selected: is_current,
            was_selected: is_current,
        }
    }).collect();
    // Sort: selected first, then alphabetical.
    items.sort_by(|a, b| {
        b.selected.cmp(&a.selected)
            .then_with(|| a.login.to_lowercase().cmp(&b.login.to_lowercase()))
    });
    items
}

pub(crate) async fn fetch_collaborators(repo: &str) -> Vec<String> {
    let output = tokio::process::Command::new("gh")
        .args(["api", &format!("repos/{repo}/collaborators"), "--jq", ".[].login"])
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            tracing::error!("Failed to fetch collaborators for {repo}: {err}");
            // Fallback: try assignees endpoint (lower permission requirement).
            let fallback = tokio::process::Command::new("gh")
                .args(["api", &format!("repos/{repo}/assignees"), "--jq", ".[].login"])
                .output()
                .await;
            match fallback {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                }
                _ => vec![],
            }
        }
        Err(e) => {
            tracing::error!("Failed to run gh: {e}");
            vec![]
        }
    }
}
