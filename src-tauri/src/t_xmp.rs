//! XMP Sidecar Metadata Support (Read-Only Import).
//!
//! Handles discovery and namespace-aware, non-destructive parsing of `.xmp` sidecar
//! files (compatible with darktable, Adobe Lightroom / Camera Raw, RapidRAW,
//! Capture One, digiKam, etc.).
//!
//! ## Field Mappings
//!
//! | Lap Field | Canonical Namespace URI | XMP Property / Element | Mapping Rules |
//! | :--- | :--- | :--- | :--- |
//! | `afiles.rating` | `http://ns.adobe.com/xap/1.0/` | `xmp:Rating` (`xap:Rating`) | `0..=5` → `Some(0..=5)`; `""` → `Some(0)`; `-1` → `Some(0)` (and sets reject flag if unflagged); missing → `None` |
//! | `afiles.culling_flag` | `http://ns.adobe.com/xmp/1.0/DynamicMedia/`<br>`http://darktable.sf.net/`<br>`http://ns.adobe.com/xap/1.0/` | `xmpDM:pick`<br>`darktable:rejected`<br>`xmp:Rating="-1"` / `xmp:Label` | `xmpDM:pick="1"` or `xmp:Label="Pick"\|"Select"` → `Some(1)` (Pick)<br>`xmpDM:pick="-1"` or `darktable:rejected="1"` or `xmp:Rating="-1"` or `xmp:Label="Reject"\|"Rejected"` → `Some(2)` (Reject)<br>`xmpDM:pick="0"` or `darktable:rejected="0"` → `Some(0)` (Unflagged)<br>missing → `None` |
//! | `atags` / `afile_tags` | `http://purl.org/dc/elements/1.1/`<br>`http://ns.adobe.com/lightroom/1.0/`<br>`http://www.digikam.org/ns/1.0/` | `dc:subject` (`rdf:Bag`)<br>`lr:hierarchicalSubject` (`rdf:Bag`)<br>`digiKam:TagsList` (`rdf:Seq`) | Deduplicated union preserving order; internal `darktable\|*` system tags excluded; empty container → `Some(vec![])`; missing → `None` |
//! | `afiles.comments` | `http://purl.org/dc/elements/1.1/`<br>`http://ns.adobe.com/exif/1.0/`<br>`http://ns.adobe.com/photoshop/1.0/` | `dc:description` (`rdf:Alt`)<br>`exif:UserComment` (`rdf:Alt`)<br>`photoshop:Caption` | Priority: `dc:description` (`xml:lang="x-default"` preferred) → `exif:UserComment` → `photoshop:Caption`; empty element/attr → `Some("")`; missing → `None` |

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::t_sqlite::open_conn;

// Canonical XMP Namespace URIs
pub const NS_XMP: &str = "http://ns.adobe.com/xap/1.0/";
pub const NS_XMP_DM: &str = "http://ns.adobe.com/xmp/1.0/DynamicMedia/";
pub const NS_DARKTABLE: &str = "http://darktable.sf.net/";
pub const NS_DC: &str = "http://purl.org/dc/elements/1.1/";
pub const NS_LR: &str = "http://ns.adobe.com/lightroom/1.0/";
pub const NS_EXIF: &str = "http://ns.adobe.com/exif/1.0/";
pub const NS_PHOTOSHOP: &str = "http://ns.adobe.com/photoshop/1.0/";
pub const NS_DIGIKAM: &str = "http://www.digikam.org/ns/1.0/";
pub const NS_RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

static XMP_IMPORT_ENABLED: AtomicBool = AtomicBool::new(true);

pub fn is_xmp_import_enabled() -> bool {
    XMP_IMPORT_ENABLED.load(Ordering::Relaxed)
}

pub fn set_xmp_import_enabled(enabled: bool) {
    XMP_IMPORT_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Parsed XMP metadata distinguishing missing properties (`None`) from
/// explicitly empty/cleared properties (`Some(0)`, `Some(vec![])`, `Some("")`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XmpMetadata {
    /// Star rating (`0..=5`).
    /// - `None`: rating property is omitted from the sidecar.
    /// - `Some(0)`: explicitly unrated (`0` or `""`, or `-1` when mapped to reject).
    /// - `Some(1..=5)`: star rating.
    pub rating: Option<i32>,
    /// Culling flag (`0` = unflagged, `1` = pick, `2` = reject).
    /// - `None`: no culling/pick/reject property is present in the sidecar.
    /// - `Some(0)`: explicitly unflagged.
    /// - `Some(1)`: pick.
    /// - `Some(2)`: reject.
    pub culling_flag: Option<i32>,
    /// Keywords / subject tags (`dc:subject`, `lr:hierarchicalSubject`, `digiKam:TagsList`).
    /// - `None`: no tag container element exists in the sidecar.
    /// - `Some(vec![])`: tag container exists in the sidecar but contains no user tags.
    /// - `Some(tags)`: non-empty list of deduplicated user tags.
    pub tags: Option<Vec<String>>,
    /// User comment / caption (`dc:description`, `exif:UserComment`, `photoshop:Caption`).
    /// - `None`: no description/caption property exists in the sidecar.
    /// - `Some("")`: property exists in the sidecar, but is explicitly empty.
    /// - `Some(text)`: non-empty description text.
    pub description: Option<String>,
}

