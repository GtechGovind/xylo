//! Numeric GitHub continuation metadata. Link targets are never executed. A
//! continuation must retain the exact origin, route and non-page query scope.
use anyhow::{Context, Result, ensure};
use reqwest::header::{HeaderMap, LINK};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use url::Url;

pub(super) fn github(headers: &HeaderMap, request: &Url, page: u64) -> Result<Value> {
    let mut next = None;
    let mut last = None;
    let mut size = 0usize;
    for header in headers.get_all(LINK) {
        size += header.as_bytes().len();
        ensure!(
            size <= 16 * 1024,
            "GitHub pagination header exceeds its size limit"
        );
        let mut remaining = header
            .to_str()
            .context("Invalid GitHub pagination header")?;
        while !remaining.trim().is_empty() {
            remaining = remaining.trim_start();
            ensure!(remaining.starts_with('<'), "Invalid GitHub pagination link");
            let end = remaining
                .find('>')
                .context("Incomplete GitHub pagination link")?;
            let target = request.join(&remaining[1..end])?;
            remaining = &remaining[end + 1..];
            let (parameters, rest) = remaining.split_once(',').unwrap_or((remaining, ""));
            remaining = rest;
            let relation = parameters.split(';').find_map(|parameter| {
                let (key, value) = parameter.trim().split_once('=')?;
                (key.trim() == "rel").then(|| value.trim().trim_matches('"'))
            });
            let Some(relation) = relation else { continue };
            if !relation
                .split_ascii_whitespace()
                .any(|item| matches!(item, "next" | "last"))
            {
                continue;
            }
            ensure!(
                target.origin() == request.origin()
                    && target.username().is_empty()
                    && target.password().is_none()
                    && target.path() == request.path()
                    && target.fragment().is_none()
                    && query_scope(&target)? == query_scope(request)?,
                "GitHub continuation changed the requested resource"
            );
            let pages: Vec<_> = target
                .query_pairs()
                .filter(|(key, _)| key == "page")
                .map(|(_, value)| value.into_owned())
                .collect();
            ensure!(
                pages.len() == 1,
                "GitHub continuation requires one page number"
            );
            let target_page: u64 = pages[0]
                .parse()
                .context("Invalid GitHub continuation page")?;
            ensure!(
                (1..=1000).contains(&target_page),
                "GitHub continuation exceeds the page bound"
            );
            for relation in relation.split_ascii_whitespace() {
                match relation {
                    "next" => {
                        ensure!(
                            target_page > page && next.is_none(),
                            "Invalid next GitHub page"
                        );
                        next = Some(target_page);
                    }
                    "last" => {
                        ensure!(
                            target_page >= page && last.is_none(),
                            "Invalid last GitHub page"
                        );
                        last = Some(target_page);
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(json!({"page":page,"next_page":next,"last_page":last}))
}

fn query_scope(url: &Url) -> Result<BTreeMap<String, String>> {
    let mut scope = BTreeMap::new();
    for (key, value) in url.query_pairs().filter(|(key, _)| key != "page") {
        ensure!(
            scope.insert(key.into_owned(), value.into_owned()).is_none(),
            "Duplicate GitHub pagination query argument"
        );
    }
    Ok(scope)
}
