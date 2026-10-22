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

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// Fills empty fields and merges tags only when the corresponding field has
    /// not been edited locally. Explicit local clears are protected as well.
    RescanNonDestructive,
    /// Used when the user explicitly selects "Import from XMP" from the context menu.
    /// Applies all `Some(...)` fields from the sidecar (including `Some(0)` or `Some("")` to clear),
    /// while leaving `None` (missing) fields untouched.
    ManualOverwrite,
}

const MAX_XMP_BYTES: usize = 16 * 1024 * 1024;
const MAX_XML_DEPTH: usize = 64;
const MAX_XML_NODES: usize = 100_000;
const MAX_XML_EXPANDED_BYTES: usize = 32 * 1024 * 1024;

type SidecarIndex = (i64, PathBuf, HashMap<String, PathBuf>);
thread_local! {
    static SCAN: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
    static INDEX: std::cell::RefCell<Option<SidecarIndex>> = const { std::cell::RefCell::new(None) };
}
pub struct SidecarScan(Option<i64>);
impl Drop for SidecarScan {
    fn drop(&mut self) {
        SCAN.with(|scan| scan.set(self.0));
    }
}
/// Reuse an index only within this scan's timestamp, never across scan generations.
pub fn sidecar_scan(generation: i64) -> SidecarScan {
    SidecarScan(SCAN.with(|scan| scan.replace(Some(generation))))
}

