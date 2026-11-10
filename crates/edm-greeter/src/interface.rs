//! Loading an interface directory (`main.html` + optional `style.css`) and the built-in fallback.
//! std only.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use edm_core::config::{UserDir, INTERFACE_ENTRY};

pub const DEFAULT_HTML: &str = include_str!("../../../interfaces/Hyproman/main.html");
pub const DEFAULT_CSS: &str = include_str!("../../../interfaces/Hyproman/style.css");

const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Put `css` in a `<style>` block just before `</head>` (or at the very top if there is no head).
/// Done as text so the interface loader doesn't depend on Blitz's resource loading.
pub fn inject_css(html: &str, css: &str) -> String {
    let block = format!("<style>\n{css}\n</style>\n");
    // ASCII lowercasing keeps byte offsets identical, so the index is valid for `html`.
    match html.to_ascii_lowercase().find("</head>") {
        Some(i) => format!("{}{}{}", &html[..i], block, &html[i..]),
        None => format!("{block}{html}"),
    }
}

fn read_capped(path: &Path) -> io::Result<String> {
    if fs::metadata(path)?.len() > MAX_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is too large", path.display())));
    }
    fs::read_to_string(path)
}

pub fn load_dir(dir: &Path) -> io::Result<String> {
    let html = read_capped(&dir.join(INTERFACE_ENTRY))?;
    Ok(match read_capped(&dir.join("style.css")) {
        Ok(css) => inject_css(&html, &css),
        Err(_) => html,
    })
}

const WALLPAPER_EXTS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "gif"];
const WALLPAPER_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// `wallpaper=` from the config: absolute, or relative to the interface directory. Only existing
/// regular image files of a sane size are accepted (the value comes from a config file).
pub fn resolve_wallpaper(value: &str, interface_dir: Option<&Path>) -> Option<PathBuf> {
    let p = Path::new(value.trim());
    let full = if p.is_absolute() { p.to_path_buf() } else { interface_dir?.join(p) };
    // Relative values must not climb out of the interface directory.
    if !p.is_absolute() && p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return None;
    }
    let ext = full.extension()?.to_str()?.to_ascii_lowercase();
    if !WALLPAPER_EXTS.contains(&ext.as_str()) {
        return None;
    }
    let meta = fs::metadata(&full).ok()?;
    (meta.is_file() && meta.len() <= WALLPAPER_MAX_BYTES).then_some(full)
}

/// `file://` URL with everything outside `[A-Za-z0-9-._~/]` percent-encoded.
#[allow(dead_code)] // no longer used at startup (the wallpaper is embedded as a data: URI); kept + tested
pub fn file_url(path: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    if !path.is_absolute() {
        return None;
    }
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    Some(out)
}

/// CSS that puts the image behind the page. Appended after the interface's own CSS so it wins
/// over the built-in gradient.
#[allow(dead_code)] // see file_url
pub fn wallpaper_css(path: &Path) -> Option<String> {
    let url = file_url(path)?;
    Some(format!(
        "body, #stage, #bg {{ background-image: url(\"{url}\"); background-size: cover; background-position: center; }}"
    ))
}


// ---- picture inspector: folders from `user_dirs`, indexed as raw bytes ---------------------------

/// Largest picture the inspector will embed (the bytes become a base64 `data:` URI in the DOM).
pub const PICTURE_MAX_BYTES: u64 = 12 * 1024 * 1024;
/// At most this many pictures are indexed (searchable). Only `PICTURE_SHOWN_MAX` of them are drawn.
pub const PICTURE_LIST_MAX: usize = 5000;
/// At most this many list buttons are real DOM nodes at once; the search box narrows the rest down.
pub const PICTURE_SHOWN_MAX: usize = 100;
/// Depth limit for `/*` entries (the folder itself is depth 0).
const PICTURE_DEPTH: usize = 8;
/// Directory entries looked at per scan; a stray `/*` on a huge tree must not stall the login screen.
const PICTURE_SCAN_BUDGET: usize = 50_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub path: PathBuf,
    /// The `user_dirs` label it was found under (e.g. `Pictures`).
    pub label: String,
    /// `Label/path/relative/to/the/folder`.
    pub name: String,
    pub size: u64,
}

