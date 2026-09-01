//! JSON export and import for scanned trees.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path};

use serde_json::{Map, Value};

use crate::tree::{Dir, FileStats, Flag};

/// Optional fields emitted by an export.
#[derive(Debug, Clone, Copy)]
pub struct Attributes {
    pub apparent_size: bool,
    pub disk_size: bool,
    pub items: bool,
    pub mtime: bool,
    pub not_regular: bool,
}

impl Default for Attributes {
    fn default() -> Self {
        Self {
            apparent_size: true,
            disk_size: true,
            items: true,
            mtime: true,
            not_regular: true,
        }
    }
}

impl Attributes {
    /// Parses a `--output-attrs` list.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut attributes = Self {
            apparent_size: false,
            disk_size: false,
            items: false,
            mtime: false,
            not_regular: false,
        };
        for field in spec.split(',').map(str::trim).filter(|f| !f.is_empty()) {
            match field {
                "name" => {}
                "asize" => attributes.apparent_size = true,
                "dsize" => attributes.disk_size = true,
                "items" => attributes.items = true,
                "mtime" => attributes.mtime = true,
                "notreg" => attributes.not_regular = true,
                other => {
                    return Err(format!(
                        "`{other}` is not an exportable attribute \
                         (name, asize, dsize, items, mtime, notreg)"
                    ));
                }
            }
        }
        Ok(attributes)
    }
}

/// Writes the whole tree as one JSON document.
pub fn export<W: Write>(
    dir: &Dir,
    writer: &mut W,
    attributes: Attributes,
    version: &str,
    timestamp: i64,
) -> io::Result<()> {
    write!(writer, "[1,2,{{\"progname\":\"gdu\",\"progver\":")?;
    serde_json::to_writer(&mut *writer, version)?;
    writeln!(writer, ",\"timestamp\":{timestamp}}},")?;
    write_dir(dir, writer, attributes)?;
    writeln!(writer, "]")
}

fn write_dir<W: Write>(dir: &Dir, writer: &mut W, attributes: Attributes) -> io::Result<()> {
    write!(writer, "[{{\"name\":")?;
    write_name(writer, dir.fs_name())?;

    if attributes.apparent_size {
        write!(writer, ",\"asize\":{}", dir.size)?;
    }
    if attributes.disk_size {
        write!(writer, ",\"dsize\":{}", dir.usage)?;
    }
    if attributes.items {
        write!(writer, ",\"items\":{}", dir.items)?;
    }
    if attributes.mtime && dir.mtime != 0 {
        write!(writer, ",\"mtime\":{}", dir.mtime)?;
    }
    match dir.flag {
        Flag::Unreadable => write!(writer, ",\"read_error\":true")?,
        Flag::ChildUnreadable => write!(writer, ",\"child_read_error\":true")?,
        Flag::Empty => write!(writer, ",\"empty\":true")?,
        _ => {}
    }
    write!(writer, "}}")?;

    for entry in dir.entries() {
        writeln!(writer, ",")?;
        match entry.dir {
            Some(subdir) => write_dir(subdir, writer, attributes)?,
            None => {
                write!(writer, "{{\"name\":")?;
                write_name(writer, entry.fs_name)?;
                if attributes.apparent_size && entry.size != 0 {
                    write!(writer, ",\"asize\":{}", entry.size)?;
                }
                if attributes.disk_size && entry.usage != 0 {
                    write!(writer, ",\"dsize\":{}", entry.usage)?;
                }
                if attributes.mtime && entry.mtime != 0 {
                    write!(writer, ",\"mtime\":{}", entry.mtime)?;
                }
                if attributes.not_regular && entry.flag == Flag::NotRegular {
                    write!(writer, ",\"notreg\":true")?;
                }
                if entry.flag == Flag::HardLink {
                    write!(writer, ",\"hlnkc\":true")?;
                }
                write!(writer, "}}")?;
            }
        }
    }

    write!(writer, "]")
}

fn write_name<W: Write>(writer: &mut W, name: &OsStr) -> io::Result<()> {
    let display = name.to_string_lossy();
    serde_json::to_writer(&mut *writer, display.as_ref())?;

    if name.to_str().is_none() {
        write!(writer, ",\"name_bytes\":[")?;
        for (index, byte) in name.as_bytes().iter().enumerate() {
            if index > 0 {
                write!(writer, ",")?;
            }
            write!(writer, "{byte}")?;
        }
        write!(writer, "]")?;
    }

    Ok(())
}