/// Controls how imported XMP metadata merges with existing database records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XmpImportMode {
    /// Used when a new file is first inserted into `afiles`.
    /// Populates all present non-empty fields from the sidecar.
    InitialScan,
    /// Used during folder rescans or `update_file_info` when a file already exists in `afiles`.
    /// Never overwrites local edits in Lap: only populates `rating`, `culling_flag`, or `comments`
    /// if they are currently unset (`0` or empty) in the database, and merges tags additively.
    RescanNonDestructive,
    /// Used when the user explicitly selects "Import from XMP" from the context menu.
    /// Applies all `Some(...)` fields from the sidecar (including `Some(0)` or `Some("")` to clear),
    /// while leaving `None` (missing) fields untouched.
    ManualOverwrite,
}

/// Discovers an existing XMP sidecar for a given image file path.
///
/// Priority order:
/// 1. `<filename>.<ext>.xmp` / `<filename>.<ext>.XMP` (darktable / Exiv2 convention, e.g. `DSC0001.RAF.xmp`)
/// 2. `<stem>.xmp` / `<stem>.XMP` (Lightroom / Camera Raw convention, e.g. `DSC0001.xmp`)
/// 3. Case-insensitive fallback scan in the parent directory for filesystems where
///    mixed-case extensions such as `.Xmp` exist.
pub fn find_xmp_sidecar(image_path: &Path) -> Option<PathBuf> {
    let parent = image_path.parent().unwrap_or(Path::new("."));
    let file_name = image_path.file_name()?.to_str()?;
    let stem = image_path.file_stem()?.to_str()?;

    // 1. Exact checks for <filename>.<ext>.xmp / .XMP (highest priority)
    let candidate_ext_xmp = parent.join(format!("{}.xmp", file_name));
    if candidate_ext_xmp.is_file() {
        return Some(candidate_ext_xmp);
    }
    let candidate_ext_xmp_upper = parent.join(format!("{}.XMP", file_name));
    if candidate_ext_xmp_upper.is_file() {
        return Some(candidate_ext_xmp_upper);
    }

    // 2. Exact checks for <stem>.xmp / .XMP
    let candidate_stem_xmp = parent.join(format!("{}.xmp", stem));
    if candidate_stem_xmp.is_file() {
        return Some(candidate_stem_xmp);
    }
    let candidate_stem_xmp_upper = parent.join(format!("{}.XMP", stem));
    if candidate_stem_xmp_upper.is_file() {
        return Some(candidate_stem_xmp_upper);
    }

    // 3. Case-insensitive directory fallback (e.g. .Xmp on case-sensitive Linux/macOS APFS)
    if let Ok(entries) = fs::read_dir(parent) {
        let target_ext_name = format!("{}.xmp", file_name);
        let target_stem_name = format!("{}.xmp", stem);
        let mut stem_match: Option<PathBuf> = None;

        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.eq_ignore_ascii_case(&target_ext_name) && entry.path().is_file() {
                    return Some(entry.path());
                }
                if stem_match.is_none()
                    && name.eq_ignore_ascii_case(&target_stem_name)
                    && entry.path().is_file()
                {
                    stem_match = Some(entry.path());
                }
            }
        }
        if stem_match.is_some() {
            return stem_match;
        }
    }

    None
}

/// Returns `Some(sidecar_path)` if an XMP sidecar exists and its modification timestamp
/// (in milliseconds since UNIX_EPOCH) is strictly newer than `prev_scan_time_ms`.
pub fn find_newer_xmp_sidecar(image_path: &Path, prev_scan_time_ms: i64) -> Option<PathBuf> {
    let sidecar = find_xmp_sidecar(image_path)?;
    let sidecar_mtime_ms = fs::metadata(&sidecar)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if should_import_xmp_on_rescan(sidecar_mtime_ms, prev_scan_time_ms) {
        Some(sidecar)
    } else {
        None
    }
}

/// Pure helper determining whether a sidecar with `sidecar_mtime_ms` should be
/// re-evaluated during a rescan of a file last scanned at `prev_scan_time_ms`.
#[inline]
pub fn should_import_xmp_on_rescan(sidecar_mtime_ms: i64, prev_scan_time_ms: i64) -> bool {
    sidecar_mtime_ms > 0 && sidecar_mtime_ms > prev_scan_time_ms
}