/// How much one label has indexed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub label: String,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PictureIndex {
    /// Sorted by name, case-insensitive.
    pub pictures: Vec<Picture>,
    /// One entry per label, in config order (also for labels that found nothing).
    pub stats: Vec<IndexStats>,
    /// Entries that could not be used (bad path, missing folder), already worded for display.
    pub problems: Vec<String>,
    /// The list or the scan budget ran out; more pictures exist than are shown.
    pub truncated: bool,
}

/// Used when the config has no `user_dirs`: `$EDM_PICTURES_DIR`, else `$HOME/Pictures`, everything below.
pub fn default_user_dirs() -> Vec<UserDir> {
    let path = match std::env::var_os("EDM_PICTURES_DIR").filter(|d| !d.is_empty()).and_then(|d| d.into_string().ok()) {
        Some(d) => format!("{}/*", d.trim_end_matches('/')),
        None => "$HOME/Pictures/*".to_string(),
    };
    vec![UserDir { label: "Pictures".into(), path }]
}

/// The real folder behind an entry: `$HOME`, `${HOME}` and a leading `~` are expanded from `home`,
/// and the result must be an absolute path without `..` that is not `/`.
pub fn resolve_user_dir(d: &UserDir, home: Option<&Path>) -> Result<PathBuf, String> {
    let base = d.base();
    let home_rest = ["$HOME", "${HOME}", "~"]
        .iter()
        .find_map(|p| base.strip_prefix(p).filter(|r| r.is_empty() || r.starts_with('/')));
    let full = match home_rest {
        Some(rest) => {
            let home = home.filter(|h| h.is_absolute()).ok_or("$HOME is not set to an absolute path")?;
            home.join(rest.trim_start_matches('/'))
        }
        None => PathBuf::from(base),
    };
    if !full.is_absolute() {
        return Err(format!("{} is not an absolute path", full.display()));
    }
    if full.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("`..` is not allowed".into());
    }
    if full.parent().is_none() {
        return Err("refusing to index the whole file system".into());
    }
    Ok(full)
}

/// Image files below `dir` down to `max_depth` levels (0 = only `dir` itself). Symlinks are not
/// followed. `budget` counts directory entries looked at.
fn collect(root: &Path, dir: &Path, depth: usize, max_depth: usize, budget: &mut usize, out: &mut Vec<(PathBuf, String, u64)>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if depth < max_depth {
                collect(root, &path, depth + 1, max_depth, budget, out);
            }
        } else if ft.is_file() && mime_of(&path).is_some() {
            let Ok(meta) = entry.metadata() else { continue };
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();
            out.push((path, rel, meta.len()));
        }
    }
}

/// Index every `user_dirs` entry. A picture reachable through two entries is listed once.
pub fn index_pictures(dirs: &[UserDir], home: Option<&Path>) -> PictureIndex {
    let mut idx = PictureIndex::default();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for d in dirs {
        let slot = match idx.stats.iter().position(|s| s.label == d.label) {
            Some(i) => i,
            None => {
                idx.stats.push(IndexStats { label: d.label.clone(), ..Default::default() });
                idx.stats.len() - 1
            }
        };
        let root = match resolve_user_dir(d, home) {
            Ok(r) if r.is_dir() => r,
            Ok(r) => {
                idx.problems.push(format!("{}: {} is not a readable folder", d.label, r.display()));
                continue;
            }
            Err(e) => {
                idx.problems.push(format!("{}: {} ({e})", d.label, d.path));
                continue;
            }
        };
        let mut found = Vec::new();
        let mut budget = PICTURE_SCAN_BUDGET;
        collect(&root, &root, 0, if d.recursive() { PICTURE_DEPTH } else { 0 }, &mut budget, &mut found);
        if budget == 0 {
            idx.truncated = true;
        }
        for (path, rel, size) in found {
            if idx.pictures.len() >= PICTURE_LIST_MAX {
                idx.truncated = true;
                break;
            }
            if !seen.insert(path.clone()) {
                continue;
            }
            idx.stats[slot].files += 1;
            idx.stats[slot].bytes += size;
            idx.pictures.push(Picture { path, label: d.label.clone(), name: format!("{}/{rel}", d.label), size });
        }
    }
    idx.pictures.sort_by_key(|a| a.name.to_lowercase());
    idx
}