/// The photo name is case-sensitive; only the sidecar extension is folded.
/// Duplicate extension spellings are resolved deterministically.
pub fn find_xmp_sidecar(image_path: &Path) -> Option<PathBuf> {
    let parent = image_path.parent().unwrap_or(Path::new("."));
    let file_name = image_path.file_name()?.to_str()?;
    let stem = image_path.file_stem()?.to_str()?;
    INDEX.with(|cache| {
        let generation = SCAN.with(|scan| scan.get());
        let mut cache = cache.borrow_mut();
        if generation.is_none()
            || cache
                .as_ref()
                .is_none_or(|(scan, dir, _)| Some(*scan) != generation || dir != parent)
        {
            let mut names = HashMap::<String, PathBuf>::new();
            for entry in fs::read_dir(parent).ok()?.flatten() {
                let path = entry.path();
                if path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("xmp"))
                    && path.is_file()
                {
                    let name = path.file_stem()?.to_str()?.to_string();
                    let chosen = names.entry(name).or_insert_with(|| path.clone());
                    if path < *chosen {
                        *chosen = path;
                    }
                }
            }
            *cache = Some((generation.unwrap_or(0), parent.to_path_buf(), names));
        }
        let names = &cache.as_ref()?.2;
        names.get(file_name).or_else(|| names.get(stem)).cloned()
    })
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct XmpBatchResult {
    pub imported_ids: Vec<i64>,
    pub skipped_ids: Vec<i64>,
    pub failures: Vec<XmpImportFailure>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct XmpImportFailure {
    pub file_id: i64,
    pub path: String,
    pub error: String,
}
pub fn import_batch(files: &[(i64, String)], conn: &Connection) -> XmpBatchResult {
    static GENERATION: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
    let _scan = sidecar_scan(GENERATION.fetch_sub(1, Ordering::Relaxed));
    let mut result = XmpBatchResult::default();
    for (id, path) in files {
        match import_xmp_for_file_with_conn(*id, path, conn, XmpImportMode::ManualOverwrite) {
            Ok(true) => result.imported_ids.push(*id),
            Ok(false) => result.skipped_ids.push(*id),
            Err(error) => result.failures.push(XmpImportFailure {
                file_id: *id,
                path: path.clone(),
                error,
            }),
        }
    }
    result
}

/// Reads and parses an XMP sidecar file, skipping cloud-only (unhydrated) placeholders.
pub fn read_xmp_file(path: &Path) -> Result<XmpMetadata, String> {
    if let Ok(meta) = fs::metadata(path) {
        if crate::t_utils::is_cloud_only(&meta) {
            return Err(format!(
                "Skipping cloud-only XMP sidecar '{}'",
                path.display()
            ));
        }
    }
    use std::io::Read;
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut content = String::new();
    file.take(MAX_XMP_BYTES as u64 + 1)
        .read_to_string(&mut content)
        .map_err(|e| e.to_string())?;
    if content.len() > MAX_XMP_BYTES {
        return Err("XMP exceeds 16 MiB size limit".into());
    }
    parse_xmp_str(&content)
}

/// Parse the complete XML document before exposing any metadata to the importer.
pub fn parse_xmp_str(content: &str) -> Result<XmpMetadata, String> {
    let root = parse_xml(content)?;
    let mut properties = Vec::new();
    collect_properties(&root, &mut properties);
    let value = |uri: &str, name: &str| {
        properties
            .iter()
            .find(|node| node.uri == uri && node.name == name)
            .map(|n| n.value())
    };
    let mut meta = XmpMetadata::default();
    let mut rejected = false;
    if let Some(v) = value(NS_XMP, "Rating") {
        if v.trim().is_empty() {
            meta.rating = Some(0);
        } else if let Ok(n) = v.trim().parse::<i32>() {
            if (-1..=5).contains(&n) {
                meta.rating = Some(n.max(0));
                rejected = n == -1;
            }
        }
    }
    meta.culling_flag = value(NS_XMP_DM, "pick")
        .and_then(|v| match v.trim() {
            "1" => Some(1),
            "-1" => Some(2),
            "0" | "" => Some(0),
            _ => None,
        })
        .or_else(|| {
            value(NS_DARKTABLE, "rejected").and_then(|v| match v.trim() {
                "1" | "true" => Some(2),
                "0" | "false" | "" => Some(0),
                _ => None,
            })
        })
        .or_else(|| {
            value(NS_XMP, "Label").and_then(|v| match v.trim().to_ascii_lowercase().as_str() {
                "pick" | "select" => Some(1),
                "reject" | "rejected" => Some(2),
                _ => None,
            })
        })
        .or(if rejected { Some(2) } else { None });
    let mut seen = HashSet::new();
    for node in &properties {
        if matches!(
            (node.uri.as_str(), node.name.as_str()),
            (NS_DC, "subject") | (NS_LR, "hierarchicalSubject") | (NS_DIGIKAM, "TagsList")
        ) {
            let tags = meta.tags.get_or_insert_with(Vec::new);
            for container in &node.children {
                if container.uri != NS_RDF || !matches!(container.name.as_str(), "Bag" | "Seq") {
                    continue;
                }
                for item in &container.children {
                    let tag = item.text.trim();
                    if item.uri == NS_RDF
                        && item.name == "li"
                        && !tag.is_empty()
                        && !is_internal_system_tag(tag)
                        && seen.insert(tag.to_lowercase())
                    {
                        tags.push(tag.to_string());
                    }
                }
            }
        }
    }
    meta.description = value(NS_DC, "description")
        .or_else(|| value(NS_EXIF, "UserComment"))
        .or_else(|| value(NS_PHOTOSHOP, "Caption"));
    Ok(meta)
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

    let info = fs::metadata(&sidecar_path).map_err(|e| e.to_string())?;
    let fingerprint = format!(
        "{}:{:?}:{}",
        sidecar_path.display(),
        info.modified().ok(),
        info.len()
    );
    if mode != XmpImportMode::ManualOverwrite {
        let previous: Option<String> = conn
            .query_row(
                "SELECT fingerprint FROM axmp_imports WHERE file_id = ?1",
                [file_id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        if previous.as_deref() == Some(&fingerprint) {
            return Ok(false);
        }
    }
    let meta = read_xmp_file(&sidecar_path)?;
    apply_with_state(file_id, &meta, conn, mode, Some(&fingerprint))
}

/// Installs durable import state and observes local edits, including clears.
/// Import writes restore the prior dirty mask within the same transaction.
pub fn initialize(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS axmp_imports (
        file_id INTEGER PRIMARY KEY REFERENCES afiles(id) ON DELETE CASCADE,
        fingerprint TEXT, dirty INTEGER NOT NULL DEFAULT 0
    );",
    )
    .map_err(|e| e.to_string())?;
    for (column, bit) in [("rating", 1), ("culling_flag", 2), ("comments", 4)] {
        conn.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS axmp_local_{column}
        AFTER UPDATE OF {column} ON afiles WHEN NEW.{column} IS NOT OLD.{column}
        BEGIN INSERT INTO axmp_imports(file_id, dirty) VALUES(NEW.id, {bit})
        ON CONFLICT(file_id) DO UPDATE SET dirty = dirty | {bit}; END;"
        ))
        .map_err(|e| e.to_string())?;
    }
    for (operation, row) in [("INSERT", "NEW"), ("DELETE", "OLD")] {
        conn.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS axmp_local_tags_{operation}
        AFTER {operation} ON afile_tags BEGIN
        INSERT INTO axmp_imports(file_id, dirty) SELECT {row}.file_id, 8 WHERE EXISTS(SELECT 1 FROM afiles WHERE id = {row}.file_id)
        ON CONFLICT(file_id) DO UPDATE SET dirty = dirty | 8; END;" )).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
fn apply_xmp_metadata_to_db(
    file_id: i64,
    meta: &XmpMetadata,
    conn: &Connection,
    mode: XmpImportMode,
) -> Result<bool, String> {
    apply_with_state(file_id, meta, conn, mode, None)
}

fn apply_with_state(
    file_id: i64,
    meta: &XmpMetadata,
    conn: &Connection,
    mode: XmpImportMode,
    fingerprint: Option<&str>,
) -> Result<bool, String> {
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let dirty: i32 = tx
        .query_row(
            "SELECT dirty FROM axmp_imports WHERE file_id = ?1",
            [file_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let result = apply_fields(file_id, meta, &tx, mode, dirty)?;
    tx.execute("INSERT INTO axmp_imports(file_id, fingerprint, dirty) VALUES(?1, ?2, ?3)
        ON CONFLICT(file_id) DO UPDATE SET fingerprint = COALESCE(excluded.fingerprint, fingerprint), dirty = excluded.dirty",
        params![file_id, fingerprint, dirty]).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(result)
}

fn apply_fields(
    file_id: i64,
    meta: &XmpMetadata,
    conn: &Connection,
    mode: XmpImportMode,
    dirty: i32,
) -> Result<bool, String> {
    let (db_rating, db_culling_flag, db_comments): (i32, i32, Option<String>) = conn.query_row(
        "SELECT COALESCE(rating, 0), COALESCE(culling_flag, 0), comments FROM afiles WHERE id = ?1", [file_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    ).map_err(|e| e.to_string())?;

    // 1. Rating
    if let Some(rating) = meta.rating {
        let clamped = rating.clamp(0, 5);
        let should_update = match mode {
            XmpImportMode::InitialScan => clamped > 0,
            XmpImportMode::RescanNonDestructive => dirty & 1 == 0 && db_rating == 0 && clamped > 0,
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
            XmpImportMode::RescanNonDestructive => {
                dirty & 2 == 0 && db_culling_flag == 0 && clamped > 0
            }
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
        let db_comment_empty = db_comments.as_deref().is_none_or(|c| c.trim().is_empty());

        let should_update = match mode {
            XmpImportMode::InitialScan => !trimmed.is_empty(),
            XmpImportMode::RescanNonDestructive => {
                dirty & 4 == 0 && db_comment_empty && !trimmed.is_empty()
            }
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
        if mode == XmpImportMode::RescanNonDestructive && dirty & 8 != 0 {
            return Ok(true);
        }
        if mode == XmpImportMode::ManualOverwrite {
            conn.execute("DELETE FROM afile_tags WHERE file_id = ?1", [file_id])
                .map_err(|e| e.to_string())?;
            conn.execute("UPDATE afiles SET has_tags = 0 WHERE id = ?1", [file_id])
                .map_err(|e| e.to_string())?;
        }
        if tags.is_empty() {
            if mode == XmpImportMode::ManualOverwrite {
                conn.execute(
                    "DELETE FROM afile_tags WHERE file_id = ?1",
                    params![file_id],
                )
                .map_err(|e| e.to_string())?;
                conn.execute(
                    "UPDATE afiles SET has_tags = 0 WHERE id = ?1",
                    params![file_id],
                )
                .map_err(|e| e.to_string())?;
            }
        } else {
            let default_group_id: i64 = conn
                .query_row("SELECT id FROM atag_groups WHERE is_default = 1", [], |r| {
                    r.get(0)
                })
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

#[derive(Default)]
struct XmlNode {
    uri: String,
    name: String,
    text: String,
    lang: Option<String>,
    attributes: Vec<XmlNode>,
    children: Vec<XmlNode>,
}
impl XmlNode {
    fn value(&self) -> String {
        if let Some(alt) = self
            .children
            .iter()
            .find(|n| n.uri == NS_RDF && n.name == "Alt")
        {
            let items = || {
                alt.children
                    .iter()
                    .filter(|n| n.uri == NS_RDF && n.name == "li")
            };
            return items()
                .find(|n| n.lang.as_deref() == Some("x-default"))
                .or_else(|| items().next())
                .map(|n| n.text.trim().to_string())
                .unwrap_or_default();
        }
        self.text.trim().to_string()
    }
}
fn collect_properties<'a>(node: &'a XmlNode, out: &mut Vec<&'a XmlNode>) {
    if node.uri == NS_RDF && node.name == "Description" {
        out.extend(node.attributes.iter());
        out.extend(node.children.iter());
    }
    for child in &node.children {
        collect_properties(child, out);
    }
}
fn resolved_name(result: quick_xml::name::ResolveResult<'_>) -> Result<String, String> {
    match result {
        quick_xml::name::ResolveResult::Bound(ns) => {
            String::from_utf8(ns.as_ref().to_vec()).map_err(|e| e.to_string())
        }
        quick_xml::name::ResolveResult::Unbound => Ok(String::new()),
        quick_xml::name::ResolveResult::Unknown(prefix) => {
            Err(format!("Unbound XML prefix: {:?}", prefix))
        }
    }
}
fn parse_xml(content: &str) -> Result<XmlNode, String> {
    use quick_xml::{events::Event, reader::NsReader};
    if content.len() > MAX_XMP_BYTES {
        return Err("XMP exceeds 16 MiB size limit".into());
    }
    let mut reader = NsReader::from_str(content);
    let mut nodes = 0usize;
    let mut expanded = 0usize;
    let mut stack = vec![XmlNode::default()];
    loop {
        let event = reader.read_event().map_err(|e| e.to_string())?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                if stack.len() > MAX_XML_DEPTH {
                    return Err("XMP nesting depth limit exceeded".into());
                }
                nodes += 1;
                if nodes > MAX_XML_NODES {
                    return Err("XMP node limit exceeded".into());
                }
                let (ns, local) = reader.resolve_element(e.name());
                let mut node = XmlNode {
                    uri: resolved_name(ns)?,
                    name: String::from_utf8_lossy(local.as_ref()).into_owned(),
                    ..Default::default()
                };
                expanded += node.uri.len() + node.name.len();
                if expanded > MAX_XML_EXPANDED_BYTES {
                    return Err("XMP expanded text limit exceeded".into());
                }
                for attr in e.attributes() {
                    let attr = attr.map_err(|e| e.to_string())?;
                    nodes += 1;
                    if nodes > MAX_XML_NODES {
                        return Err("XMP node limit exceeded".into());
                    }
                    if attr.key.as_ref() == b"xmlns" || attr.key.as_ref().starts_with(b"xmlns:") {
                        continue;
                    }
                    let (ns, local) = reader.resolve_attribute(attr.key);
                    let uri = resolved_name(ns)?;
                    let name = String::from_utf8_lossy(local.as_ref()).into_owned();
                    let text = attr
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|e| e.to_string())?
                        .into_owned();
                    expanded += uri.len() + name.len() + text.len();
                    if expanded > MAX_XML_EXPANDED_BYTES {
                        return Err("XMP expanded text limit exceeded".into());
                    }
                    if uri == "http://www.w3.org/XML/1998/namespace" && name == "lang" {
                        node.lang = Some(text.clone());
                    }
                    node.attributes.push(XmlNode {
                        uri,
                        name,
                        text,
                        ..Default::default()
                    });
                }
                if matches!(event, Event::Empty(_)) {
                    stack.last_mut().unwrap().children.push(node);
                } else {
                    stack.push(node);
                }
            }
            Event::End(_) => {
                if stack.len() <= 1 {
                    return Err("Unexpected XML closing tag".into());
                }
                let node = stack.pop().unwrap();
                stack.last_mut().unwrap().children.push(node);
            }
            Event::Text(e) => {
                let text = e.unescape().map_err(|e| e.to_string())?;
                if stack.len() == 1 && !text.trim().is_empty() {
                    return Err("Text outside XML root".into());
                }
                expanded += text.len();
                if expanded > MAX_XML_EXPANDED_BYTES {
                    return Err("XMP expanded text limit exceeded".into());
                }
                stack.last_mut().unwrap().text.push_str(&text);
            }
            Event::CData(e) => {
                let text = e.decode().map_err(|e| e.to_string())?;
                expanded += text.len();
                if expanded > MAX_XML_EXPANDED_BYTES {
                    return Err("XMP expanded text limit exceeded".into());
                }
                stack.last_mut().unwrap().text.push_str(&text);
            }
            Event::DocType(_) => return Err("DTD is not supported in XMP".into()),
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 || stack[0].children.len() != 1 {
        return Err("Incomplete XML document or multiple roots".into());
    }
    Ok(stack.pop().unwrap())
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
        initialize(&conn).unwrap();
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

        let meta = parse_xmp_str(xmp).unwrap();
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

        let missing_meta = parse_xmp_str(missing_xmp).unwrap();
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

        let empty_meta = parse_xmp_str(empty_xmp).unwrap();
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

        let meta = parse_xmp_str(xmp).unwrap();
        assert_eq!(meta.rating, Some(3));
        assert_eq!(meta.culling_flag, Some(2)); // Rejected via dt:rejected
        assert_eq!(meta.description.as_deref(), Some("Default English caption"));
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
        conn.execute(
            "INSERT INTO atags (id, name, group_id) VALUES (10, 'Favorite', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO afile_tags (file_id, tag_id) VALUES (1, 10)",
            [],
        )
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

        // Local edits to the tag set prevent automatic additions, including restoring deleted tags.
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
            vec!["Favorite".to_string(), "Landscape".to_string(),]
        );

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
    fn xml(body: &str) -> String {
        format!(
            r#"<rdf:RDF xmlns:rdf="{NS_RDF}"><rdf:Description xmlns:xmp="{NS_XMP}" xmlns:dc="{NS_DC}">{body}</rdf:Description></rdf:RDF>"#
        )
    }
    #[test]
    fn parses_xml_text_and_ignores_comments() {
        let meta = parse_xmp_str(&xml(r#"<!-- xmp:Rating="5" --><xmp:Rating>2</xmp:Rating><dc:description><![CDATA[A < B]]> &amp; caf&#233;</dc:description>"#)).unwrap();
        assert_eq!(meta.rating, Some(2));
        assert_eq!(meta.description.as_deref(), Some("A < B & café"));
        let meta = parse_xmp_str(&format!(
            r#"<rdf:Description xmlns:rdf="{NS_RDF}" xmlns:xmp="{NS_XMP}" xmp:Rating = "5"/>"#
        ))
        .unwrap();
        assert_eq!(meta.rating, Some(5));
    }
    #[test]
    fn namespaces_are_scoped_and_language_is_exact() {
        let meta = parse_xmp_str(&xml(r#"<dc:description><rdf:Alt><rdf:li note="x-default" xml:lang="fr">French</rdf:li><rdf:li xml:lang="x-default">Default</rdf:li></rdf:Alt></dc:description><other xmlns:xmp="urn:other" xmp:Rating="5"/><xmp:Rating>3</xmp:Rating>"#)).unwrap();
        assert_eq!(meta.rating, Some(3));
        assert_eq!(meta.description.as_deref(), Some("Default"));
    }
    #[test]
    fn malformed_xml_is_an_error() {
        for input in [
            "",
            "<root>",
            "<root><a></root>",
            "<a/><b/>",
            "<root>&unknown;</root>",
        ] {
            assert!(parse_xmp_str(input).is_err(), "{input}");
        }
    }
    fn file(conn: &Connection) {
        conn.execute("INSERT INTO afiles(id,name) VALUES(1,'photo')", [])
            .unwrap();
    }
    #[test]
    fn rescan_protects_clears_and_removed_tags() {
        let conn = setup_in_memory_db();
        file(&conn);
        let meta = XmpMetadata {
            rating: Some(4),
            culling_flag: Some(1),
            description: Some("caption".into()),
            tags: Some(vec!["tag".into()]),
        };
        apply_xmp_metadata_to_db(1, &meta, &conn, XmpImportMode::InitialScan).unwrap();
        conn.execute(
            "UPDATE afiles SET rating=0,culling_flag=0,comments=NULL WHERE id=1",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM afile_tags WHERE file_id=1", [])
            .unwrap();
        apply_xmp_metadata_to_db(1, &meta, &conn, XmpImportMode::RescanNonDestructive).unwrap();
        let state: (i32, i32, Option<String>) = conn
            .query_row(
                "SELECT rating,culling_flag,comments FROM afiles WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, (0, 0, None));
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM afile_tags", [], |r| r
                .get::<_, i32>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn manual_tags_replace_missing_preserves_and_empty_clears() {
        let conn = setup_in_memory_db();
        file(&conn);
        for name in ["old", "new"] {
            apply_xmp_metadata_to_db(
                1,
                &XmpMetadata {
                    tags: Some(vec![name.into()]),
                    ..Default::default()
                },
                &conn,
                XmpImportMode::ManualOverwrite,
            )
            .unwrap();
        }
        apply_xmp_metadata_to_db(
            1,
            &XmpMetadata::default(),
            &conn,
            XmpImportMode::ManualOverwrite,
        )
        .unwrap();
        let tags: String = conn
            .query_row(
                "SELECT group_concat(name) FROM atags JOIN afile_tags ON tag_id=atags.id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tags, "new");
        apply_xmp_metadata_to_db(
            1,
            &XmpMetadata {
                tags: Some(vec![]),
                ..Default::default()
            },
            &conn,
            XmpImportMode::ManualOverwrite,
        )
        .unwrap();
        assert_eq!(
            conn.query_row("SELECT has_tags FROM afiles", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn failure_rolls_back_all_fields_and_dirty_tracking() {
        let conn = setup_in_memory_db();
        file(&conn);
        conn.execute_batch("CREATE TRIGGER fail_tag BEFORE INSERT ON atags BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
        let meta = XmpMetadata {
            rating: Some(5),
            tags: Some(vec!["fail".into()]),
            ..Default::default()
        };
        assert!(apply_xmp_metadata_to_db(1, &meta, &conn, XmpImportMode::ManualOverwrite).is_err());
        assert_eq!(
            conn.query_row("SELECT rating FROM afiles", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM axmp_imports", [], |r| r
                .get::<_, i32>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn existing_sidecars_import_independently_of_photo_scan_time() {
        let conn = setup_in_memory_db();
        file(&conn);
        let dir = create_temp_test_dir("state");
        let image = dir.join("photo.RAF");
        let sidecar = dir.join("photo.RAF.xmp");
        fs::write(&sidecar, xml("<xmp:Rating>4</xmp:Rating>")).unwrap();
        let path = image.to_str().unwrap();
        assert!(
            import_xmp_for_file_with_conn(1, path, &conn, XmpImportMode::RescanNonDestructive)
                .unwrap()
        );
        assert!(
            !import_xmp_for_file_with_conn(1, path, &conn, XmpImportMode::RescanNonDestructive)
                .unwrap()
        );
        conn.execute("UPDATE afiles SET rating=0 WHERE id=1", [])
            .unwrap();
        fs::write(&sidecar, xml("<xmp:Rating>5</xmp:Rating><!-- changed -->")).unwrap();
        assert!(
            import_xmp_for_file_with_conn(1, path, &conn, XmpImportMode::RescanNonDestructive)
                .unwrap()
        );
        assert_eq!(
            conn.query_row("SELECT rating FROM afiles", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            0
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn mixed_case_full_name_takes_priority_and_negative_cache_invalidates() {
        let dir = create_temp_test_dir("cache");
        let image = dir.join("photo.RAF");
        assert!(find_xmp_sidecar(&image).is_none());
        let stem = dir.join("photo.xmp");
        fs::write(&stem, "").unwrap();
        assert_eq!(find_xmp_sidecar(&image), Some(stem));
        let full = dir.join("photo.RAF.Xmp");
        fs::write(&full, "").unwrap();
        assert_eq!(
            fs::canonicalize(find_xmp_sidecar(&image).unwrap()).unwrap(),
            fs::canonicalize(full).unwrap()
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn photo_case_is_preserved() {
        let dir = create_temp_test_dir("case");
        fs::write(dir.join("A.RAF.Xmp"), "").unwrap();
        assert!(find_xmp_sidecar(&dir.join("a.RAF")).is_none());
        assert!(find_xmp_sidecar(&dir.join("A.RAF")).is_some());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn new_scan_rebuilds_index_without_directory_timestamp() {
        let dir = create_temp_test_dir("generation");
        let image = dir.join("photo.RAF");
        {
            let _scan = sidecar_scan(101);
            assert!(find_xmp_sidecar(&image).is_none());
            fs::write(dir.join("photo.Xmp"), "").unwrap();
            assert!(find_xmp_sidecar(&image).is_none());
        }
        {
            let _scan = sidecar_scan(102);
            assert!(find_xmp_sidecar(&image).is_some());
        }
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rejects_resource_exhaustion() {
        let deep = format!(
            "{}{}",
            "<a>".repeat(MAX_XML_DEPTH + 1),
            "</a>".repeat(MAX_XML_DEPTH + 1)
        );
        assert!(parse_xmp_str(&deep).unwrap_err().contains("depth"));
        let wide = format!("<root>{}</root>", "<a/>".repeat(MAX_XML_NODES));
        assert!(parse_xmp_str(&wide).unwrap_err().contains("node"));
        assert!(
            parse_xmp_str(&" ".repeat(MAX_XMP_BYTES + 1))
                .unwrap_err()
                .contains("size")
        );
        let amplified = format!(
            "<root xmlns:n='{}'>{}</root>",
            "x".repeat(1024 * 1024),
            "<n:a/>".repeat(40)
        );
        assert!(parse_xmp_str(&amplified).unwrap_err().contains("expanded"));
        let dir = create_temp_test_dir("size");
        let path = dir.join("huge.xmp");
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_XMP_BYTES as u64 + 1)
            .unwrap();
        assert!(read_xmp_file(&path).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn batch_continues_after_failure_and_reports_each_result() {
        let conn = setup_in_memory_db();
        let dir = create_temp_test_dir("batch");
        let mut files = Vec::new();
        for id in 1..=4 {
            conn.execute(
                "INSERT INTO afiles(id,name) VALUES(?1,?2)",
                params![id, format!("p{id}")],
            )
            .unwrap();
            let path = dir.join(format!("p{id}.RAF"));
            if id != 4 {
                fs::write(
                    dir.join(format!("p{id}.xmp")),
                    if id == 2 {
                        "<broken>".into()
                    } else {
                        xml("<xmp:Rating>5</xmp:Rating>")
                    },
                )
                .unwrap();
            }
            files.push((id, path.to_string_lossy().into_owned()));
        }
        let result = import_batch(&files, &conn);
        assert_eq!(result.imported_ids, vec![1, 3]);
        assert_eq!(result.skipped_ids, vec![4]);
        assert_eq!(result.failures.len(), 1);
        assert_eq!(result.failures[0].file_id, 2);
        assert_eq!(
            conn.query_row("SELECT rating FROM afiles WHERE id=2", [], |r| r
                .get::<_, i32>(0))
                .unwrap(),
            0
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