/// Reads and parses an XMP sidecar file, skipping cloud-only (unhydrated) placeholders.
pub fn read_xmp_file(path: &Path) -> Result<XmpMetadata, String> {
    if let Ok(meta) = fs::metadata(path) {
        if crate::t_utils::is_cloud_only(&meta) {
            return Err(format!("Skipping cloud-only XMP sidecar '{}'", path.display()));
        }
    }
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read XMP file '{}': {}", path.display(), e))?;
    Ok(parse_xmp_str(&content))
}

/// Parses XMP XML content into structured metadata using namespace-aware resolution.
pub fn parse_xmp_str(content: &str) -> XmpMetadata {
    let mut meta = XmpMetadata::default();
    let ns = XmpNamespaces::parse(content);

    let xmp_prefixes = ns.prefixes_for(NS_XMP, &["xmp", "xap"]);
    let xmp_dm_prefixes = ns.prefixes_for(NS_XMP_DM, &["xmpDM"]);
    let dt_prefixes = ns.prefixes_for(NS_DARKTABLE, &["darktable"]);
    let dc_prefixes = ns.prefixes_for(NS_DC, &["dc"]);
    let lr_prefixes = ns.prefixes_for(NS_LR, &["lr"]);
    let digikam_prefixes = ns.prefixes_for(NS_DIGIKAM, &["digiKam"]);
    let exif_prefixes = ns.prefixes_for(NS_EXIF, &["exif"]);
    let photoshop_prefixes = ns.prefixes_for(NS_PHOTOSHOP, &["photoshop"]);
    let rdf_prefixes = ns.prefixes_for(NS_RDF, &["rdf"]);

    // 1. Rating (xmp:Rating / xap:Rating)
    let mut rating_rejected = false;
    if let Some(rating_val) = extract_ns_attribute(content, &xmp_prefixes, "Rating")
        .or_else(|| extract_ns_tag_value(content, &xmp_prefixes, "Rating"))
    {
        let trimmed = rating_val.trim();
        if trimmed.is_empty() {
            meta.rating = Some(0);
        } else if let Ok(num) = trimmed.parse::<i32>() {
            if num < 0 {
                rating_rejected = true;
                meta.rating = Some(0);
            } else {
                meta.rating = Some(num.clamp(0, 5));
            }
        }
    }

    // 2. Culling / Pick / Reject flags
    // Priority: explicit xmpDM:pick -> darktable:rejected -> xmp:Label (Pick/Reject) -> xmp:Rating="-1"
    if let Some(pick_val) = extract_ns_attribute(content, &xmp_dm_prefixes, "pick")
        .or_else(|| extract_ns_tag_value(content, &xmp_dm_prefixes, "pick"))
    {
        let trimmed = pick_val.trim();
        if trimmed.is_empty() || trimmed == "0" {
            meta.culling_flag = Some(0);
        } else if let Ok(num) = trimmed.parse::<i32>() {
            if num > 0 {
                meta.culling_flag = Some(1);
            } else if num < 0 {
                meta.culling_flag = Some(2);
            }
        }
    }

    if let Some(rej_val) = extract_ns_attribute(content, &dt_prefixes, "rejected")
        .or_else(|| extract_ns_tag_value(content, &dt_prefixes, "rejected"))
    {
        match rej_val.trim() {
            "1" | "true" => meta.culling_flag = Some(2),
            "0" | "false" | "" => {
                if meta.culling_flag.is_none() {
                    meta.culling_flag = Some(0);
                }
            }
            _ => {}
        }
    }

    if let Some(label_val) = extract_ns_attribute(content, &xmp_prefixes, "Label")
        .or_else(|| extract_ns_tag_value(content, &xmp_prefixes, "Label"))
    {
        let trimmed = label_val.trim();
        if trimmed.eq_ignore_ascii_case("Reject") || trimmed.eq_ignore_ascii_case("Rejected") {
            meta.culling_flag = Some(2);
        } else if trimmed.eq_ignore_ascii_case("Pick") || trimmed.eq_ignore_ascii_case("Select") {
            if meta.culling_flag != Some(2) {
                meta.culling_flag = Some(1);
            }
        }
    }

    if rating_rejected && meta.culling_flag.unwrap_or(0) == 0 {
        meta.culling_flag = Some(2);
    }

    // 3. Keywords / Tags (dc:subject, lr:hierarchicalSubject, digiKam:TagsList)
    let mut has_tag_container = false;
    let mut tags_set = HashSet::new();
    let mut tags_list = Vec::new();

    for block in [
        extract_ns_tag_block(content, &dc_prefixes, "subject"),
        extract_ns_tag_block(content, &lr_prefixes, "hierarchicalSubject"),
        extract_ns_tag_block(content, &digikam_prefixes, "TagsList"),
    ]
    .into_iter()
    .flatten()
    {
        has_tag_container = true;
        for raw_tag in extract_li_items_ns(&block, &rdf_prefixes) {
            let clean = xml_unescape(raw_tag.trim());
            if clean.is_empty() || is_internal_system_tag(&clean) {
                continue;
            }
            if tags_set.insert(clean.to_lowercase()) {
                tags_list.push(clean);
            }
        }
    }

    if has_tag_container {
        meta.tags = Some(tags_list);
    }

    // 4. Comments / Description (dc:description -> exif:UserComment -> photoshop:Caption)
    if let Some(desc) = extract_alt_or_tag_text(content, &dc_prefixes, "description", &rdf_prefixes)
        .or_else(|| extract_alt_or_tag_text(content, &exif_prefixes, "UserComment", &rdf_prefixes))
        .or_else(|| {
            extract_ns_attribute(content, &photoshop_prefixes, "Caption")
                .or_else(|| extract_ns_tag_value(content, &photoshop_prefixes, "Caption"))
                .map(|s| xml_unescape(s.trim()))
        })
    {
        meta.description = Some(desc);
    }

    meta
}