/// One line for `#inspector-info`: what each label indexed, and what could not be used.
pub fn index_summary(idx: &PictureIndex) -> String {
    let mut parts: Vec<String> = idx
        .stats
        .iter()
        .map(|s| format!("{}: {} picture(s), {}", s.label, s.files, human_size(s.bytes)))
        .collect();
    parts.extend(idx.problems.iter().cloned());
    if idx.truncated {
        parts.push(format!("only the first {PICTURE_LIST_MAX} are indexed"));
    }
    if parts.is_empty() {
        "no picture folders".into()
    } else {
        parts.join(" | ")
    }
}

/// One `filter=` entry as an existing picture file. Absolute paths and `~` / `$HOME` ones are used
/// as they are; a relative one is looked up below each of `bases` (the resolved `user_dirs` folders)
/// and then below `home`, first hit wins. `..` is not allowed, the file must be an image of at most
/// `PICTURE_MAX_BYTES` (it is embedded as a `data:` URI when chosen).
pub fn resolve_filter_entry(entry: &str, bases: &[PathBuf], home: Option<&Path>) -> Result<PathBuf, String> {
    let e = entry.trim();
    if e.is_empty() {
        return Err("empty entry".into());
    }
    let home_rest = ["$HOME", "${HOME}", "~"]
        .iter()
        .find_map(|p| e.strip_prefix(p).filter(|r| r.is_empty() || r.starts_with('/')));
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(rest) = home_rest {
        let h = home.filter(|h| h.is_absolute()).ok_or("$HOME is not set to an absolute path")?;
        candidates.push(h.join(rest.trim_start_matches('/')));
    } else if Path::new(e).is_absolute() {
        candidates.push(PathBuf::from(e));
    } else {
        candidates.extend(bases.iter().map(|b| b.join(e)));
        candidates.extend(home.filter(|h| h.is_absolute()).map(|h| h.join(e)));
    }
    if candidates.iter().any(|c| c.components().any(|c| matches!(c, Component::ParentDir))) {
        return Err("`..` is not allowed".into());
    }
    if candidates.is_empty() {
        return Err("relative path but no picture folder or $HOME to look in".into());
    }
    if mime_of(&candidates[0]).is_none() {
        return Err("not a png/jpg/webp/gif file".into());
    }
    let mut too_big = false;
    for c in &candidates {
        if let Ok(meta) = fs::metadata(c) {
            if !meta.is_file() {
                continue;
            }
            if meta.len() > PICTURE_MAX_BYTES {
                too_big = true;
                continue;
            }
            return Ok(c.clone());
        }
    }
    Err(if too_big { "larger than 12 MB".into() } else { "file not found".into() })
}

/// The `filter=` entries as pictures (in the given order, duplicates dropped) plus one worded
/// problem per entry that could not be used.
pub fn known_pictures(entries: &[String], bases: &[PathBuf], home: Option<&Path>) -> (Vec<Picture>, Vec<String>) {
    let mut pics: Vec<Picture> = Vec::new();
    let mut problems = Vec::new();
    for e in entries {
        match resolve_filter_entry(e, bases, home) {
            Ok(path) if pics.iter().any(|p| p.path == path) => drop(path),
            Ok(path) => {
                let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                pics.push(Picture { path, label: "Filter".into(), name: format!("Filter/{}", e.trim()), size });
            }
            Err(why) => problems.push(format!("filter '{}': {why}", e.trim())),
        }
    }
    (pics, problems)
}

/// Indices (into `pics`) of the pictures matching `query`, at most `max`, plus the total number of
/// matches. Every whitespace-separated word must occur in the name (`Label/path/file.ext`),
/// case-insensitively; an empty query matches everything.
pub fn filter_pictures(pics: &[Picture], query: &str, max: usize) -> (Vec<usize>, usize) {
    let words: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();
    let mut shown = Vec::new();
    let mut total = 0;
    for (i, p) in pics.iter().enumerate() {
        let name = p.name.to_lowercase();
        if words.iter().all(|w| name.contains(w.as_str())) {
            total += 1;
            if shown.len() < max {
                shown.push(i);
            }
        }
    }
    (shown, total)
}

