use super::{bounded_title, excerpt, GlobalSearchHit, GlobalSearchPage};
use crate::commands::workspace::{
    is_allowed_linked_asset_rel_path, should_skip_workspace_entry_with_hidden,
};
use std::{path::Path, time::UNIX_EPOCH};

fn resource_hit(
    kind: &'static str,
    id: String,
    title: &str,
    path: String,
    query: &str,
    modified_at: i64,
) -> GlobalSearchHit {
    GlobalSearchHit {
        kind,
        id,
        title: bounded_title(title, query),
        excerpt: path.clone(),
        field: "title",
        doc_type: None,
        path: Some(path),
        message_id: None,
        checkout_id: None,
        archived: false,
        modified_at,
    }
}

/// File hits per page. The ranked list is rebuilt for every page, so one page
/// covers the panel's initial result budget with a single walk.
const FILE_PAGE_SIZE: usize = 40;

/// Ranks a path that contains `query_lower` (ASCII case-insensitive). Lower
/// keys sort first: an exact file name or stem, a file-name prefix, a match
/// inside the file name (earlier first), and last a match only in a parent
/// directory, so every file-name match precedes every path-only match.
fn file_match_key(rel: &str, name: &str, query_lower: &str) -> Option<(u8, usize)> {
    if !rel.to_ascii_lowercase().contains(query_lower) {
        return None;
    }
    let name_lower = name.to_ascii_lowercase();
    let stem = name_lower
        .rsplit_once('.')
        .map_or(name_lower.as_str(), |(stem, _)| stem);
    Some(if name_lower == query_lower || stem == query_lower {
        (0, 0)
    } else if name_lower.starts_with(query_lower) {
        (1, 0)
    } else if let Some(index) = name_lower.find(query_lower) {
        (2, index)
    } else {
        (3, 0)
    })
}

struct RankedFile {
    tier: u8,
    position: usize,
    rel: String,
    entry: walkdir::DirEntry,
}

pub(super) fn search_files_page(
    root: &Path,
    query: &str,
    cursor: Option<&str>,
) -> Result<GlobalSearchPage, String> {
    let offset = match cursor {
        Some(cursor) => cursor
            .parse::<usize>()
            .map_err(|_| "Invalid file search cursor".to_string())?,
        None => 0,
    };
    let query_lower = query.to_ascii_lowercase();
    let mut ranked = Vec::new();
    let walk = walkdir::WalkDir::new(root)
        .min_depth(1)
        .max_depth(64)
        .follow_links(true)
        .into_iter()
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            let is_dir = entry.file_type().is_dir();
            if should_skip_workspace_entry_with_hidden(&name, is_dir, false, None)
                || (is_dir && name.eq_ignore_ascii_case("target"))
            {
                return false;
            }
            if !(is_dir && entry.path_is_symlink()) {
                return true;
            }
            let rel = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            is_allowed_linked_asset_rel_path(&rel)
        });
    for entry in walk {
        let entry = match entry {
            Ok(entry) => entry,
            // Files may disappear during a scan; linked asset cycles are not results.
            Err(error)
                if error.loop_ancestor().is_some()
                    || error
                        .io_error()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue
            }
            Err(error) => return Err(format!("File search failed: {error}")),
        };
        if !entry.file_type().is_file() {
            continue;
        }
        // Unity `.meta` sidecars duplicate every asset hit.
        if entry
            .path()
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("meta"))
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        let rel = entry
            .path()
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let Some((tier, position)) = file_match_key(&rel, &name, &query_lower) else {
            continue;
        };
        ranked.push(RankedFile {
            tier,
            position,
            rel,
            entry,
        });
    }
    ranked.sort_by(|left, right| {
        left.tier
            .cmp(&right.tier)
            .then(left.position.cmp(&right.position))
            .then(
                left.entry
                    .file_name()
                    .len()
                    .cmp(&right.entry.file_name().len()),
            )
            .then(left.entry.depth().cmp(&right.entry.depth()))
            .then_with(|| left.rel.cmp(&right.rel))
    });
    let total = ranked.len();
    let end = offset.saturating_add(FILE_PAGE_SIZE).min(total);
    let matches = ranked
        .into_iter()
        .skip(offset)
        .take(FILE_PAGE_SIZE)
        .map(|file| {
            let modified_at = file
                .entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|time| time.as_millis().min(i64::MAX as u128) as i64)
                .unwrap_or(0);
            resource_hit(
                "file",
                file.rel.clone(),
                &file.entry.file_name().to_string_lossy(),
                file.rel,
                query,
                modified_at,
            )
        })
        .collect();
    Ok(GlobalSearchPage {
        matches,
        next_cursor: (end < total).then(|| end.to_string()),
    })
}