/// Reads a tree back from an exported document.
pub fn import<R: Read>(reader: R) -> io::Result<Dir> {
    let document: Value = serde_json::from_reader(reader)?;
    let envelope = document
        .as_array()
        .filter(|items| items.len() >= 4)
        .ok_or_else(|| bad_data("expected a four-element report envelope at the top level"))?;
    let tree = envelope[3]
        .as_array()
        .ok_or_else(|| bad_data("the fourth envelope element is not a directory array"))?;

    read_dir(tree, false)
}

fn read_dir(items: &[Value], child: bool) -> io::Result<Dir> {
    let (header, children) = items
        .split_first()
        .ok_or_else(|| bad_data("a directory array is never empty"))?;
    let header = header
        .as_object()
        .ok_or_else(|| bad_data("a directory array starts with its own description"))?;

    let name = read_name(header, "directory")?;
    if child {
        validate_child_name(&name)?;
    }
    let mut dir = Dir::new(&name);

    for child in children {
        match child {
            Value::Array(subdir) => push_dir_checked(&mut dir, read_dir(subdir, true)?)?,
            Value::Object(file) => {
                let name = read_name(file, "file")?;
                validate_child_name(&name)?;
                let hard_link = flag_value(file, "hlnkc")?;
                let not_regular = flag_value(file, "notreg")?;
                let flag = if hard_link {
                    Flag::HardLink
                } else if not_regular {
                    Flag::NotRegular
                } else {
                    Flag::Ok
                };
                let mut stats = FileStats::plain(
                    nonnegative_number(file.get("asize"), "asize")?,
                    nonnegative_number(file.get("dsize"), "dsize")?,
                    number(file.get("mtime"), "mtime")?,
                )
                .with_flag(flag);
                // Hard links were already included in the exported totals.
                stats.counted = flag != Flag::HardLink;
                push_file_checked(&mut dir, &name, stats)?;
            }
            other => return Err(bad_data(&format!("unexpected tree element: {other}"))),
        }
    }

    // Preserve recorded totals that are not recoverable from children.
    if let Some(items) = header.get("items") {
        dir.items = dir.items.max(nonnegative_number(Some(items), "items")?);
    }
    if let Some(size) = header.get("asize") {
        dir.size = dir.size.max(nonnegative_number(Some(size), "asize")?);
    }
    if let Some(usage) = header.get("dsize") {
        dir.usage = dir.usage.max(nonnegative_number(Some(usage), "dsize")?);
    }
    dir.mtime = dir.mtime.max(number(header.get("mtime"), "mtime")?);
    let read_error = flag_value(header, "read_error")?;
    let child_read_error = flag_value(header, "child_read_error")?;
    let empty = flag_value(header, "empty")?;
    if read_error {
        dir.flag = Flag::Unreadable;
    } else if child_read_error {
        dir.flag = Flag::ChildUnreadable;
    } else if empty {
        dir.flag = Flag::Empty;
    }

    Ok(dir)
}

fn push_file_checked(dir: &mut Dir, name: &OsStr, stats: FileStats) -> io::Result<()> {
    checked_sum(dir.items, 1, "items")?;
    if stats.counted {
        checked_sum(dir.size, stats.size, "asize")?;
        checked_sum(dir.usage, stats.usage, "dsize")?;
    }
    dir.push_file(name, stats);
    Ok(())
}

fn push_dir_checked(dir: &mut Dir, child: Dir) -> io::Result<()> {
    checked_sum(dir.items, child.items, "items")?;
    checked_sum(dir.size, child.size, "asize")?;
    checked_sum(dir.usage, child.usage, "dsize")?;
    dir.push_dir(child);
    Ok(())
}

fn checked_sum(total: i64, value: i64, field: &str) -> io::Result<()> {
    total
        .checked_add(value)
        .map(drop)
        .ok_or_else(|| bad_data(&format!("report field `{field}` overflows the tree total")))
}

fn read_name(object: &Map<String, Value>, kind: &str) -> io::Result<OsString> {
    let display = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_data(&format!("a {kind} has no string name")))?;

    let Some(raw) = object.get("name_bytes") else {
        return Ok(OsString::from(display));
    };
    let bytes = raw
        .as_array()
        .ok_or_else(|| bad_data("report field `name_bytes` must be an array"))?;
    let mut name = Vec::with_capacity(bytes.len());
    for byte in bytes {
        let byte = byte
            .as_u64()
            .and_then(|byte| u8::try_from(byte).ok())
            .ok_or_else(|| bad_data("report field `name_bytes` must contain bytes"))?;
        name.push(byte);
    }

    let name = OsString::from_vec(name);
    if name.to_string_lossy() != display {
        return Err(bad_data("report fields `name` and `name_bytes` disagree"));
    }
    Ok(name)
}