pub fn mime_of(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

pub fn human_size(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f >= K * K {
        format!("{:.1} MB", f / (K * K))
    } else if f >= K {
        format!("{:.0} KB", f / K)
    } else {
        format!("{n} B")
    }
}

pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// The picture's raw bytes as a `data:` URI (no file:// loading, so no network provider needed).
pub fn picture_data_uri(path: &Path) -> io::Result<String> {
    data_uri_capped(path, PICTURE_MAX_BYTES)
}

/// Same as [`picture_data_uri`] with an explicit size limit (the startup `wallpaper=` may be up to
/// `WALLPAPER_MAX_BYTES`, larger than what the inspector embeds).
pub fn data_uri_capped(path: &Path, max_bytes: u64) -> io::Result<String> {
    let mime = mime_of(path).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not an image file"))?;
    let meta = fs::metadata(path)?;
    if !meta.is_file() || meta.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("picture is missing or larger than {} MB", max_bytes / (1024 * 1024)),
        ));
    }
    Ok(format!("data:{mime};base64,{}", base64(&fs::read(path)?)))
}

/// Writes the picture into the page source itself: a `.pic-layer` div with the `data:` URI as an
/// inline style right inside `#bg-pic`, and the default glow/rings hidden. The document then has
/// the picture from its very first frame (no DOM mutation before the first style resolve, which
/// panics in stylo). `None` when the interface has no `#bg-pic`.
pub fn embed_wallpaper(html: &str, data_uri: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase(); // ASCII only: byte offsets stay valid for `html`
    let at = lower.find("id=\"bg-pic\"").or_else(|| lower.find("id='bg-pic'"))?;
    let close = at + lower[at..].find('>')? + 1;
    let layer = format!(
        "<div class=\"pic-layer\" style=\"position:absolute;top:0;left:0;right:0;bottom:0;background-image:url('{data_uri}');background-size:cover;background-position:center;\"></div>"
    );
    let with_layer = format!("{}{}{}", &html[..close], layer, &html[close..]);
    Some(inject_css(&with_layer, "#bg .glow, #bg .ring { display: none; }"))
}

/// The configured `wallpaper=` as a `data:` URI, read at startup. Same transport the inspector uses
/// (no file:// URL, so nothing depends on the net provider's file loading or on URL encoding).
pub fn wallpaper_data_uri(path: &Path) -> io::Result<String> {
    data_uri_capped(path, WALLPAPER_MAX_BYTES)
}

/// Value for the `style` attribute of `#bg`.
pub fn bg_style(data_uri: &str) -> String {
    format!("background-image: url(\"{data_uri}\"); background-size: cover; background-position: center;")
}

