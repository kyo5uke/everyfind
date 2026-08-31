//! `ef content` engine plumbing (`everyfind::content`): first-contact walk
//! scan, explicit index build, indexed search agreement, freshness after an
//! edit, and path scoping, all against a temp tree. No elevation, no daemon,
//! no real volume: this is exactly the client-side surface `ef content` uses.
//!
//! One #[test] on purpose: the grix store roots itself in `GRIX_DATA_DIR`,
//! and a single test per integration binary makes the env var race-free.

use std::path::{Path, PathBuf};

use everyfind::content::{self, ContentOpts};

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("ef-content-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(root: &Path, rel: &str, content: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn key(out: &content::ContentOutcome) -> Vec<(String, u64, String)> {
    let mut v: Vec<_> = out
        .hits
        .iter()
        .map(|h| (h.rel_path.clone(), h.line_number, h.text.clone()))
        .collect();
    v.sort();
    v
}

#[test]
fn content_search_lifecycle() {
    // Isolate the grix index store from the machine's real cache.
    let data = TempDirGuard::new("data");
    std::env::set_var("GRIX_DATA_DIR", &data.0);
    let tree = TempDirGuard::new("tree");
    let root = &tree.0;

    write(
        root,
        "src/main.rs",
        b"fn main() {\n    needle_alpha();\n}\n",
    );
    write(root, "docs/notes.md", b"needle_alpha in docs\nplain line\n");
    write(root, "sub/deep/hit.txt", b"needle_alpha deep\n");
    write(root, "bin.dat", b"\x00\x01needle_alpha\x00"); // binary: excluded
    let opts = ContentOpts {
        case_insensitive: true,
        scopes: Vec::new(),
    };

    // First contact: no index anywhere -> walk scan answers, caller is told
    // to build. Binary file must not appear.
    let first = content::search(root, "needle_alpha", &opts).unwrap();
    assert!(first.needs_index, "no index yet -> needs_index");
    let first_hits = key(&first);
    assert_eq!(first_hits.len(), 3, "hits: {first_hits:?}");
    assert!(first_hits.iter().all(|(p, _, _)| p != "bin.dat"));

    // Build the index (what the detached `ef content-index` child runs).
    content::build_index(root).unwrap();

    // Indexed search agrees with the walk exactly.
    let indexed = content::search(root, "needle_alpha", &opts).unwrap();
    assert!(!indexed.needs_index, "index must be picked up");
    assert_eq!(key(&indexed), first_hits, "index vs walk diverged");

    // Anchoring in a subdirectory finds the ancestor root's index and still
    // searches the whole tree (grix semantics).
    let from_sub = content::search(&root.join("sub"), "needle_alpha", &opts).unwrap();
    assert!(!from_sub.needs_index);
    assert_eq!(key(&from_sub), first_hits);

    // Freshness: a file added after the build is found on the next search
    // (the pre-search incremental refresh; overlay path, base untouched).
    write(root, "src/later.rs", b"const L: &str = \"needle_alpha\";\n");
    let refreshed = content::search(root, "needle_alpha", &opts).unwrap();
    assert!(!refreshed.needs_index);
    assert_eq!(refreshed.hits.len(), 4, "new file must be picked up");
    assert!(refreshed
        .hits
        .iter()
        .any(|h| h.rel_path == "src/later.rs" && h.line_number == 1));

    // Path scoping: restrict to src/ only.
    let scoped = content::search(
        root,
        "needle_alpha",
        &ContentOpts {
            case_insensitive: true,
            scopes: vec![root.join("src")],
        },
    )
    .unwrap();
    assert!(scoped.hits.iter().all(|h| h.rel_path.starts_with("src/")));
    assert_eq!(scoped.hits.len(), 2);

    // Case flags behave like grep: insensitive finds the upper-cased query,
    // sensitive does not.
    let upper_ci = content::search(root, "NEEDLE_ALPHA", &opts).unwrap();
    assert_eq!(upper_ci.hits.len(), 4);
    let upper_cs = content::search(
        root,
        "NEEDLE_ALPHA",
        &ContentOpts {
            case_insensitive: false,
            scopes: Vec::new(),
        },
    )
    .unwrap();
    assert!(upper_cs.hits.is_empty());

    // Regex (not just literals) flows through to the grix engine.
    let re = content::search(root, r"needle_\w+\(\)", &opts).unwrap();
    assert_eq!(re.hits.len(), 1);
    assert_eq!(re.hits[0].rel_path, "src/main.rs");
}