fn validate_child_name(name: &OsStr) -> io::Result<()> {
    let mut components = Path::new(name).components();
    if matches!(components.next(), Some(Component::Normal(part)) if part == name)
        && components.next().is_none()
    {
        Ok(())
    } else {
        Err(bad_data(
            "an imported child name must be exactly one normal relative path component",
        ))
    }
}

fn number(value: Option<&Value>, field: &str) -> io::Result<i64> {
    match value {
        None => Ok(0),
        Some(value) => value
            .as_i64()
            .ok_or_else(|| bad_data(&format!("report field `{field}` must be an integer"))),
    }
}

fn nonnegative_number(value: Option<&Value>, field: &str) -> io::Result<i64> {
    let value = number(value, field)?;
    if value < 0 {
        return Err(bad_data(&format!(
            "report field `{field}` must not be negative"
        )));
    }
    Ok(value)
}

fn flag_value(object: &serde_json::Map<String, Value>, field: &str) -> io::Result<bool> {
    match object.get(field) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(bad_data(&format!(
            "report field `{field}` must be a boolean"
        ))),
    }
}

fn bad_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Dir {
        let mut root = Dir::new("/tmp/sample");
        root.push_file("plain.txt", FileStats::plain(4096, 4096, 1_700_000_000));
        root.push_file(
            "link",
            FileStats::plain(12, 0, 1_700_000_100).with_flag(Flag::NotRegular),
        );
        root.push_file(
            "copy",
            FileStats::plain(0, 0, 1_700_000_200).with_flag(Flag::HardLink),
        );

        let mut sub = Dir::new("sub");
        sub.push_file("deep.bin", FileStats::plain(8192, 8192, 1_700_000_300));
        sub.finish(1);
        root.push_dir(sub);
        root.finish(4);
        root
    }

    fn round_trip(dir: &Dir, attributes: Attributes) -> Dir {
        let mut buffer = Vec::new();
        export(dir, &mut buffer, attributes, "test", 1_700_000_000).unwrap();
        import(buffer.as_slice()).unwrap()
    }

    #[test]
    fn a_tree_survives_export_and_import_unchanged() {
        let original = sample();
        let restored = round_trip(&original, Attributes::default());

        assert_eq!(restored.name, original.name);
        assert_eq!(restored.size, original.size);
        assert_eq!(restored.usage, original.usage);
        assert_eq!(restored.items, original.items);
        assert_eq!(restored.mtime, original.mtime);

        let names: Vec<_> = restored.entries().map(|e| e.name).collect();
        assert!(names.contains(&"plain.txt"));
        assert!(names.contains(&"sub"));

        let sub = restored
            .entries()
            .find(|e| e.name == "sub")
            .and_then(|e| e.dir)
            .expect("sub survived as a directory");
        assert_eq!(sub.entries().count(), 1);
    }

    #[test]
    fn empty_directory_size_survives_export_and_import() {
        let mut root = Dir::new("/tmp/sample");
        let mut empty = Dir::new("empty");
        empty.finish(0);
        root.push_dir(empty);
        root.finish(1);

        let restored = round_trip(&root, Attributes::default());
        let empty = restored
            .entries()
            .find(|entry| entry.name == "empty")
            .and_then(|entry| entry.dir)
            .expect("empty directory survived");

        assert_eq!(empty.size, crate::tree::EMPTY_DIR_SIZE);
        assert_eq!(empty.flag, Flag::Empty);
        assert_eq!(restored.size, root.size);
    }

    #[test]
    fn flags_survive_the_round_trip() {
        let restored = round_trip(&sample(), Attributes::default());
        let flag_of = |name: &str| {
            restored
                .entries()
                .find(|e| e.name == name)
                .expect("entry present")
                .flag
        };

        assert_eq!(flag_of("link"), Flag::NotRegular);
        assert_eq!(flag_of("copy"), Flag::HardLink);
        assert_eq!(flag_of("plain.txt"), Flag::Ok);
    }

    #[test]
    fn incomplete_scan_state_survives_the_round_trip() {
        let mut original = sample();
        original.flag = Flag::ChildUnreadable;

        let restored = round_trip(&original, Attributes::default());
        assert_eq!(restored.flag, Flag::ChildUnreadable);
    }

    #[test]
    fn the_report_has_the_expected_envelope() {
        let mut buffer = Vec::new();
        export(&sample(), &mut buffer, Attributes::default(), "9.9", 42).unwrap();
        let document: Value = serde_json::from_slice(&buffer).unwrap();

        let envelope = document.as_array().unwrap();
        assert_eq!(envelope[0], 1);
        assert_eq!(envelope[1], 2);
        assert_eq!(envelope[2]["progname"], "gdu");
        assert_eq!(envelope[2]["progver"], "9.9");
        assert_eq!(envelope[2]["timestamp"], 42);
        assert!(envelope[3].is_array());
    }

    #[test]
    fn selected_attributes_drop_the_rest() {
        let attributes = Attributes::parse("name,asize").unwrap();
        let mut buffer = Vec::new();
        export(&sample(), &mut buffer, attributes, "t", 0).unwrap();
        let text = String::from_utf8(buffer).unwrap();

        assert!(text.contains("\"asize\""));
        assert!(!text.contains("\"dsize\""));
        assert!(!text.contains("\"mtime\""));
        assert!(!text.contains("\"items\""));

        assert!(Attributes::parse("nope").is_err());
    }

    #[test]
    fn names_needing_escapes_survive() {
        let mut dir = Dir::new("/tmp/od\"d");
        dir.push_file("new\nline\t\"quoted\"", FileStats::plain(10, 10, 5));
        dir.finish(1);

        let restored = round_trip(&dir, Attributes::default());
        assert_eq!(restored.name.as_ref(), "/tmp/od\"d");
        assert_eq!(
            restored.entries().next().unwrap().name,
            "new\nline\t\"quoted\""
        );
    }

    #[test]
    fn non_utf8_names_survive_export_and_import() {
        let mut dir = Dir::new("/tmp/root");
        let raw = OsStr::from_bytes(b"bad\xff");
        dir.push_file(raw, FileStats::plain(10, 10, 5));
        dir.finish(1);

        let mut buffer = Vec::new();
        export(&dir, &mut buffer, Attributes::default(), "test", 0).unwrap();
        assert!(
            String::from_utf8(buffer.clone())
                .unwrap()
                .contains("\"name_bytes\":[98,97,100,255]"),
            "raw bytes are present in the report"
        );

        let restored = import(buffer.as_slice()).unwrap();
        let entry = restored.entries().next().unwrap();
        assert_eq!(entry.fs_name.as_bytes(), b"bad\xff");
    }

    #[test]
    fn malformed_documents_are_rejected_rather_than_panicking() {
        assert!(import(b"not json".as_slice()).is_err());
        assert!(import(b"[1,2]".as_slice()).is_err());
        assert!(import(b"[1,2,{},{}]".as_slice()).is_err());
        assert!(import(b"[1,2,{},[]]".as_slice()).is_err());
    }

    #[test]
    fn malformed_field_types_are_rejected_and_false_flags_are_ignored() {
        let false_flags =
            br#"[1,2,{},[{"name":"/root","read_error":false},{"name":"x","asize":10,"dsize":10,"hlnkc":false,"notreg":false}]]"#;
        let restored = import(false_flags.as_slice()).unwrap();
        let entry = restored.entries().next().unwrap();
        assert_eq!(entry.flag, Flag::Ok);
        assert_eq!(restored.size, 10);
        assert_eq!(restored.flag, Flag::Ok);

        let bad_number = br#"[1,2,{},[{"name":"/root"},{"name":"x","asize":"10"}]]"#;
        assert!(import(bad_number.as_slice()).is_err());

        let negative_size = br#"[1,2,{},[{"name":"/root"},{"name":"x","dsize":-1}]]"#;
        assert!(import(negative_size.as_slice()).is_err());

        let bad_flag = br#"[1,2,{},[{"name":"/root","read_error":"yes"}]]"#;
        assert!(import(bad_flag.as_slice()).is_err());
    }

    #[test]
    fn overflowing_tree_totals_are_rejected() {
        let document = br#"[1,2,{},[{"name":"/root"},{"name":"a","asize":9223372036854775807},{"name":"b","asize":1}]]"#;
        let error = import(document.as_slice()).expect_err("the total exceeds i64");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("`asize` overflows"));
    }

    #[test]
    fn imported_children_must_be_single_normal_relative_components() {
        for name in [
            "",
            ".",
            "..",
            "/absolute",
            "a/b",
            "a/../b",
            "./a",
            "a/",
            "a//b",
        ] {
            let document = format!(
                "[1,2,{{}},[{{\"name\":\"/root\"}},{{\"name\":{}}}]]",
                serde_json::to_string(name).unwrap()
            );
            assert!(import(document.as_bytes()).is_err(), "accepted {name:?}");
        }

        let nested = b"[1,2,{},[{\"name\":\"/root\"},[{\"name\":\"../outside\"}]]]";
        assert!(import(nested.as_slice()).is_err());

        let valid = b"[1,2,{},[{\"name\":\"/root\"},{\"name\":\"normal\"}]]";
        assert!(import(valid.as_slice()).is_ok());
    }
}
