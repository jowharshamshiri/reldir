//! The documentation, built into the binary: `reldir docs <topic>` prints a
//! page, and the MCP server serves the same pages as resources. The pages are
//! the site's own, with what only the site renders -- front matter, links
//! through the site's address -- made plain.

/// Every page, by topic: the tutorial, the how-to guides, the reference and
/// the explanation, in the order the site's front page lists them.
pub const PAGES: &[(&str, &str)] = &[
    (
        "getting-started",
        include_str!(concat!(env!("RELDIR_DOCS"), "/getting-started.md")),
    ),
    (
        "install",
        include_str!(concat!(env!("RELDIR_DOCS"), "/install.md")),
    ),
    (
        "adopt-a-folder",
        include_str!(concat!(env!("RELDIR_DOCS"), "/adopt-a-folder.md")),
    ),
    (
        "declare-a-table",
        include_str!(concat!(env!("RELDIR_DOCS"), "/declare-a-table.md")),
    ),
    (
        "widen-a-contract",
        include_str!(concat!(env!("RELDIR_DOCS"), "/widen-a-contract.md")),
    ),
    (
        "change-a-schema",
        include_str!(concat!(env!("RELDIR_DOCS"), "/change-a-schema.md")),
    ),
    (
        "import-and-export",
        include_str!(concat!(env!("RELDIR_DOCS"), "/import-and-export.md")),
    ),
    (
        "repair-a-database",
        include_str!(concat!(env!("RELDIR_DOCS"), "/repair-a-database.md")),
    ),
    (
        "undo-a-change",
        include_str!(concat!(env!("RELDIR_DOCS"), "/undo-a-change.md")),
    ),
    (
        "recover",
        include_str!(concat!(env!("RELDIR_DOCS"), "/recover.md")),
    ),
    (
        "retry-safely",
        include_str!(concat!(env!("RELDIR_DOCS"), "/retry-safely.md")),
    ),
    (
        "merge-with-git",
        include_str!(concat!(env!("RELDIR_DOCS"), "/merge-with-git.md")),
    ),
    (
        "check-in-ci",
        include_str!(concat!(env!("RELDIR_DOCS"), "/check-in-ci.md")),
    ),
    (
        "connect-an-agent",
        include_str!(concat!(env!("RELDIR_DOCS"), "/connect-an-agent.md")),
    ),
    (
        "keep-a-large-folder-fast",
        include_str!(concat!(env!("RELDIR_DOCS"), "/keep-a-large-folder-fast.md")),
    ),
    ("cli", include_str!(concat!(env!("RELDIR_DOCS"), "/cli.md"))),
    ("sql", include_str!(concat!(env!("RELDIR_DOCS"), "/sql.md"))),
    (
        "schemas",
        include_str!(concat!(env!("RELDIR_DOCS"), "/schemas.md")),
    ),
    (
        "validation",
        include_str!(concat!(env!("RELDIR_DOCS"), "/validation.md")),
    ),
    (
        "migrations",
        include_str!(concat!(env!("RELDIR_DOCS"), "/migrations.md")),
    ),
    (
        "snapshots",
        include_str!(concat!(env!("RELDIR_DOCS"), "/snapshots.md")),
    ),
    ("git", include_str!(concat!(env!("RELDIR_DOCS"), "/git.md"))),
    ("mcp", include_str!(concat!(env!("RELDIR_DOCS"), "/mcp.md"))),
    (
        "protocol",
        include_str!(concat!(env!("RELDIR_DOCS"), "/protocol.md")),
    ),
    (
        "errors",
        include_str!(concat!(env!("RELDIR_DOCS"), "/errors.md")),
    ),
    (
        "configuration",
        include_str!(concat!(env!("RELDIR_DOCS"), "/configuration.md")),
    ),
    (
        "format",
        include_str!(concat!(env!("RELDIR_DOCS"), "/format.md")),
    ),
    (
        "python",
        include_str!(concat!(env!("RELDIR_DOCS"), "/python.md")),
    ),
    (
        "node",
        include_str!(concat!(env!("RELDIR_DOCS"), "/node.md")),
    ),
    (
        "rust",
        include_str!(concat!(env!("RELDIR_DOCS"), "/rust.md")),
    ),
    (
        "stability",
        include_str!(concat!(env!("RELDIR_DOCS"), "/stability.md")),
    ),
    (
        "concepts",
        include_str!(concat!(env!("RELDIR_DOCS"), "/concepts.md")),
    ),
    (
        "transactions",
        include_str!(concat!(env!("RELDIR_DOCS"), "/transactions.md")),
    ),
    (
        "performance",
        include_str!(concat!(env!("RELDIR_DOCS"), "/performance.md")),
    ),
    (
        "security",
        include_str!(concat!(env!("RELDIR_DOCS"), "/security.md")),
    ),
    (
        "comparison",
        include_str!(concat!(env!("RELDIR_DOCS"), "/comparison.md")),
    ),
];

/// A page as plain Markdown: no front matter, and each link into the site
/// written as the topic `reldir docs` prints.
pub fn render(source: &str) -> String {
    let body = match source.strip_prefix("---\n") {
        Some(rest) => rest.split_once("\n---\n").map_or(rest, |(_, body)| body),
        None => source,
    };
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(at) = rest.find("{{ site.baseurl }}/") {
        out.push_str(&rest[..at]);
        rest = &rest[at + "{{ site.baseurl }}/".len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(rest.len());
        out.push_str(&format!("reldir docs {}", &rest[..end]));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out.trim_start().to_string()
}

/// A page's title: its first heading.
pub fn title(source: &str) -> String {
    render(source)
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .unwrap_or_default()
        .to_string()
}

/// The page on a topic, rendered.
pub fn page(topic: &str) -> Option<String> {
    PAGES
        .iter()
        .find(|(name, _)| *name == topic)
        .map(|(_, source)| render(source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test2231_every_page_renders_without_site_markup() {
        for (topic, source) in PAGES {
            let text = render(source);
            assert!(!text.contains("{{"), "{topic} keeps site markup");
            assert!(!text.starts_with("---"), "{topic} keeps its front matter");
            assert!(!title(source).is_empty(), "{topic} has no title");
        }
        assert_eq!(
            render("---\ntitle: X\n---\n\nSee [sql]({{ site.baseurl }}/sql#joins)."),
            "See [sql](reldir docs sql#joins)."
        );
    }

    #[test]
    fn test2233_every_page_of_the_site_is_a_topic() {
        let mut site: Vec<String> = std::fs::read_dir(env!("RELDIR_DOCS"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter_map(|name| name.strip_suffix(".md").map(str::to_string))
            // The site's front page is the list of topics itself.
            .filter(|name| name != "index")
            .collect();
        site.sort();
        let mut topics: Vec<String> = PAGES.iter().map(|(name, _)| name.to_string()).collect();
        topics.sort();
        assert_eq!(topics, site);
    }
}