/// Returns `true` for internal darktable / host system tags that should not be imported as user tags.
fn is_internal_system_tag(tag: &str) -> bool {
    let lower = tag.trim().to_ascii_lowercase();
    lower.starts_with("darktable|") || lower == "darktable"
}

// ---------------------------------------------------------------------------
// Database Synchronization Operations
// ---------------------------------------------------------------------------

/// Imports XMP metadata into the database for a specific file using the provided mode.
pub fn import_xmp_for_file_with_conn(
    file_id: i64,
    file_path: &str,
    conn: &Connection,
    mode: XmpImportMode,
) -> Result<bool, String> {
    let Some(sidecar_path) = find_xmp_sidecar(Path::new(file_path)) else {
        return Ok(false);
    };

    let meta = read_xmp_file(&sidecar_path)?;
    apply_xmp_metadata_to_db(file_id, &meta, conn, mode)
}

/// Applies parsed `XmpMetadata` to a file record in SQLite according to `mode`.
pub fn apply_xmp_metadata_to_db(
    file_id: i64,
    meta: &XmpMetadata,
    conn: &Connection,
    mode: XmpImportMode,
) -> Result<bool, String> {
    let (db_rating, db_culling_flag, db_comments): (i32, i32, Option<String>) = conn
        .query_row(
            "SELECT COALESCE(rating, 0), COALESCE(culling_flag, 0), comments FROM afiles WHERE id = ?1",
            params![file_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| format!("Failed to read existing file state for id {}: {}", file_id, e))?;

    // 1. Rating
    if let Some(rating) = meta.rating {
        let clamped = rating.clamp(0, 5);
        let should_update = match mode {
            XmpImportMode::InitialScan => clamped > 0,
            XmpImportMode::RescanNonDestructive => db_rating == 0 && clamped > 0,
            XmpImportMode::ManualOverwrite => clamped != db_rating,
        };
        if should_update {
            conn.execute(
                "UPDATE afiles SET rating = ?1 WHERE id = ?2",
                params![clamped, file_id],
            )
            .map_err(|e| e.to_string())?;
        }
    }

    // 2. Culling flag
    if let Some(flag) = meta.culling_flag {
        let clamped = flag.clamp(0, 2);
        let should_update = match mode {
            XmpImportMode::InitialScan => clamped > 0,
            XmpImportMode::RescanNonDestructive => db_culling_flag == 0 && clamped > 0,
            XmpImportMode::ManualOverwrite => clamped != db_culling_flag,
        };
        if should_update {
            conn.execute(
                "UPDATE afiles SET culling_flag = ?1 WHERE id = ?2",
                params![clamped, file_id],
            )
            .map_err(|e| e.to_string())?;
        }
    }

    // 3. Comments / description
    if let Some(ref desc) = meta.description {
        let trimmed = desc.trim();
        let db_comment_empty = db_comments
            .as_deref()
            .is_none_or(|c| c.trim().is_empty());

        let should_update = match mode {
            XmpImportMode::InitialScan => !trimmed.is_empty(),
            XmpImportMode::RescanNonDestructive => db_comment_empty && !trimmed.is_empty(),
            XmpImportMode::ManualOverwrite => true,
        };
        if should_update {
            let new_val: Option<&str> = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            };
            conn.execute(
                "UPDATE afiles SET comments = ?1 WHERE id = ?2",
                params![new_val, file_id],
            )
            .map_err(|e| e.to_string())?;
        }
    }

    // 4. Keywords / Tags
    if let Some(ref tags) = meta.tags {
        if tags.is_empty() {
            if mode == XmpImportMode::ManualOverwrite {
                conn.execute("DELETE FROM afile_tags WHERE file_id = ?1", params![file_id])
                    .map_err(|e| e.to_string())?;
                conn.execute("UPDATE afiles SET has_tags = 0 WHERE id = ?1", params![file_id])
                    .map_err(|e| e.to_string())?;
            }
        } else {
            let default_group_id: i64 = conn
                .query_row(
                    "SELECT id FROM atag_groups WHERE is_default = 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(1);

            let mut added_any = false;
            for tag in tags {
                let trimmed = tag.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let tag_id: i64 = match conn.query_row(
                    "SELECT id FROM atags WHERE name = ?1 COLLATE NOCASE",
                    params![trimmed],
                    |r| r.get(0),
                ) {
                    Ok(id) => id,
                    Err(_) => {
                        conn.execute(
                            "INSERT INTO atags (name, group_id) VALUES (?1, ?2)",
                            params![trimmed, default_group_id],
                        )
                        .map_err(|e| e.to_string())?;
                        conn.last_insert_rowid()
                    }
                };

                conn.execute(
                    "INSERT OR IGNORE INTO afile_tags (file_id, tag_id) VALUES (?1, ?2)",
                    params![file_id, tag_id],
                )
                .map_err(|e| e.to_string())?;
                added_any = true;
            }

            if added_any {
                conn.execute(
                    "UPDATE afiles SET has_tags = 1 WHERE id = ?1",
                    params![file_id],
                )
                .map_err(|e| e.to_string())?;
            }
        }
    }

    Ok(true)
}

/// Imports XMP metadata into the database for a file using a fresh connection.
pub fn import_xmp_for_file_with_mode(
    file_id: i64,
    file_path: &str,
    mode: XmpImportMode,
) -> Result<bool, String> {
    let conn = open_conn()?;
    import_xmp_for_file_with_conn(file_id, file_path, &conn, mode)
}

// ---------------------------------------------------------------------------
// Namespace-Aware XML Parsing Helpers
// ---------------------------------------------------------------------------

struct XmpNamespaces {
    prefix_to_uri: HashMap<String, String>,
}

impl XmpNamespaces {
    fn parse(xml: &str) -> Self {
        let mut prefix_to_uri = HashMap::new();
        let mut rest = xml;
        while let Some(idx) = rest.find("xmlns:") {
            let after_xmlns = &rest[idx + "xmlns:".len()..];
            let Some(eq_idx) = after_xmlns.find('=') else {
                break;
            };
            let prefix = after_xmlns[..eq_idx].trim();
            let after_eq = after_xmlns[eq_idx + 1..].trim_start();
            let Some(quote_char) = after_eq.chars().next() else {
                break;
            };
            if quote_char != '"' && quote_char != '\'' {
                rest = after_eq;
                continue;
            }
            let after_quote = &after_eq[quote_char.len_utf8()..];
            let Some(end_quote) = after_quote.find(quote_char) else {
                break;
            };
            let uri = after_quote[..end_quote].trim();
            if !prefix.is_empty()
                && prefix
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                prefix_to_uri.insert(prefix.to_string(), uri.to_string());
            }
            rest = &after_quote[end_quote + quote_char.len_utf8()..];
        }
        Self { prefix_to_uri }
    }

    fn prefixes_for(&self, target_uri: &str, fallbacks: &[&str]) -> Vec<String> {
        let mut result = Vec::new();
        for (prefix, uri) in &self.prefix_to_uri {
            if uri == target_uri && !result.contains(prefix) {
                result.push(prefix.clone());
            }
        }
        for &fb in fallbacks {
            let fb_str = fb.to_string();
            // Include fallback prefix if it is not bound to a different namespace URI
            if !result.contains(&fb_str)
                && self
                    .prefix_to_uri
                    .get(&fb_str)
                    .is_none_or(|uri| uri == target_uri)
            {
                result.push(fb_str);
            }
        }
        result
    }
}

