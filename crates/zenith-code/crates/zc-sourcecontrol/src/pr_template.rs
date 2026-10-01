//! `PrTemplateDetection.ts`: the pull request template of a base tree, read from committed blobs
//! (`ls-tree` + `cat-file`), never from the worktree, so repository-controlled symlinks and path
//! races cannot reach the host file system.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use zc_vcs::git_exec::ExecuteGitInput;
use zc_vcs::GitVcsDriver;

const TEMPLATE_MAX_BYTES: usize = 8_000;
const TREE_LIST_MAX_BYTES: usize = 100_000;
const TRUNCATION_MARKER: &str = "[truncated]";

pub const TEMPLATE_PATHS: [&str; 6] = [
    ".github/pull_request_template.md",
    ".github/PULL_REQUEST_TEMPLATE.md",
    "pull_request_template.md",
    "PULL_REQUEST_TEMPLATE.md",
    "docs/pull_request_template.md",
    "docs/PULL_REQUEST_TEMPLATE.md",
];

pub const TEMPLATE_DIRECTORIES: [&str; 3] = [".github/PULL_REQUEST_TEMPLATE", "PULL_REQUEST_TEMPLATE", "docs/PULL_REQUEST_TEMPLATE"];

#[derive(Debug, Clone)]
struct TreeEntry {
    object_id: String,
    path: String,
}

fn parse_template_tree_entries(output: &str) -> Vec<TreeEntry> {
    static OBJECT_ID: OnceLock<Regex> = OnceLock::new();
    let object_id = OBJECT_ID.get_or_init(|| Regex::new(r"^[0-9a-f]{40,64}$").expect("valid regex"));
    output
        .split('\0')
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            let separator = record.find('\t')?;
            let mut fields = record[..separator].split(' ');
            let (mode, kind, id) = (fields.next()?, fields.next()?, fields.next()?);
            if kind != "blob" || (mode != "100644" && mode != "100755") || !object_id.is_match(id) {
                return None;
            }
            Some(TreeEntry {
                object_id: id.to_owned(),
                path: record[separator + 1..].to_owned(),
            })
        })
        .collect()
}

async fn read_template_blob(git: &GitVcsDriver, cwd: &str, entry: &TreeEntry) -> Result<Option<String>, ()> {
    let mut input = ExecuteGitInput::new("PrTemplateDetection.readTemplateBlob", cwd, ["cat-file", "blob", entry.object_id.as_str()]);
    input.max_output_bytes = Some(TEMPLATE_MAX_BYTES);
    input.append_truncation_marker = true;
    let result = git.execute(input).await.map_err(drop)?;
    let template = crate::util::js_trim(&result.stdout);
    if template.is_empty() {
        return Ok(None);
    }
    Ok(Some(if result.stdout_truncated && !template.ends_with(TRUNCATION_MARKER) {
        format!("{template}\n\n{TRUNCATION_MARKER}")
    } else {
        template.to_owned()
    }))
}

enum DirectoryTemplate {
    None,
    Ambiguous,
    Template(String),
}

async fn read_template_directory(git: &GitVcsDriver, cwd: &str, entries: &[TreeEntry], directory: &str) -> Result<DirectoryTemplate, ()> {
    let prefix = format!("{directory}/");
    let mut templates = Vec::new();
    for entry in entries {
        let Some(relative) = entry.path.strip_prefix(&prefix) else {
            continue;
        };
        if relative.contains('/') || !relative.to_lowercase().ends_with(".md") {
            continue;
        }
        if let Some(template) = read_template_blob(git, cwd, entry).await? {
            templates.push(template);
            if templates.len() > 1 {
                return Ok(DirectoryTemplate::Ambiguous);
            }
        }
    }
    Ok(templates.pop().map_or(DirectoryTemplate::None, DirectoryTemplate::Template))
}

/// `detectPrTemplate(cwd, treeish, executeGit)`: the first non-empty template in path order,
/// else the single template of a template directory. Any failure is "no template".
pub async fn detect_pr_template(cwd: &str, treeish: &str, git: &GitVcsDriver) -> Option<String> {
    detect(cwd, treeish, git).await.ok().flatten()
}

async fn detect(cwd: &str, treeish: &str, git: &GitVcsDriver) -> Result<Option<String>, ()> {
    let mut args = vec![
        "ls-tree".to_owned(),
        "-r".into(),
        "-z".into(),
        "--full-tree".into(),
        treeish.to_owned(),
        "--".into(),
    ];
    args.extend(TEMPLATE_PATHS.iter().chain(TEMPLATE_DIRECTORIES.iter()).map(|p| (*p).to_owned()));
    let mut input = ExecuteGitInput::new("PrTemplateDetection.listTemplates", cwd, args);
    input.max_output_bytes = Some(TREE_LIST_MAX_BYTES);
    input.append_truncation_marker = true;
    let result = git.execute(input).await.map_err(drop)?;
    if result.stdout_truncated {
        return Ok(None);
    }
    let entries = parse_template_tree_entries(&result.stdout);
    let by_path: HashMap<&str, &TreeEntry> = entries.iter().map(|e| (e.path.as_str(), e)).collect();
    for path in TEMPLATE_PATHS {
        if let Some(entry) = by_path.get(path) {
            if let Some(template) = read_template_blob(git, cwd, entry).await? {
                return Ok(Some(template));
            }
        }
    }
    for directory in TEMPLATE_DIRECTORIES {
        match read_template_directory(git, cwd, &entries, directory).await? {
            DirectoryTemplate::Template(template) => return Ok(Some(template)),
            DirectoryTemplate::Ambiguous => return Ok(None),
            DirectoryTemplate::None => {}
        }
    }
    Ok(None)
}