pub fn default_html() -> String {
    inject_css(DEFAULT_HTML, DEFAULT_CSS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_goes_before_head_end_any_case() {
        let out = inject_css("<html><HEAD><title>x</title></HEAD><body></body></html>", "a{b:c}");
        assert!(out.contains("<style>\na{b:c}\n</style>\n</HEAD>"));
        assert!(out.starts_with("<html><HEAD>"));
    }

    #[test]
    fn css_without_head_is_prepended() {
        assert!(inject_css("<p>hi</p>", "a{}").starts_with("<style>"));
    }

    #[test]
    fn non_ascii_html_keeps_valid_offsets() {
        let out = inject_css("<head><title>Ş ğ</title></head><body>İ</body>", "x{}");
        assert!(out.contains("Ş ğ") && out.contains("İ"));
    }

    #[test]
    fn built_in_default_has_required_ids() {
        let h = default_html();
        for id in ["username", "password", "login"] {
            assert!(h.contains(&format!("id=\"{id}\"")), "{id}");
        }
        assert!(h.contains("<style>"));
    }

    #[test]
    fn wallpaper_url_and_css() {
        assert_eq!(file_url(Path::new("/a/b c/ü.png")).unwrap(), "file:///a/b%20c/%C3%BC.png");
        assert_eq!(file_url(Path::new("/x\"y)z.png")).unwrap(), "file:///x%22y%29z.png");
        assert!(file_url(Path::new("rel.png")).is_none());
        let css = wallpaper_css(Path::new("/w/a.png")).unwrap();
        assert!(css.contains("url(\"file:///w/a.png\")") && css.contains("cover"));
    }

    #[test]
    fn wallpaper_resolution_rules() {
        let base = std::env::var_os("EDM_TEST_TMP").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let d = base.join(format!("edm-test-wall-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("bg.PNG"), "x").unwrap();
        fs::write(d.join("notes.txt"), "x").unwrap();
        fs::write(d.join("sub/in.jpg"), "x").unwrap();
        // relative to the interface dir, extension case-insensitive
        assert_eq!(resolve_wallpaper("bg.PNG", Some(&d)), Some(d.join("bg.PNG")));
        assert_eq!(resolve_wallpaper(" sub/in.jpg ", Some(&d)), Some(d.join("sub/in.jpg")));
        // absolute works without an interface dir
        assert_eq!(resolve_wallpaper(d.join("bg.PNG").to_str().unwrap(), None), Some(d.join("bg.PNG")));
        // refused: relative without dir, climbing out, wrong type, missing, directory
        assert_eq!(resolve_wallpaper("bg.PNG", None), None);
        assert_eq!(resolve_wallpaper("../bg.PNG", Some(&d.join("sub"))), None);
        assert_eq!(resolve_wallpaper("notes.txt", Some(&d)), None);
        assert_eq!(resolve_wallpaper("nope.png", Some(&d)), None);
        fs::create_dir_all(d.join("dir.png")).unwrap();
        assert_eq!(resolve_wallpaper("dir.png", Some(&d)), None);
    }

    #[test]
    fn load_dir_reads_html_and_css() {
        let base = std::env::var_os("EDM_TEST_TMP").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let d = base.join(format!("edm-test-ui-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        assert!(load_dir(&d).is_err());
        fs::write(d.join("main.html"), "<head></head><p id=\"login\"></p>").unwrap();
        assert!(!load_dir(&d).unwrap().contains("<style>"));
        fs::write(d.join("style.css"), "p{color:red}").unwrap();
        assert!(load_dir(&d).unwrap().contains("p{color:red}"));
    }

    #[test]
    fn wallpaper_is_embedded_in_the_page_source() {
        let page = "<html><head></head><body><div id=\"bg\"><div id=\"bg-pic\"></div><div class=\"glow\"></div></div></body></html>";
        let out = embed_wallpaper(page, "data:image/png;base64,Zm9v").unwrap();
        let layer = out.find("pic-layer").unwrap();
        assert!(out.find("id=\"bg-pic\">").unwrap() < layer && layer < out.find("class=\"glow\"").unwrap());
        assert!(out.contains("url('data:image/png;base64,Zm9v')"));
        assert!(out.contains("#bg .glow, #bg .ring { display: none; }"));
        assert!(embed_wallpaper("<html><body><div id=\"bg\"></div></body></html>", "data:x").is_none());
        // the built-in interface has the container
        assert!(embed_wallpaper(&default_html(), "data:image/png;base64,Zm9v").is_some());
    }

    #[test]
    fn filter_entries_resolve_to_existing_images() {
        let base = std::env::var_os("EDM_TEST_TMP").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let d = base.join(format!("edm-test-filter-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("home/Pictures/sub")).unwrap();
        fs::create_dir_all(d.join("other")).unwrap();
        fs::write(d.join("home/Pictures/sub/a.png"), b"a").unwrap();
        fs::write(d.join("home/Pictures/my wall.JPG"), b"bb").unwrap();
        fs::write(d.join("other/x.webp"), b"x").unwrap();
        fs::write(d.join("other/notes.txt"), b"n").unwrap();
        let home = d.join("home");
        let bases = vec![d.join("home/Pictures")];
        let r = |e: &str| resolve_filter_entry(e, &bases, Some(&home));
        assert_eq!(r("sub/a.png"), Ok(d.join("home/Pictures/sub/a.png")), "relative to a user_dirs folder");
        assert_eq!(r("Pictures/sub/a.png"), Ok(d.join("home/Pictures/sub/a.png")), "relative to $HOME");
        assert_eq!(r("~/Pictures/my wall.JPG"), Ok(d.join("home/Pictures/my wall.JPG")));
        assert_eq!(r("$HOME/Pictures/sub/a.png"), Ok(d.join("home/Pictures/sub/a.png")));
        assert_eq!(r(d.join("other/x.webp").to_str().unwrap()), Ok(d.join("other/x.webp")), "absolute, outside the folders");
        assert_eq!(r("nope.png"), Err("file not found".into()));
        assert_eq!(r("../other/x.webp"), Err("`..` is not allowed".into()));
        assert_eq!(r(d.join("other/notes.txt").to_str().unwrap()), Err("not a png/jpg/webp/gif file".into()));
        assert_eq!(r("sub"), Err("not a png/jpg/webp/gif file".into()));
        assert!(resolve_filter_entry("~/a.png", &[], None).is_err());
        assert!(resolve_filter_entry("a.png", &[], None).is_err(), "relative with nowhere to look");
        let entries: Vec<String> = ["sub/a.png", "missing.png", "Pictures/sub/a.png", "~/Pictures/my wall.JPG"].map(String::from).into();
        let (pics, problems) = known_pictures(&entries, &bases, Some(&home));
        assert_eq!(pics.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["Filter/sub/a.png", "Filter/~/Pictures/my wall.JPG"], "order kept, same file once");
        assert_eq!(pics[1].size, 2);
        assert_eq!(problems, ["filter 'missing.png': file not found"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn search_filters_by_every_word_case_insensitively() {
        let mk = |n: &str| Picture { path: PathBuf::from(format!("/p/{n}")), label: "Pictures".into(), name: format!("Pictures/{n}"), size: 1 };
        let pics = vec![mk("Sunset Beach.jpg"), mk("mountain/Snow.PNG"), mk("beach/night.webp"), mk("ÇAY.png")];
        assert_eq!(filter_pictures(&pics, "", 10), (vec![0, 1, 2, 3], 4));
        assert_eq!(filter_pictures(&pics, "   ", 10).1, 4);
        assert_eq!(filter_pictures(&pics, "BEACH", 10), (vec![0, 2], 2));
        assert_eq!(filter_pictures(&pics, "beach sunset", 10), (vec![0], 1));
        assert_eq!(filter_pictures(&pics, "png", 10), (vec![1, 3], 2));
        assert_eq!(filter_pictures(&pics, "çay", 10), (vec![3], 1));
        assert_eq!(filter_pictures(&pics, "nope", 10), (vec![], 0));
        assert_eq!(filter_pictures(&pics, "", 2), (vec![0, 1], 4), "capped list, true total");
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn picture_scan_and_data_uri() {
        let base = std::env::var_os("EDM_TEST_TMP").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let d = base.join(format!("edm-test-pics-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("trip")).unwrap();
        fs::write(d.join("b.PNG"), b"foo").unwrap();
        fs::write(d.join("a.jpg"), b"x").unwrap();
        fs::write(d.join("trip/c.webp"), b"y").unwrap();
        fs::write(d.join("notes.txt"), b"n").unwrap();
        let rec = UserDir { label: "Pictures".into(), path: format!("{}/*", d.display()) };
        let names: Vec<String> = index_pictures(&[rec], None).pictures.into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["Pictures/a.jpg", "Pictures/b.PNG", "Pictures/trip/c.webp"]);
        assert_eq!(picture_data_uri(&d.join("b.PNG")).unwrap(), "data:image/png;base64,Zm9v");
        assert!(picture_data_uri(&d.join("notes.txt")).is_err());
        // The startup wallpaper goes through the same data: URI path (also for a path with spaces).
        fs::write(d.join("my wall é.PNG"), b"foo").unwrap();
        assert_eq!(wallpaper_data_uri(&d.join("my wall é.PNG")).unwrap(), "data:image/png;base64,Zm9v");
        assert!(data_uri_capped(&d.join("b.PNG"), 2).is_err(), "size cap is enforced");
        let gone = UserDir { label: "P".into(), path: format!("{}/missing/*", d.display()) };
        assert!(index_pictures(&[gone], None).pictures.is_empty());
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2 KB");
    }

    fn tmpdir(name: &str) -> PathBuf {
        let base = std::env::var_os("EDM_TEST_TMP").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let d = base.join(format!("edm-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn ud(label: &str, path: String) -> UserDir {
        UserDir { label: label.into(), path }
    }

    fn names(idx: &PictureIndex) -> Vec<&str> {
        idx.pictures.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn star_means_everything_below_plain_means_that_folder_only() {
        let r = tmpdir("scope");
        for f in ["Pictures/a.png", "Pictures/sha/b.jpg", "Pictures/sha/deep/c.gif", "Pictures/picture/d.webp", "Pictures/picture/sub/e.png"] {
            fs::create_dir_all(r.join(f).parent().unwrap()).unwrap();
            fs::write(r.join(f), b"12345").unwrap();
        }
        let p = r.join("Pictures");
        let all = index_pictures(&[ud("Pictures", format!("{}/*", p.display()))], None);
        assert_eq!(all.pictures.len(), 5);
        // the user's own example: one `*` entry plus single-folder entries
        let only_top = index_pictures(&[ud("Pictures", p.display().to_string())], None);
        assert_eq!(names(&only_top), ["Pictures/a.png"]);
        let picture_only = index_pictures(&[ud("Pictures", format!("{}/picture", p.display()))], None);
        assert_eq!(names(&picture_only), ["Pictures/d.webp"], "no sub-folders without `*`");
        // overlapping entries list a file once; stats count what is listed
        let both = index_pictures(
            &[ud("Pictures", format!("{}/*", p.display())), ud("Pictures", format!("{}/picture", p.display()))],
            None,
        );
        assert_eq!(both.pictures.len(), 5);
        assert_eq!(both.stats, [IndexStats { label: "Pictures".into(), files: 5, bytes: 25 }]);
        assert_eq!(index_summary(&both), "Pictures: 5 picture(s), 25 B");
    }

    #[test]
    fn stats_are_per_label_and_problems_are_reported() {
        let r = tmpdir("stats");
        fs::create_dir_all(r.join("A")).unwrap();
        fs::create_dir_all(r.join("B")).unwrap();
        fs::write(r.join("A/x.png"), vec![0u8; 2048]).unwrap();
        fs::write(r.join("B/y.jpg"), b"1").unwrap();
        fs::write(r.join("B/z.jpg"), b"1").unwrap();
        let idx = index_pictures(
            &[
                ud("Photos", format!("{}/A", r.display())),
                ud("Walls", format!("{}/B", r.display())),
                ud("Gone", format!("{}/nope/*", r.display())),
                ud("Rel", "relative/dir".into()),
            ],
            None,
        );
        let s = |l: &str| idx.stats.iter().find(|s| s.label == l).map(|s| (s.files, s.bytes)).unwrap();
        assert_eq!((s("Photos"), s("Walls"), s("Gone")), ((1, 2048), (2, 2), (0, 0)));
        assert_eq!(idx.problems.len(), 2);
        let line = index_summary(&idx);
        assert!(line.contains("Photos: 1 picture(s), 2 KB") && line.contains("Walls: 2 picture(s), 2 B"), "{line}");
        assert!(line.contains("Gone:") && line.contains("not a readable folder"), "{line}");
        assert_eq!(index_summary(&PictureIndex::default()), "no picture folders");
    }

    #[test]
    fn home_is_expanded_but_must_give_a_real_absolute_path() {
        let r = tmpdir("home");
        fs::create_dir_all(r.join("Pictures")).unwrap();
        fs::write(r.join("Pictures/a.png"), b"x").unwrap();
        for spelling in ["$HOME/Pictures", "${HOME}/Pictures", "~/Pictures"] {
            let d = ud("P", spelling.into());
            assert_eq!(resolve_user_dir(&d, Some(&r)).unwrap(), r.join("Pictures"), "{spelling}");
            assert_eq!(names(&index_pictures(&[d], Some(&r))), ["P/a.png"], "{spelling}");
        }
        assert_eq!(resolve_user_dir(&ud("P", "$HOME/*".into()), Some(&r)).unwrap(), r.join(""));
        // refused: no HOME, relative HOME, other variables, `..`, the root
        assert!(resolve_user_dir(&ud("P", "$HOME/x".into()), None).is_err());
        assert!(resolve_user_dir(&ud("P", "$HOME/x".into()), Some(Path::new("rel"))).is_err());
        assert!(resolve_user_dir(&ud("P", "$HOMEX/x".into()), Some(&r)).is_err());
        assert!(resolve_user_dir(&ud("P", "/a/../b".into()), None).is_err());
        assert!(resolve_user_dir(&ud("P", "/".into()), None).is_err());
        assert!(resolve_user_dir(&ud("P", "$HOME".into()), Some(Path::new("/"))).is_err());
    }

    #[test]
    fn default_dirs_follow_the_environment_rules() {
        // Only the shape is checked (the env var may or may not be set by the caller).
        let d = default_user_dirs();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].label, "Pictures");
        assert!(d[0].recursive());
    }
}
