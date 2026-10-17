//! XMP Sidecar Metadata Support: Import and 2-way synchronization.
//!
//! Handles discovery, non-destructive parsing, and preservation-first updating
//! of .xmp sidecar files (compatible with Adobe Lightroom, Darktable, RapidRAW,
//! Capture One, DigiKam, etc.).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::t_sqlite::{AFile, open_conn};

static XMP_IMPORT_ENABLED: AtomicBool = AtomicBool::new(true);
static XMP_SYNC_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn is_xmp_import_enabled() -> bool {
    XMP_IMPORT_ENABLED.load(Ordering::Relaxed)
}

pub fn set_xmp_import_enabled(enabled: bool) {
    XMP_IMPORT_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn is_xmp_sync_enabled() -> bool {
    XMP_SYNC_ENABLED.load(Ordering::Relaxed)
}

pub fn set_xmp_sync_enabled(enabled: bool) {
    XMP_SYNC_ENABLED.store(enabled, Ordering::Relaxed);
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XmpMetadata {
    pub rating: Option<i32>,
    pub culling_flag: Option<i32>,
    pub tags: Vec<String>,
    pub color_label: Option<String>,
    pub description: Option<String>,
}

/// Discovers an existing XMP sidecar for a given image file path.
/// Checks `<stem>.<ext>.xmp` first (Darktable convention), then `<stem>.xmp` (Lightroom / RapidRAW).
pub fn find_xmp_sidecar(image_path: &Path) -> Option<PathBuf> {
    let parent = image_path.parent().unwrap_or(Path::new("."));
    let file_name = image_path.file_name()?.to_str()?;

    // 1. Convention B: <filename>.<ext>.xmp (e.g. _DSF3208.RAF.xmp)
    let candidate_ext_xmp = parent.join(format!("{}.xmp", file_name));
    if candidate_ext_xmp.is_file() {
        return Some(candidate_ext_xmp);
    }
    let candidate_ext_xmp_upper = parent.join(format!("{}.XMP", file_name));
    if candidate_ext_xmp_upper.is_file() {
        return Some(candidate_ext_xmp_upper);
    }

    // 2. Convention A: <stem>.xmp (e.g. _DSF3208.xmp)
    let stem = image_path.file_stem()?.to_str()?;
    let candidate_stem_xmp = parent.join(format!("{}.xmp", stem));
    if candidate_stem_xmp.is_file() {
        return Some(candidate_stem_xmp);
    }
    let candidate_stem_xmp_upper = parent.join(format!("{}.XMP", stem));
    if candidate_stem_xmp_upper.is_file() {
        return Some(candidate_stem_xmp_upper);
    }

    None
}

/// Resolves the file path to write XMP metadata to.
/// If a sidecar already exists (either naming convention), returns that path.
/// If no sidecar exists yet, defaults to `<stem>.xmp` (standard Adobe convention).
pub fn resolve_target_xmp_path(image_path: &Path) -> PathBuf {
    if let Some(existing) = find_xmp_sidecar(image_path) {
        return existing;
    }
    let parent = image_path.parent().unwrap_or(Path::new("."));
    let stem = image_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    parent.join(format!("{}.xmp", stem))
}

/// Reads and parses an XMP sidecar file.
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

/// Parses XMP XML content into structured metadata.
pub fn parse_xmp_str(content: &str) -> XmpMetadata {
    let mut meta = XmpMetadata::default();

    // 1. Rating & Culling
    if let Some(rating_val) = extract_attribute(content, "xmp:Rating")
        .or_else(|| extract_attribute(content, "xap:Rating"))
        .or_else(|| extract_tag_value(content, "xmp:Rating"))
        .or_else(|| extract_tag_value(content, "xap:Rating"))
    {
        if let Ok(num) = rating_val.trim().parse::<i32>() {
            if num == -1 {
                meta.culling_flag = Some(2); // Rejected
                meta.rating = Some(0);
            } else if (0..=5).contains(&num) {
                meta.rating = Some(num);
            }
        }
    }

    // Check Darktable rejected flag: <darktable:rejected>1</darktable:rejected>
    if let Some(rej) = extract_tag_value(content, "darktable:rejected") {
        if rej.trim() == "1" {
            meta.culling_flag = Some(2);
        }
    }

    // 2. Color Label
    let label = extract_attribute(content, "xmp:Label")
        .or_else(|| extract_tag_value(content, "xmp:Label"))
        .or_else(|| extract_attribute(content, "xap:Label"))
        .or_else(|| extract_tag_value(content, "xap:Label"));

    if let Some(lbl) = label {
        let trimmed = lbl.trim();
        if trimmed.eq_ignore_ascii_case("Reject") || trimmed.eq_ignore_ascii_case("Rejected") {
            meta.culling_flag = Some(2);
        } else if trimmed.eq_ignore_ascii_case("Pick") || trimmed.eq_ignore_ascii_case("Select") {
            meta.culling_flag = Some(1);
        } else if !trimmed.is_empty() {
            meta.color_label = Some(normalize_color_name(trimmed));
        }
    } else if let Some(dt_labels) = extract_tag_block(content, "darktable:colorlabels") {
        // Darktable colorlabels: 0=red, 1=yellow, 2=green, 3=blue, 4=purple
        let items = extract_li_items(&dt_labels);
        if let Some(first) = items.first() {
            let color = match first.trim() {
                "0" => "Red",
                "1" => "Yellow",
                "2" => "Green",
                "3" => "Blue",
                "4" => "Purple",
                _ => "",
            };
            if !color.is_empty() {
                meta.color_label = Some(color.to_string());
            }
        }
    } else if let Some(dk_lbl) = extract_attribute(content, "digiKam:ColorLabel") {
        let color = match dk_lbl.trim() {
            "1" => "Red",
            "2" => "Orange",
            "3" => "Yellow",
            "4" => "Green",
            "5" => "Blue",
            "6" => "Magenta",
            _ => "",
        };
        if !color.is_empty() {
            meta.color_label = Some(color.to_string());
        }
    }

    // 3. Keywords / Tags (dc:subject and lr:hierarchicalSubject)
    let mut tags_set = HashSet::new();
    let mut tags_list = Vec::new();

    if let Some(subject_block) = extract_tag_block(content, "dc:subject") {
        for tag in extract_li_items(&subject_block) {
            let clean = xml_unescape(tag.trim());
            if !clean.is_empty() && tags_set.insert(clean.to_lowercase()) {
                tags_list.push(clean);
            }
        }
    }

    if let Some(hierarchical_block) = extract_tag_block(content, "lr:hierarchicalSubject") {
        for tag in extract_li_items(&hierarchical_block) {
            let clean = xml_unescape(tag.trim());
            if !clean.is_empty() && tags_set.insert(clean.to_lowercase()) {
                tags_list.push(clean);
            }
        }
    }

    meta.tags = tags_list;

    // 4. Comments / Description
    if let Some(desc_block) = extract_tag_block(content, "dc:description") {
        let items = extract_li_items(&desc_block);
        if let Some(first) = items.first() {
            let clean = xml_unescape(first.trim());
            if !clean.is_empty() {
                meta.description = Some(clean);
            }
        } else if let Some(direct) = extract_tag_value(&desc_block, "dc:description") {
            let clean = xml_unescape(direct.trim());
            if !clean.is_empty() {
                meta.description = Some(clean);
            }
        }
    } else if let Some(user_comment) = extract_tag_block(content, "exif:UserComment") {
        let items = extract_li_items(&user_comment);
        if let Some(first) = items.first() {
            let clean = xml_unescape(first.trim());
            if !clean.is_empty() {
                meta.description = Some(clean);
            }
        }
    } else if let Some(caption) = extract_tag_value(content, "photoshop:Caption") {
        let clean = xml_unescape(caption.trim());
        if !clean.is_empty() {
            meta.description = Some(clean);
        }
    }

    meta
}

/// Updates an existing XMP sidecar or creates a new one, strictly preserving
/// all other tags, develop parameters (e.g. `<darktable:history>`), and namespaces.
pub fn write_xmp_file(
    image_path: &Path,
    rating: Option<i32>,
    culling_flag: Option<i32>,
    tags: &[String],
    color_label: Option<&str>,
    description: Option<&str>,
) -> Result<PathBuf, String> {
    let target_path = resolve_target_xmp_path(image_path);

    let output_content = if target_path.is_file() {
        let existing = fs::read_to_string(&target_path)
            .map_err(|e| format!("Failed to read existing XMP '{}': {}", target_path.display(), e))?;
        update_xmp_str(&existing, rating, culling_flag, tags, color_label, description)
    } else {
        create_xmp_template(rating, culling_flag, tags, color_label, description)
    };

    // Atomic write via temp file
    let tmp_path = target_path.with_extension("xmp.tmp");
    fs::write(&tmp_path, output_content.as_bytes())
        .map_err(|e| format!("Failed to write temporary XMP '{}': {}", tmp_path.display(), e))?;
    fs::rename(&tmp_path, &target_path)
        .map_err(|e| format!("Failed to replace XMP file '{}': {}", target_path.display(), e))?;

    Ok(target_path)
}

/// Updates existing XMP XML content in-place without disturbing other metadata.
pub fn update_xmp_str(
    content: &str,
    rating: Option<i32>,
    culling_flag: Option<i32>,
    tags: &[String],
    color_label: Option<&str>,
    description: Option<&str>,
) -> String {
    let mut updated = content.to_string();

    // 1. Resolve Rating value to write
    let target_rating = if culling_flag == Some(2) {
        Some(-1) // Rejected flag in standard Lightroom/XMP
    } else {
        rating
    };

    if let Some(r) = target_rating {
        let rating_str = r.to_string();
        // Check if xmp:Rating attribute exists
        if let Some(attr_match) = find_attribute_range(&updated, "xmp:Rating") {
            updated.replace_range(attr_match.val_start..attr_match.val_end, &rating_str);
        } else if let Some(tag_match) = find_tag_range(&updated, "xmp:Rating") {
            updated.replace_range(tag_match.content_start..tag_match.content_end, &rating_str);
        } else {
            // Insert xmp:Rating into <rdf:Description ...>
            if let Some(desc_idx) = find_desc_tag_insert_point(&updated) {
                let insert = format!(" xmp:Rating=\"{}\"", rating_str);
                updated.insert_str(desc_idx, &insert);
            }
        }
    }

    // Reset Darktable rejected flag if un-rejected
    if culling_flag != Some(2) {
        if let Some(tag_match) = find_tag_range(&updated, "darktable:rejected") {
            updated.replace_range(tag_match.content_start..tag_match.content_end, "0");
        }
    } else if let Some(tag_match) = find_tag_range(&updated, "darktable:rejected") {
        updated.replace_range(tag_match.content_start..tag_match.content_end, "1");
    }

    // 2. Color Label or Pick Label
    let label_to_set = if let Some(lbl) = color_label.filter(|s| !s.trim().is_empty()) {
        Some(lbl.trim().to_string())
    } else if culling_flag == Some(1) {
        Some("Pick".to_string())
    } else {
        None
    };

    if let Some(lbl) = label_to_set {
        let clean_lbl = xml_escape(&lbl);
        if let Some(attr_match) = find_attribute_range(&updated, "xmp:Label") {
            updated.replace_range(attr_match.val_start..attr_match.val_end, &clean_lbl);
        } else if let Some(tag_match) = find_tag_range(&updated, "xmp:Label") {
            updated.replace_range(tag_match.content_start..tag_match.content_end, &clean_lbl);
        } else if let Some(desc_idx) = find_desc_tag_insert_point(&updated) {
            let insert = format!(" xmp:Label=\"{}\"", clean_lbl);
            updated.insert_str(desc_idx, &insert);
        }
    } else {
        // Clear label if present
        remove_attribute(&mut updated, "xmp:Label");
        if let Some((start, end)) = find_full_tag_range(&updated, "xmp:Label") {
            updated.replace_range(start..end, "");
        }
    }

    // 3. Keywords / Tags (<dc:subject>)
    if !tags.is_empty() {
        let mut li_lines = String::new();
        for tag in tags {
            let clean = xml_escape(tag.trim());
            if !clean.is_empty() {
                li_lines.push_str(&format!("     <rdf:li>{}</rdf:li>\n", clean));
            }
        }
        let new_subject = format!(
            "<dc:subject>\n    <rdf:Bag>\n{}    </rdf:Bag>\n   </dc:subject>",
            li_lines
        );

        if let Some(tag_range) = find_full_tag_range(&updated, "dc:subject") {
            updated.replace_range(tag_range.0..tag_range.1, &new_subject);
        } else if let Some(end_desc) = updated.rfind("</rdf:Description>") {
            let insert = format!("   {}\n  ", new_subject);
            updated.insert_str(end_desc, &insert);
        }
    } else if let Some(tag_range) = find_full_tag_range(&updated, "dc:subject") {
        updated.replace_range(tag_range.0..tag_range.1, "");
    }

    // 4. Description (<dc:description>)
    if let Some(desc) = description.filter(|s| !s.trim().is_empty()) {
        let clean_desc = xml_escape(desc.trim());
        let new_desc = format!(
            "<dc:description>\n    <rdf:Alt>\n     <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n    </rdf:Alt>\n   </dc:description>",
            clean_desc
        );

        if let Some(tag_range) = find_full_tag_range(&updated, "dc:description") {
            updated.replace_range(tag_range.0..tag_range.1, &new_desc);
        } else if let Some(end_desc) = updated.rfind("</rdf:Description>") {
            let insert = format!("   {}\n  ", new_desc);
            updated.insert_str(end_desc, &insert);
        }
    } else if let Some(tag_range) = find_full_tag_range(&updated, "dc:description") {
        updated.replace_range(tag_range.0..tag_range.1, "");
    }

    updated
}

/// Generates a fresh, minimal Adobe-compliant XMP sidecar document.
pub fn create_xmp_template(
    rating: Option<i32>,
    culling_flag: Option<i32>,
    tags: &[String],
    color_label: Option<&str>,
    description: Option<&str>,
) -> String {
    let target_rating = if culling_flag == Some(2) {
        -1
    } else {
        rating.unwrap_or(0)
    };

    let label_attr = color_label
        .map(|l| format!("\n    xmp:Label=\"{}\"", xml_escape(l.trim())))
        .unwrap_or_default();

    let mut subject_block = String::new();
    if !tags.is_empty() {
        let mut li_items = String::new();
        for t in tags {
            let clean = xml_escape(t.trim());
            if !clean.is_empty() {
                li_items.push_str(&format!("     <rdf:li>{}</rdf:li>\n", clean));
            }
        }
        subject_block = format!(
            "\n   <dc:subject>\n    <rdf:Bag>\n{}    </rdf:Bag>\n   </dc:subject>",
            li_items
        );
    }

    let mut desc_block = String::new();
    if let Some(d) = description {
        let clean = xml_escape(d.trim());
        if !clean.is_empty() {
            desc_block = format!(
                "\n   <dc:description>\n    <rdf:Alt>\n     <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n    </rdf:Alt>\n   </dc:description>",
                clean
            );
        }
    }

    format!(
r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Lap Photo Manager">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:lr="http://ns.adobe.com/lightroom/1.0/"
    xmp:Rating="{}"{}>{}{}
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#,
        target_rating, label_attr, subject_block, desc_block
    )
}

// ---------------------------------------------------------------------------
// Database synchronization operations
// ---------------------------------------------------------------------------

const COLOR_LABEL_GROUP_NAME: &str = "Color Labels";

/// Imports XMP metadata into the database for a specific file.
pub fn import_xmp_for_file_with_conn(file_id: i64, file_path: &str, conn: &Connection) -> Result<bool, String> {
    let Some(sidecar_path) = find_xmp_sidecar(Path::new(file_path)) else {
        return Ok(false);
    };

    let meta = read_xmp_file(&sidecar_path)?;

    // 1. Update rating if extracted
    if let Some(rating) = meta.rating {
        conn.execute(
            "UPDATE afiles SET rating = ?1 WHERE id = ?2",
            params![rating.clamp(0, 5), file_id],
        )
        .map_err(|e| e.to_string())?;
    }

    // 2. Update culling flag if extracted
    if let Some(flag) = meta.culling_flag {
        conn.execute(
            "UPDATE afiles SET culling_flag = ?1 WHERE id = ?2",
            params![flag.clamp(0, 2), file_id],
        )
        .map_err(|e| e.to_string())?;
    }

    // 3. Update comments / description if extracted
    if let Some(desc) = meta.description {
        if !desc.trim().is_empty() {
            conn.execute(
                "UPDATE afiles SET comments = ?1 WHERE id = ?2",
                params![desc.trim(), file_id],
            )
            .map_err(|e| e.to_string())?;
        }
    }

    // 4. Resolve default tag group ID
    let default_group_id: i64 = conn
        .query_row(
            "SELECT id FROM atag_groups WHERE is_default = 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or(1);

    // 5. Handle Color Labels as tags in "Color Labels" tag group
    if let Some(color_name) = meta.color_label {
        let trimmed_color = color_name.trim();
        if !trimmed_color.is_empty() {
            let color_group_id: i64 = match conn.query_row(
                "SELECT id FROM atag_groups WHERE name = ?1 COLLATE NOCASE",
                params![COLOR_LABEL_GROUP_NAME],
                |r| r.get(0),
            ) {
                Ok(id) => id,
                Err(_) => {
                    let _ = conn.execute(
                        "INSERT INTO atag_groups (name, is_default, sort_order) VALUES (?1, 0, 99)",
                        params![COLOR_LABEL_GROUP_NAME],
                    );
                    conn.last_insert_rowid()
                }
            };

            let tag_id: i64 = match conn.query_row(
                "SELECT id FROM atags WHERE name = ?1 COLLATE NOCASE",
                params![trimmed_color],
                |r| r.get(0),
            ) {
                Ok(id) => id,
                Err(_) => {
                    let _ = conn.execute(
                        "INSERT INTO atags (name, group_id) VALUES (?1, ?2)",
                        params![trimmed_color, color_group_id],
                    );
                    conn.last_insert_rowid()
                }
            };

            let _ = conn.execute(
                "INSERT OR IGNORE INTO afile_tags (file_id, tag_id) VALUES (?1, ?2)",
                params![file_id, tag_id],
            );
            let _ = conn.execute(
                "UPDATE afiles SET has_tags = 1 WHERE id = ?1",
                params![file_id],
            );
        }
    }

    // 6. Handle Keywords / Tags
    for tag in meta.tags {
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
                let _ = conn.execute(
                    "INSERT INTO atags (name, group_id) VALUES (?1, ?2)",
                    params![trimmed, default_group_id],
                );
                conn.last_insert_rowid()
            }
        };

        let _ = conn.execute(
            "INSERT OR IGNORE INTO afile_tags (file_id, tag_id) VALUES (?1, ?2)",
            params![file_id, tag_id],
        );
        let _ = conn.execute(
            "UPDATE afiles SET has_tags = 1 WHERE id = ?1",
            params![file_id],
        );
    }

    Ok(true)
}

/// Imports XMP metadata into the database for a file using a fresh connection.
pub fn import_xmp_for_file(file_id: i64, file_path: &str) -> Result<bool, String> {
    let conn = open_conn()?;
    import_xmp_for_file_with_conn(file_id, file_path, &conn)
}

/// Exports current Lap metadata (rating, culling, tags, color labels, comments)
/// for a file into its associated .xmp sidecar.
pub fn export_xmp_for_file(file_id: i64) -> Result<PathBuf, String> {
    let conn = open_conn()?;
    let file = AFile::get_file_info(file_id)?
        .ok_or_else(|| format!("File id {} not found", file_id))?;
    let file_path = file.file_path.as_deref().ok_or("File path missing")?;

    // Fetch tags
    let mut stmt = conn
        .prepare(
            "SELECT t.name, g.name
             FROM atags t
             JOIN atag_groups g ON g.id = t.group_id
             INNER JOIN afile_tags ft ON t.id = ft.tag_id
             WHERE ft.file_id = ?1
             ORDER BY t.name ASC",
        )
        .map_err(|e| e.to_string())?;

    let tag_rows = stmt
        .query_map(params![file_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?;

    let mut subject_tags = Vec::new();
    let mut color_label: Option<String> = None;

    for row in tag_rows {
        let (tag_name, group_name) = row.map_err(|e| e.to_string())?;
        if group_name.eq_ignore_ascii_case(COLOR_LABEL_GROUP_NAME) {
            color_label = Some(normalize_color_name(&tag_name));
        } else {
            subject_tags.push(tag_name);
        }
    }

    write_xmp_file(
        Path::new(file_path),
        file.rating,
        file.culling_flag,
        &subject_tags,
        color_label.as_deref(),
        file.comments.as_deref(),
    )
}

// ---------------------------------------------------------------------------
// XML Parsing & Manipulation Helpers
// ---------------------------------------------------------------------------

struct AttrMatch {
    val_start: usize,
    val_end: usize,
}

struct TagMatch {
    content_start: usize,
    content_end: usize,
}

fn normalize_color_name(name: &str) -> String {
    let trimmed = name.trim();
    match trimmed.to_lowercase().as_str() {
        "red" => "Red".to_string(),
        "yellow" => "Yellow".to_string(),
        "green" => "Green".to_string(),
        "blue" => "Blue".to_string(),
        "purple" => "Purple".to_string(),
        "orange" => "Orange".to_string(),
        "magenta" => "Magenta".to_string(),
        "pink" => "Pink".to_string(),
        "gray" | "grey" => "Gray".to_string(),
        _ => trimmed.to_string(),
    }
}

fn remove_attribute(xml: &mut String, attr_name: &str) {
    let needle = format!("{}=\"", attr_name);
    if let Some(start) = xml.find(&needle) {
        let leading_space_start = xml[..start]
            .rfind(|c: char| !c.is_whitespace())
            .map(|idx| idx + 1)
            .unwrap_or(start);
        let after_start = start + needle.len();
        if let Some(quote_end) = xml[after_start..].find('"') {
            let full_end = after_start + quote_end + 1;
            xml.replace_range(leading_space_start..full_end, "");
        }
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn extract_attribute(xml: &str, attr_name: &str) -> Option<String> {
    let needle = format!("{}=\"", attr_name);
    let start_idx = xml.find(&needle)? + needle.len();
    let end_idx = xml[start_idx..].find('"')? + start_idx;
    Some(xml[start_idx..end_idx].to_string())
}

fn extract_tag_value(xml: &str, tag_name: &str) -> Option<String> {
    let open_tag = format!("<{}>", tag_name);
    let close_tag = format!("</{}>", tag_name);
    let start_idx = xml.find(&open_tag)? + open_tag.len();
    let end_idx = xml[start_idx..].find(&close_tag)? + start_idx;
    Some(xml[start_idx..end_idx].to_string())
}

fn extract_tag_block(xml: &str, tag_name: &str) -> Option<String> {
    let open_prefix = format!("<{}", tag_name);
    let close_tag = format!("</{}>", tag_name);
    let open_start = xml.find(&open_prefix)?;
    let close_start = xml[open_start..].find(&close_tag)? + open_start;
    let full_end = close_start + close_tag.len();
    Some(xml[open_start..full_end].to_string())
}

fn extract_li_items(block: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut rest = block;
    while let Some(open_idx) = rest.find("<rdf:li") {
        let after_open = &rest[open_idx..];
        let Some(tag_end) = after_open.find('>') else { break; };
        let content_start = open_idx + tag_end + 1;
        let Some(close_idx) = rest[content_start..].find("</rdf:li>") else { break; };
        let item_text = &rest[content_start..content_start + close_idx];
        items.push(item_text.to_string());
        rest = &rest[content_start + close_idx + "</rdf:li>".len()..];
    }
    items
}

fn find_attribute_range(xml: &str, attr_name: &str) -> Option<AttrMatch> {
    let needle = format!("{}=\"", attr_name);
    let start_idx = xml.find(&needle)? + needle.len();
    let end_idx = xml[start_idx..].find('"')? + start_idx;
    Some(AttrMatch {
        val_start: start_idx,
        val_end: end_idx,
    })
}

fn find_tag_range(xml: &str, tag_name: &str) -> Option<TagMatch> {
    let open_tag = format!("<{}>", tag_name);
    let close_tag = format!("</{}>", tag_name);
    let start_idx = xml.find(&open_tag)? + open_tag.len();
    let end_idx = xml[start_idx..].find(&close_tag)? + start_idx;
    Some(TagMatch {
        content_start: start_idx,
        content_end: end_idx,
    })
}

fn find_full_tag_range(xml: &str, tag_name: &str) -> Option<(usize, usize)> {
    let open_prefix = format!("<{}", tag_name);
    let close_tag = format!("</{}>", tag_name);
    let start_idx = xml.find(&open_prefix)?;
    let close_idx = xml[start_idx..].find(&close_tag)? + start_idx;
    Some((start_idx, close_idx + close_tag.len()))
}

fn find_desc_tag_insert_point(xml: &str) -> Option<usize> {
    let start = xml.find("<rdf:Description")?;
    let end_bracket = xml[start..].find('>')? + start;
    Some(end_bracket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_darktable_xmp() {
        let xmp = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="XMP Core 4.4.0-Exiv2">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:lr="http://ns.adobe.com/lightroom/1.0/"
    xmp:Rating="2">
   <dc:subject>
    <rdf:Bag>
     <rdf:li>People</rdf:li>
     <rdf:li>Fuji/Astia</rdf:li>
    </rdf:Bag>
   </dc:subject>
   <lr:hierarchicalSubject>
    <rdf:Bag>
     <rdf:li>People|Edith</rdf:li>
    </rdf:Bag>
   </lr:hierarchicalSubject>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        let meta = parse_xmp_str(xmp);
        assert_eq!(meta.rating, Some(2));
        assert_eq!(meta.culling_flag, None);
        assert!(meta.tags.contains(&"People".to_string()));
        assert!(meta.tags.contains(&"Fuji/Astia".to_string()));
        assert!(meta.tags.contains(&"People|Edith".to_string()));
    }

    #[test]
    fn test_parse_rapidraw_xmp() {
        let xmp = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="RapidRAW">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/">
   <xmp:Rating>3</xmp:Rating>
</rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        let meta = parse_xmp_str(xmp);
        assert_eq!(meta.rating, Some(3));
    }

    #[test]
    fn test_parse_rejected_and_color_labels() {
        let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmp:Rating="-1"
    xmp:Label="Red">
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        let meta = parse_xmp_str(xmp);
        assert_eq!(meta.rating, Some(0));
        assert_eq!(meta.culling_flag, Some(2)); // Rejected
        assert_eq!(meta.color_label.as_deref(), Some("Red"));
    }

    #[test]
    fn test_update_existing_xmp_preserves_other_nodes() {
        let original = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:darktable="http://darktable.sf.net/"
    xmp:Rating="1">
   <darktable:history>
    <rdf:Seq>
     <rdf:li darktable:operation="rawprepare" darktable:enabled="1" />
    </rdf:Seq>
   </darktable:history>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        let updated = update_xmp_str(
            original,
            Some(5),
            None,
            &["Holiday".to_string(), "Family".to_string()],
            Some("Blue"),
            Some("Summer vacation"),
        );

        // Verify history wasn't touched
        assert!(updated.contains(r#"darktable:operation="rawprepare""#));
        // Verify rating updated
        assert!(updated.contains(r#"xmp:Rating="5""#));
        // Verify label added
        assert!(updated.contains(r#"xmp:Label="Blue""#));
        // Verify tags added
        assert!(updated.contains("<rdf:li>Holiday</rdf:li>"));
        assert!(updated.contains("<rdf:li>Family</rdf:li>"));
        // Verify description added
        assert!(updated.contains("Summer vacation"));
    }

    #[test]
    fn test_create_template() {
        let tpl = create_xmp_template(
            Some(4),
            None,
            &["Sunset".to_string()],
            Some("Green"),
            Some("Golden hour"),
        );
        let meta = parse_xmp_str(&tpl);
        assert_eq!(meta.rating, Some(4));
        assert_eq!(meta.color_label.as_deref(), Some("Green"));
        assert_eq!(meta.tags, vec!["Sunset".to_string()]);
        assert_eq!(meta.description.as_deref(), Some("Golden hour"));
    }

    #[test]
    fn test_clear_tags_and_description_and_unreject() {
        let original = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:darktable="http://darktable.sf.net/"
    xmp:Rating="-1"
    xmp:Label="Red">
   <darktable:rejected>1</darktable:rejected>
   <dc:subject>
    <rdf:Bag>
     <rdf:li>OldTag</rdf:li>
    </rdf:Bag>
   </dc:subject>
   <dc:description>
    <rdf:Alt>
     <rdf:li xml:lang="x-default">Old comment</rdf:li>
    </rdf:Alt>
   </dc:description>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        // Un-reject (culling_flag = Some(0)), rating = 3, clear tags (&[]), clear description (None), clear label (None)
        let updated = update_xmp_str(original, Some(3), Some(0), &[], None, None);

        assert!(updated.contains(r#"xmp:Rating="3""#));
        assert!(updated.contains("<darktable:rejected>0</darktable:rejected>"));
        assert!(!updated.contains("xmp:Label="));
        assert!(!updated.contains("<dc:subject>"));
        assert!(!updated.contains("<dc:description>"));
    }
}