pub(super) fn search_views_page(
    working_dir: &str,
    query: &str,
    cursor: Option<&str>,
) -> Result<GlobalSearchPage, String> {
    if cursor.is_some_and(|value| value.len() > 1024) {
        return Err("Invalid view search cursor".into());
    }
    let mut views = crate::view::list_views_sync(working_dir)?;
    views.sort_by(|left, right| left.id.cmp(&right.id));
    let mut matches = Vec::new();
    for view in views {
        if cursor.is_some_and(|after| view.id.as_str() <= after) {
            continue;
        }
        let fields = [
            &view.name,
            &view.display_path,
            &view.id,
            &view.package_rel_path,
        ];
        if !fields.iter().any(|value| excerpt(value, query).is_some()) {
            continue;
        }
        // Prefer the matching path/identifier on the second line, otherwise
        // show the display path used by the View explorer.
        let detail = fields[1..]
            .iter()
            .find(|value| excerpt(value, query).is_some())
            .copied()
            .unwrap_or(&view.display_path)
            .clone();
        let mut hit = resource_hit(
            "view",
            view.id.clone(),
            &view.name,
            view.manifest_path,
            query,
            view.updated_at,
        );
        hit.excerpt = detail;
        matches.push(hit);
        if matches.len() >= 20 {
            return Ok(GlobalSearchPage {
                matches,
                next_cursor: Some(view.id),
            });
        }
    }
    Ok(GlobalSearchPage {
        matches,
        next_cursor: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_search_matches_names_paths_and_ids_and_pages_without_duplicates() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_str().unwrap();
        for i in 0..23 {
            crate::view::create_view_sync(
                root,
                crate::view::ViewCreateRequest {
                    id: format!("search-panel-{i:03}"),
                    name: Some(format!("角色 Character {i}")),
                    display_path: Some(format!("角色工具/Panel {i}")),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let first = search_views_page(root, "character", None).unwrap();
        assert_eq!(first.matches.len(), 20);
        let second = search_views_page(root, "character", first.next_cursor.as_deref()).unwrap();
        assert_eq!(second.matches.len(), 3);
        assert!(second.next_cursor.is_none());
        let mut ids = std::collections::HashSet::new();
        for hit in first.matches.into_iter().chain(second.matches) {
            assert_eq!(hit.kind, "view");
            assert!(hit.modified_at > 0);
            assert!(Path::new(hit.path.as_deref().unwrap()).is_file());
            assert!(ids.insert(hit.id));
        }
        let path = search_views_page(root, "角色工具", None).unwrap();
        assert_eq!(path.matches.len(), 20);
        assert!(path
            .matches
            .iter()
            .all(|hit| hit.excerpt.contains("角色工具")));
        let id = search_views_page(root, "search-panel-022", None).unwrap();
        assert_eq!(id.matches.len(), 1);
        assert_eq!(id.matches[0].excerpt, "search-panel-022");
    }

    #[test]
    fn file_search_pages_through_paths_without_duplicates_and_skips_generated_folders() {
        let temp = tempfile::tempdir().unwrap();
        for folder in [
            "a",
            "a.cs",
            "Assets/Scripts",
            "node_modules",
            ".git",
            "Library",
            "target",
        ] {
            std::fs::create_dir_all(temp.path().join(folder)).unwrap();
            for i in 0..45 {
                std::fs::write(
                    temp.path()
                        .join(folder)
                        .join(format!("Character-中文-{i:03}.cs")),
                    "",
                )
                .unwrap();
            }
        }
        let mut cursor = None;
        let mut paths = std::collections::HashSet::new();
        loop {
            let page = search_files_page(temp.path(), "character-中文", cursor.as_deref()).unwrap();
            assert!(page.matches.len() <= FILE_PAGE_SIZE);
            for hit in page.matches {
                assert_eq!(hit.kind, "file");
                assert!(hit.modified_at > 0);
                assert!(paths.insert(hit.path.unwrap()));
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(paths.len(), 135);
        let page = search_files_page(temp.path(), "assets/scripts/character", None).unwrap();
        assert_eq!(page.matches.len(), FILE_PAGE_SIZE);
        assert!(search_files_page(temp.path(), "character.*", None)
            .unwrap()
            .matches
            .is_empty());
        assert!(search_files_page(temp.path(), "x", Some("../outside")).is_err());
    }

    #[test]
    fn file_search_returns_a_late_match_on_the_first_page() {
        let temp = tempfile::tempdir().unwrap();
        for i in 0..300 {
            std::fs::write(temp.path().join(format!("a-{i:03}.txt")), "").unwrap();
        }
        std::fs::write(temp.path().join("z-needle.txt"), "").unwrap();
        let first = search_files_page(temp.path(), "needle", None).unwrap();
        assert_eq!(first.matches.len(), 1);
        assert_eq!(first.matches[0].id, "z-needle.txt");
        assert!(first.next_cursor.is_none());
    }

    #[test]
    fn file_search_ranks_file_name_matches_before_path_matches_and_skips_meta() {
        let temp = tempfile::tempdir().unwrap();
        for path in [
            "Assets/Characters/C003_Kalan/Actions/Configuration/Foo.asset",
            "Assets/Characters/C003_Kalan/Actions/Configuration.meta",
            "Assets/Characters/Elora/EloraWithKalan.asset",
            "Assets/Characters/Elora/Elora_KalanActions.asset",
            "Assets/Scripts/KalanController.cs",
            "Assets/Scripts/KalanController.cs.meta",
            "Assets/Zeta/Kalan.unity",
        ] {
            let path = temp.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let page = search_files_page(temp.path(), "kalan", None).unwrap();
        let ids: Vec<&str> = page.matches.iter().map(|hit| hit.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "Assets/Zeta/Kalan.unity",
                "Assets/Scripts/KalanController.cs",
                "Assets/Characters/Elora/Elora_KalanActions.asset",
                "Assets/Characters/Elora/EloraWithKalan.asset",
                "Assets/Characters/C003_Kalan/Actions/Configuration/Foo.asset",
            ]
        );
        assert!(page.next_cursor.is_none());
    }
}