fn xml_unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn extract_ns_attribute(xml: &str, prefixes: &[String], local_name: &str) -> Option<String> {
    for prefix in prefixes {
        let qname = format!("{}:{}", prefix, local_name);
        if let Some(val) = extract_attribute(xml, &qname) {
            return Some(val);
        }
    }
    None
}

fn extract_ns_tag_value(xml: &str, prefixes: &[String], local_name: &str) -> Option<String> {
    for prefix in prefixes {
        let qname = format!("{}:{}", prefix, local_name);
        if let Some(val) = extract_tag_value(xml, &qname) {
            return Some(val);
        }
    }
    None
}

fn extract_ns_tag_block(xml: &str, prefixes: &[String], local_name: &str) -> Option<String> {
    for prefix in prefixes {
        let qname = format!("{}:{}", prefix, local_name);
        if let Some(block) = extract_tag_block(xml, &qname) {
            return Some(block);
        }
    }
    None
}

fn extract_attribute(xml: &str, attr_name: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("{}={}", attr_name, quote);
        let mut search_from = 0;
        while let Some(rel_idx) = xml[search_from..].find(&needle) {
            let idx = search_from + rel_idx;
            // Ensure attribute name is preceded by whitespace or tag start
            let valid_boundary = idx == 0
                || xml[..idx]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_whitespace());
            let val_start = idx + needle.len();
            if valid_boundary {
                let end_idx = xml[val_start..].find(quote)? + val_start;
                return Some(xml[val_start..end_idx].to_string());
            }
            search_from = val_start;
        }
    }
    None
}

fn extract_tag_value(xml: &str, tag_name: &str) -> Option<String> {
    let block = extract_tag_block(xml, tag_name)?;
    // If self-closing `<tag.../>`, block is empty string
    if block.is_empty() {
        return Some(String::new());
    }
    // Extract inner content between first `>` and closing `</tag_name>`
    let open_end = block.find('>')? + 1;
    let close_tag = format!("</{}>", tag_name);
    let close_start = block.rfind(&close_tag)?;
    if open_end <= close_start {
        Some(block[open_end..close_start].to_string())
    } else {
        Some(String::new())
    }
}

fn extract_tag_block(xml: &str, tag_name: &str) -> Option<String> {
    let open_prefix = format!("<{}", tag_name);
    let close_tag = format!("</{}>", tag_name);
    let mut search_from = 0;

    while let Some(rel_idx) = xml[search_from..].find(&open_prefix) {
        let open_start = search_from + rel_idx;
        let after_name = open_start + open_prefix.len();
        let next_char = xml[after_name..].chars().next();
        if !next_char.is_some_and(|c| c == '>' || c == '/' || c.is_whitespace()) {
            search_from = after_name;
            continue;
        }

        let tag_end_rel = xml[after_name..].find('>')?;
        let tag_end = after_name + tag_end_rel;
        // Handle self-closing tag: `<dc:subject/>` or `<dc:description />`
        if xml[open_start..tag_end].trim_end().ends_with('/') {
            return Some(String::new());
        }

        let close_start = xml[tag_end + 1..].find(&close_tag)? + tag_end + 1;
        let full_end = close_start + close_tag.len();
        return Some(xml[open_start..full_end].to_string());
    }
    None
}

fn extract_li_items_ns(block: &str, rdf_prefixes: &[String]) -> Vec<String> {
    for prefix in rdf_prefixes {
        let li_tag = format!("{}:li", prefix);
        let items = extract_elements_by_name(block, &li_tag);
        if !items.is_empty() {
            return items.into_iter().map(|(_, text)| text).collect();
        }
    }
    Vec::new()
}

/// Extracts `(opening_tag_attributes, inner_text)` for all occurrences of `<tag_name ...>...</tag_name>`
/// or self-closing `<tag_name .../>`.
fn extract_elements_by_name(block: &str, tag_name: &str) -> Vec<(String, String)> {
    let mut items = Vec::new();
    let open_prefix = format!("<{}", tag_name);
    let close_tag = format!("</{}>", tag_name);
    let mut rest = block;

    while let Some(open_idx) = rest.find(&open_prefix) {
        let after_name = &rest[open_idx + open_prefix.len()..];
        let next_char = after_name.chars().next();
        if !next_char.is_some_and(|c| c == '>' || c == '/' || c.is_whitespace()) {
            rest = after_name;
            continue;
        }
        let Some(tag_end_rel) = after_name.find('>') else {
            break;
        };
        let attrs = &after_name[..tag_end_rel];
        if attrs.trim_end().ends_with('/') {
            // Self-closing `<rdf:li .../>`
            let clean_attrs = attrs.trim_end().trim_end_matches('/').to_string();
            items.push((clean_attrs, String::new()));
            rest = &after_name[tag_end_rel + 1..];
            continue;
        }

        let content_start = open_idx + open_prefix.len() + tag_end_rel + 1;
        let Some(close_rel) = rest[content_start..].find(&close_tag) else {
            break;
        };
        let item_text = &rest[content_start..content_start + close_rel];
        items.push((attrs.to_string(), item_text.to_string()));
        rest = &rest[content_start + close_rel + close_tag.len()..];
    }
    items
}

/// Extracts text from an `rdf:Alt` container (preferring `xml:lang="x-default"`),
/// direct tag text, or an attribute on `rdf:Description`.
/// Preserves `Some(String::new())` when the property is explicitly present but empty.
fn extract_alt_or_tag_text(
    xml: &str,
    prefixes: &[String],
    local_name: &str,
    rdf_prefixes: &[String],
) -> Option<String> {
    if let Some(attr_val) = extract_ns_attribute(xml, prefixes, local_name) {
        return Some(xml_unescape(attr_val.trim()));
    }

    let block = extract_ns_tag_block(xml, prefixes, local_name)?;
    if block.is_empty() {
        // Self-closing tag such as `<dc:description/>`
        return Some(String::new());
    }

    for rdf_prefix in rdf_prefixes {
        let li_tag = format!("{}:li", rdf_prefix);
        let li_items = extract_elements_by_name(&block, &li_tag);
        if !li_items.is_empty() {
            // Prefer xml:lang="x-default" if present
            let chosen = li_items
                .iter()
                .find(|(attrs, _)| attrs.contains("x-default"))
                .or_else(|| li_items.first())
                .map(|(_, text)| xml_unescape(text.trim()))
                .unwrap_or_default();
            return Some(chosen);
        }
    }

    // No <rdf:li> items inside the block: check direct inner text (ignoring nested empty rdf:Alt)
    if let Some(val) = extract_ns_tag_value(&block, prefixes, local_name) {
        let trimmed = val.trim();
        if trimmed.starts_with('<') {
            // Contains empty container like `<rdf:Alt/>` or `<rdf:Alt></rdf:Alt>`
            return Some(String::new());
        }
        return Some(xml_unescape(trimmed));
    }

    Some(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn create_temp_test_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("lap_xmp_test_{}_{}", label, nanos));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn setup_in_memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE afiles (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                rating INTEGER NOT NULL DEFAULT 0,
                culling_flag INTEGER NOT NULL DEFAULT 0,
                comments TEXT,
                has_tags INTEGER DEFAULT 0
            );
            CREATE TABLE atag_groups (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                is_default INTEGER DEFAULT 0,
                sort_order INTEGER DEFAULT 0
            );
            CREATE TABLE atags (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                group_id INTEGER NOT NULL
            );
            CREATE TABLE afile_tags (
                file_id INTEGER NOT NULL,
                tag_id INTEGER NOT NULL,
                UNIQUE(file_id, tag_id)
            );
            INSERT INTO atag_groups (id, name, is_default, sort_order) VALUES (1, 'Default', 1, 0);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_sidecar_naming_conventions() {
        let dir = create_temp_test_dir("naming");
        let raw_path = dir.join("DSC0042.RAF");
        fs::write(&raw_path, b"dummy raw").unwrap();

        // 1. No sidecar initially
        assert_eq!(find_xmp_sidecar(&raw_path), None);

        // 2. <stem>.xmp convention (Lightroom / Camera Raw)
        let stem_xmp = dir.join("DSC0042.xmp");
        fs::write(&stem_xmp, b"<x:xmpmeta/>").unwrap();
        assert_eq!(find_xmp_sidecar(&raw_path), Some(stem_xmp.clone()));

        // 3. <filename>.<ext>.xmp convention (darktable) takes priority when both exist
        let ext_xmp = dir.join("DSC0042.RAF.xmp");
        fs::write(&ext_xmp, b"<x:xmpmeta/>").unwrap();
        assert_eq!(find_xmp_sidecar(&raw_path), Some(ext_xmp.clone()));

        // 4. Uppercase .XMP extension
        fs::remove_file(&stem_xmp).unwrap();
        fs::remove_file(&ext_xmp).unwrap();
        let upper_xmp = dir.join("DSC0042.XMP");
        fs::write(&upper_xmp, b"<x:xmpmeta/>").unwrap();
        assert!(find_xmp_sidecar(&raw_path).is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_hierarchical_tags_and_internal_filtering() {
        let xmp = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="XMP Core 4.4.0-Exiv2">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:lr="http://ns.adobe.com/lightroom/1.0/"
    xmp:Rating="4">
   <dc:subject>
    <rdf:Bag>
     <rdf:li>People</rdf:li>
     <rdf:li>Portrait &amp; Studio</rdf:li>
    </rdf:Bag>
   </dc:subject>
   <lr:hierarchicalSubject>
    <rdf:Bag>
     <rdf:li>People</rdf:li>
     <rdf:li>People|Edith</rdf:li>
     <rdf:li>darktable|format|raf</rdf:li>
     <rdf:li>darktable|exported</rdf:li>
    </rdf:Bag>
   </lr:hierarchicalSubject>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        let meta = parse_xmp_str(xmp);
        assert_eq!(meta.rating, Some(4));
        assert_eq!(meta.culling_flag, None);
        assert_eq!(
            meta.tags,
            Some(vec![
                "People".to_string(),
                "Portrait & Studio".to_string(),
                "People|Edith".to_string(),
            ])
        );
    }

    #[test]
    fn test_missing_vs_explicitly_empty_values() {
        // Case A: All metadata properties omitted from XMP
        let missing_xmp = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/">
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        let missing_meta = parse_xmp_str(missing_xmp);
        assert_eq!(missing_meta.rating, None);
        assert_eq!(missing_meta.culling_flag, None);
        assert_eq!(missing_meta.tags, None);
        assert_eq!(missing_meta.description, None);

        // Case B: Properties explicitly present but empty / zeroed
        let empty_xmp = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:xmpDM="http://ns.adobe.com/xmp/1.0/DynamicMedia/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmp:Rating="0"
    xmpDM:pick="0">
   <dc:subject>
    <rdf:Bag/>
   </dc:subject>
   <dc:description>
    <rdf:Alt>
     <rdf:li xml:lang="x-default"></rdf:li>
    </rdf:Alt>
   </dc:description>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        let empty_meta = parse_xmp_str(empty_xmp);
        assert_eq!(empty_meta.rating, Some(0));
        assert_eq!(empty_meta.culling_flag, Some(0));
        assert_eq!(empty_meta.tags, Some(vec![]));
        assert_eq!(empty_meta.description, Some(String::new()));
    }

    #[test]
    fn test_namespace_prefix_resolution_and_culling_flags() {
        // Custom prefixes bound to canonical Adobe/Darktable URIs
        let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:adobeXmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dt="http://darktable.sf.net/"
    xmlns:dcore="http://purl.org/dc/elements/1.1/"
    adobeXmp:Rating="3">
   <dt:rejected>1</dt:rejected>
   <dcore:description>
    <rdf:Alt>
     <rdf:li xml:lang="fr">Version française</rdf:li>
     <rdf:li xml:lang="x-default">Default English caption</rdf:li>
    </rdf:Alt>
   </dcore:description>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        let meta = parse_xmp_str(xmp);
        assert_eq!(meta.rating, Some(3));
        assert_eq!(meta.culling_flag, Some(2)); // Rejected via dt:rejected
        assert_eq!(
            meta.description.as_deref(),
            Some("Default English caption")
        );
    }

    #[test]
    fn test_rescan_preserves_local_edits_in_lap() {
        let conn = setup_in_memory_db();
        conn.execute(
            "INSERT INTO afiles (id, name, rating, culling_flag, comments, has_tags) VALUES (1, 'DSC0001.RAF', 0, 0, NULL, 0)",
            [],
        )
        .unwrap();

        // 1. Initial scan imports metadata from XMP
        let initial_xmp = XmpMetadata {
            rating: Some(2),
            culling_flag: Some(1), // Pick
            tags: Some(vec!["Landscape".to_string()]),
            description: Some("Initial XMP caption".to_string()),
        };
        apply_xmp_metadata_to_db(1, &initial_xmp, &conn, XmpImportMode::InitialScan).unwrap();

        let (rating, flag, comments): (i32, i32, Option<String>) = conn
            .query_row(
                "SELECT rating, culling_flag, comments FROM afiles WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(rating, 2);
        assert_eq!(flag, 1);
        assert_eq!(comments.as_deref(), Some("Initial XMP caption"));

        // 2. User edits metadata locally in Lap (changes rating to 5, flag to Reject (2), comment to "Local Lap edit", and adds "Favorite" tag)
        conn.execute(
            "UPDATE afiles SET rating = 5, culling_flag = 2, comments = 'Local Lap edit' WHERE id = 1",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO atags (id, name, group_id) VALUES (10, 'Favorite', 1)", [])
            .unwrap();
        conn.execute("INSERT INTO afile_tags (file_id, tag_id) VALUES (1, 10)", [])
            .unwrap();

        // 3. Background rescan runs on an updated sidecar (e.g. darktable saved exposure history with old Rating=2, Flag=1, and added "Mountains" tag)
        let rescan_xmp = XmpMetadata {
            rating: Some(2),
            culling_flag: Some(1),
            tags: Some(vec!["Landscape".to_string(), "Mountains".to_string()]),
            description: Some("Initial XMP caption".to_string()),
        };
        apply_xmp_metadata_to_db(1, &rescan_xmp, &conn, XmpImportMode::RescanNonDestructive)
            .unwrap();

        // Verify local edits in Lap were NOT overwritten by the rescan
        let (rating_after, flag_after, comments_after): (i32, i32, Option<String>) = conn
            .query_row(
                "SELECT rating, culling_flag, comments FROM afiles WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(rating_after, 5);
        assert_eq!(flag_after, 2);
        assert_eq!(comments_after.as_deref(), Some("Local Lap edit"));

        // Verify both local tag ("Favorite") and newly discovered XMP tag ("Mountains") are present
        let mut stmt = conn
            .prepare(
                "SELECT t.name FROM atags t JOIN afile_tags ft ON t.id = ft.tag_id WHERE ft.file_id = 1 ORDER BY t.name",
            )
            .unwrap();
        let tag_names: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            tag_names,
            vec![
                "Favorite".to_string(),
                "Landscape".to_string(),
                "Mountains".to_string()
            ]
        );

        // 4. Also verify mtime gate: if user clears rating to 0 in Lap and rescans without XMP changing, rescan is skipped
        assert!(!should_import_xmp_on_rescan(1700000000000, 1700000005000));
        assert!(should_import_xmp_on_rescan(1700000010000, 1700000005000));

        // 5. Explicit manual "Import from XMP" overwrites present values from sidecar
        apply_xmp_metadata_to_db(1, &rescan_xmp, &conn, XmpImportMode::ManualOverwrite).unwrap();
        let (manual_rating, manual_flag, manual_comments): (i32, i32, Option<String>) = conn
            .query_row(
                "SELECT rating, culling_flag, comments FROM afiles WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(manual_rating, 2);
        assert_eq!(manual_flag, 1);
        assert_eq!(manual_comments.as_deref(), Some("Initial XMP caption"));
    }
}
