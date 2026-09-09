// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Bounded, explicit-unit frequency list imports. No instrument I/O.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;

use calamine::{DataRef, Reader, Xlsx};
use quick_xml::events::Event;

pub const MIN_POINTS: usize = 3;
pub const MAX_POINTS: usize = 1001;
const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ZIP_ENTRIES: usize = 128;
const MAX_UNPACKED_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ROWS: u32 = 4096;
const MAX_CELLS: usize = 16_384;
const MAX_NUMBER_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FrequencyUnit {
    Hz,
    KHz,
    #[default]
    MHz,
}

impl FrequencyUnit {
    pub const ALL: [Self; 3] = [Self::Hz, Self::KHz, Self::MHz];

    pub fn label(self) -> &'static str {
        match self {
            Self::Hz => "Hz",
            Self::KHz => "kHz",
            Self::MHz => "MHz",
        }
    }

    #[cfg(test)]
    fn multiplier(self) -> u64 {
        10_u64.pow(self.power() as u32)
    }

    fn power(self) -> i32 {
        match self {
            Self::Hz => 0,
            Self::KHz => 3,
            Self::MHz => 6,
        }
    }
}

/// List size is a host limit. Device frequency limits are checked by the plan.
pub fn normalize(mut points: Vec<u64>) -> Result<Vec<u64>, String> {
    if !(MIN_POINTS..=MAX_POINTS).contains(&points.len()) {
        return Err(format!(
            "Enter between {MIN_POINTS} and {MAX_POINTS} frequencies"
        ));
    }
    points.sort_unstable();
    Ok(points)
}

#[derive(Debug, PartialEq, Eq)]
struct Decimal {
    negative: bool,
    digits: String,
    power: i32,
}

impl Decimal {
    fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        if value.is_empty() || value.len() > MAX_NUMBER_BYTES {
            return Err("Enter a decimal number".into());
        }
        let (negative, unsigned) = if let Some(value) = value.strip_prefix('-') {
            (true, value)
        } else {
            (false, value.strip_prefix('+').unwrap_or(value))
        };
        let mut parts = unsigned.split(['e', 'E']);
        let mantissa = parts.next().unwrap();
        let exponent = parts.next().map_or(Ok(0), |value| {
            value
                .parse::<i32>()
                .map_err(|_| "Invalid exponent".to_string())
        })?;
        if parts.next().is_some() || !(-400..=400).contains(&exponent) {
            return Err("Invalid exponent".into());
        }
        let mut digits = String::new();
        let mut fraction = None;
        for byte in mantissa.bytes() {
            match byte {
                b'0'..=b'9' => {
                    digits.push(char::from(byte));
                    if let Some(count) = fraction.as_mut() {
                        *count += 1;
                    }
                }
                b'.' if fraction.is_none() => fraction = Some(0),
                _ => return Err("Enter a decimal number, without a unit suffix".into()),
            }
        }
        if digits.is_empty() {
            return Err("Enter a decimal number".into());
        }
        let mut digits = digits.trim_start_matches('0').to_owned();
        if digits.is_empty() {
            return Ok(Self {
                negative: false,
                digits: "0".into(),
                power: 0,
            });
        }
        let mut power = exponent - fraction.unwrap_or(0);
        while digits.ends_with('0') {
            digits.pop();
            power += 1;
        }
        Ok(Self {
            negative,
            digits,
            power,
        })
    }

    fn into_hz(self, unit: FrequencyUnit) -> Result<u64, String> {
        if self.negative {
            return Err("Frequency must not be negative".into());
        }
        if self.digits == "0" {
            return Ok(0);
        }
        let power = self.power + unit.power();
        if power < 0 {
            return Err("Frequency must resolve to whole Hz".into());
        }
        if self.digits.len() + power as usize > 20 {
            return Err("Frequency is too large".into());
        }
        self.digits
            .parse::<u64>()
            .ok()
            .and_then(|value| {
                10_u64
                    .checked_pow(power as u32)
                    .and_then(|scale| value.checked_mul(scale))
            })
            .ok_or_else(|| "Frequency is too large".into())
    }
}

fn header_unit(value: &str) -> Option<FrequencyUnit> {
    let header: String = value
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    FrequencyUnit::ALL.into_iter().find(|unit| {
        let name = unit.label().to_ascii_lowercase();
        header == name
            || ["frequency", "freq"].into_iter().any(|prefix| {
                header == format!("{prefix}({name})")
                    || header == format!("{prefix}[{name}]")
                    || header == format!("{prefix}_{name}")
            })
    })
}

struct Draft {
    unit: FrequencyUnit,
    first: bool,
    values: Vec<u64>,
}

impl Draft {
    fn new(unit: FrequencyUnit) -> Self {
        Self {
            unit,
            first: true,
            values: Vec::new(),
        }
    }

    fn push(&mut self, value: &str, row: u64) -> Result<(), String> {
        let value = value.trim().trim_start_matches('\u{feff}');
        if value.is_empty() {
            return Ok(());
        }
        if self.first
            && let Some(unit) = header_unit(value)
        {
            self.first = false;
            self.unit = unit;
            return Ok(());
        }
        self.first = false;
        let hz = Decimal::parse(value)
            .and_then(|value| value.into_hz(self.unit))
            .map_err(|error| format!("Row {row}: {error}"))?;
        if self.values.len() == MAX_POINTS {
            return Err(format!(
                "Row {row}: at most {MAX_POINTS} frequencies are supported"
            ));
        }
        self.values.push(hz);
        Ok(())
    }
}

/// Read newline values or the first CSV column. A declared unit overrides `unit`.
pub fn parse_text(text: &str, unit: FrequencyUnit) -> Result<Vec<u64>, String> {
    if text.len() > MAX_INPUT_BYTES || text.lines().count() > MAX_ROWS as usize {
        return Err("Frequency text exceeds the 4 MiB or 4096 row limit".into());
    }
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(text.trim_start_matches('\u{feff}').as_bytes());
    let mut draft = Draft::new(unit);
    for record in reader.records() {
        let record = record.map_err(|error| format!("Invalid CSV: {error}"))?;
        let row = record.position().map_or(1, |position| position.line());
        draft.push(record.get(0).unwrap_or_default(), row)?;
    }
    normalize(draft.values)
}

pub fn read_file(path: &Path, unit: FrequencyUnit) -> Result<Vec<u64>, String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "txt" | "csv" | "xlsx") {
        return Err("Choose a TXT, CSV or XLSX file".into());
    }
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_INPUT_BYTES as u64 {
        return Err("Choose a regular file no larger than 4 MiB".into());
    }
    let file = File::open(path).map_err(|error| error.to_string())?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("Choose a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err("Frequency file exceeds the 4 MiB limit".into());
    }
    if extension == "xlsx" {
        parse_xlsx(&bytes, unit)
    } else {
        parse_text(
            std::str::from_utf8(&bytes).map_err(|_| "Text files must use UTF-8")?,
            unit,
        )
    }
}

/// Check the small, non-ZIP64 directory before the ZIP reader reserves entries.
fn check_zip_directory(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_INPUT_BYTES || bytes.len() < 22 {
        return Err("Invalid or oversized XLSX file".into());
    }
    let first = bytes.len().saturating_sub(65_557);
    let offset = (first..=bytes.len() - 22)
        .rev()
        .find(|&index| {
            bytes[index..].starts_with(b"PK\x05\x06")
                && index
                    + 22
                    + usize::from(u16::from_le_bytes([bytes[index + 20], bytes[index + 21]]))
                    == bytes.len()
        })
        .ok_or("Missing XLSX ZIP directory")?;
    let number = |at| u16::from_le_bytes([bytes[offset + at], bytes[offset + at + 1]]);
    if number(4) != 0
        || number(6) != 0
        || number(8) != number(10)
        || usize::from(number(10)) > MAX_ZIP_ENTRIES
        || (offset >= 20 && bytes[offset - 20..].starts_with(b"PK\x06\x07"))
    {
        return Err("XLSX requires a single non-ZIP64 archive with at most 128 entries".into());
    }
    Ok(())
}

fn cell_reference(value: &[u8]) -> Result<(u64, u32), String> {
    let letters = value
        .iter()
        .take_while(|byte| byte.is_ascii_alphabetic())
        .count();
    if !(1..=3).contains(&letters) {
        return Err("Invalid XLSX cell reference".into());
    }
    let column = value[..letters].iter().fold(0_u32, |column, byte| {
        column * 26 + u32::from(byte.to_ascii_uppercase() - b'A') + 1
    });
    let row = std::str::from_utf8(&value[letters..])
        .ok()
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or("Invalid XLSX row number")?;
    if column > 16_384 || row == 0 || row > u64::from(MAX_ROWS) {
        return Err("XLSX exceeds the 4096 row or 16384 column limit".into());
    }
    Ok((row, column))
}

fn check_xml(bytes: &[u8]) -> Result<(), String> {
    let mut reader = quick_xml::Reader::from_reader(bytes);
    let mut events = 0_usize;
    let mut numeric_first_column = false;
    let mut first_column = false;
    let mut in_value = false;
    let mut value = String::new();
    let mut column = 0_usize;
    let mut row = 0_u64;
    loop {
        events += 1;
        if events > 100_000 {
            return Err("XLSX XML exceeds the 100000 event limit".into());
        }
        let event = reader
            .read_event()
            .map_err(|error| format!("Invalid XLSX XML: {error}"))?;
        if in_value
            && !matches!(&event, Event::Text(_))
            && !matches!(&event, Event::End(element) if element.local_name().as_ref() == b"v")
        {
            return Err(format!(
                "Row {row}: numeric cells must contain plain decimal text"
            ));
        }
        match event {
            Event::Start(element) | Event::Empty(element) => {
                let name = element.local_name();
                if matches!(name.as_ref(), b"dimension" | b"f") {
                    for attr in element.attributes() {
                        let attr = attr.map_err(|error| error.to_string())?;
                        if attr.key.as_ref() == b"ref" {
                            let mut references = attr.value.split(|byte| *byte == b':');
                            let start = cell_reference(references.next().unwrap_or_default())?;
                            if let Some(end) = references.next() {
                                let end = cell_reference(end)?;
                                if end.0 < start.0 || end.1 < start.1 {
                                    return Err("XLSX cell range is reversed".into());
                                }
                            }
                            if references.next().is_some() {
                                return Err("Invalid XLSX cell range".into());
                            }
                        }
                    }
                }
                if name.as_ref() == b"Relationship" {
                    let mut relationship_type = String::new();
                    let mut target = String::new();
                    for attr in element.attributes() {
                        let attr = attr.map_err(|error| error.to_string())?;
                        let value = attr
                            .decoded_and_normalized_value(
                                quick_xml::XmlVersion::Implicit1_0,
                                reader.decoder(),
                            )
                            .map_err(|error| error.to_string())?;
                        match attr.key.as_ref() {
                            b"Type" => relationship_type = value.into_owned(),
                            b"Target" => target = value.into_owned(),
                            _ => {}
                        }
                    }
                    if matches!(
                        relationship_type.rsplit('/').next(),
                        Some("worksheet" | "chartsheet" | "dialogsheet")
                    ) && !target.to_ascii_lowercase().ends_with(".xml")
                    {
                        return Err("XLSX worksheet parts must use an .xml filename".into());
                    }
                }
                if name.as_ref() == b"row" {
                    column = 0;
                    row += 1;
                    for attr in element.attributes() {
                        let attr = attr.map_err(|error| error.to_string())?;
                        if attr.key.as_ref() == b"r" {
                            row = std::str::from_utf8(&attr.value)
                                .ok()
                                .and_then(|value| value.parse().ok())
                                .ok_or("Invalid XLSX row number")?;
                        }
                    }
                    if row == 0 || row > u64::from(MAX_ROWS) {
                        return Err("XLSX exceeds the 4096 row limit".into());
                    }
                }
                if name.as_ref() == b"sst" {
                    for attr in element.attributes() {
                        let attr = attr.map_err(|error| error.to_string())?;
                        if attr.key.as_ref() == b"uniqueCount" {
                            let count = std::str::from_utf8(&attr.value)
                                .ok()
                                .and_then(|value| value.parse::<usize>().ok())
                                .ok_or("Invalid shared string count")?;
                            if count > MAX_CELLS {
                                return Err("XLSX shared string count exceeds 16384".into());
                            }
                        }
                    }
                }
                if name.as_ref() == b"c" {
                    let mut first = column == 0;
                    let mut numeric = true;
                    for attr in element.attributes() {
                        let attr = attr.map_err(|error| error.to_string())?;
                        match attr.key.as_ref() {
                            b"r" => {
                                let (cell_row, cell_column) = cell_reference(&attr.value)?;
                                row = cell_row;
                                first = cell_column == 1;
                            }
                            b"t" => numeric = attr.value.as_ref() == b"n",
                            _ => {}
                        }
                    }
                    numeric_first_column = first && numeric;
                    first_column = first;
                    column += 1;
                }
                if name.as_ref() == b"f" && first_column {
                    return Err(format!("Row {row}: formulas are not supported"));
                }
                if name.as_ref() == b"v" && numeric_first_column {
                    in_value = true;
                    value.clear();
                }
            }
            Event::Text(text) if in_value => {
                value.push_str(&text.decode().map_err(|error| error.to_string())?);
                if value.len() > MAX_NUMBER_BYTES {
                    return Err("XLSX numeric cell is too long".into());
                }
            }
            Event::End(element) if element.local_name().as_ref() == b"v" && in_value => {
                in_value = false;
                let decimal =
                    Decimal::parse(&value).map_err(|error| format!("Row {row}: {error}"))?;
                let parsed = value
                    .trim()
                    .parse::<f64>()
                    .map_err(|_| format!("Row {row}: invalid XLSX numeric cell"))?;
                if !parsed.is_finite()
                    || Decimal::parse(&parsed.to_string()).ok().as_ref() != Some(&decimal)
                {
                    return Err(format!(
                        "Row {row}: XLSX number loses precision. Store it as text"
                    ));
                }
            }
            Event::End(element) if element.local_name().as_ref() == b"c" => {
                numeric_first_column = false;
                first_column = false;
                in_value = false;
            }
            Event::DocType(_) => return Err("XLSX XML document types are not supported".into()),
            Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

fn check_xlsx(bytes: &[u8]) -> Result<(), String> {
    check_zip_directory(bytes)?;
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| error.to_string())?;
    if archive.len() > MAX_ZIP_ENTRIES {
        return Err("XLSX has too many ZIP entries".into());
    }
    let mut names = BTreeSet::new();
    let mut declared = 0_u64;
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .map_err(|error| error.to_string())?;
        let name = entry.name();
        if name.starts_with('/')
            || name.contains('\\')
            || name.split('/').any(|part| matches!(part, "." | ".."))
            || !names.insert(name.to_ascii_lowercase())
        {
            return Err("XLSX contains an ambiguous ZIP entry path".into());
        }
        declared = declared
            .checked_add(entry.size())
            .ok_or("XLSX size overflow")?;
        if declared > MAX_UNPACKED_BYTES {
            return Err("XLSX exceeds the 16 MiB unpacked limit".into());
        }
    }
    let mut actual = 0_u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let name = entry.name().to_ascii_lowercase();
        let xml = name.ends_with(".xml") || name.ends_with(".rels");
        let expected = entry.size();
        let mut content = Vec::new();
        entry
            .take(MAX_UNPACKED_BYTES - actual + 1)
            .read_to_end(&mut content)
            .map_err(|error| error.to_string())?;
        actual += content.len() as u64;
        if actual > MAX_UNPACKED_BYTES || content.len() as u64 != expected {
            return Err("XLSX unpacked data exceeds its size limit".into());
        }
        if xml {
            check_xml(&content)?;
        }
    }
    Ok(())
}

fn parse_xlsx(bytes: &[u8], unit: FrequencyUnit) -> Result<Vec<u64>, String> {
    check_xlsx(bytes)?;
    let mut workbook =
        Xlsx::new(Cursor::new(bytes)).map_err(|error| format!("Invalid XLSX: {error}"))?;
    let sheet = workbook
        .sheet_names()
        .first()
        .cloned()
        .ok_or("XLSX has no worksheets")?;
    let mut cells = workbook
        .worksheet_cells_reader(&sheet)
        .map_err(|error| error.to_string())?;
    if cells.dimensions().end.0 >= MAX_ROWS {
        return Err("XLSX exceeds the 4096 row limit".into());
    }
    let mut draft = Draft::new(unit);
    let mut count = 0;
    let mut rows = BTreeSet::new();
    while let Some(cell) = cells
        .next_cell_with_formula_metadata()
        .map_err(|error| error.to_string())?
    {
        count += 1;
        if count > MAX_CELLS || cell.pos.0 >= MAX_ROWS {
            return Err("XLSX exceeds the 4096 row or 16384 cell limit".into());
        }
        if cell.pos.1 != 0 {
            continue;
        }
        let row = u64::from(cell.pos.0) + 1;
        if !rows.insert(row) {
            return Err(format!("Row {row}: repeated column A cell"));
        }
        if cell.formula.is_some() {
            return Err(format!("Row {row}: formulas are not supported"));
        }
        let value = match cell.value {
            DataRef::Empty => continue,
            DataRef::Int(value) => value.to_string(),
            DataRef::Float(value) => value.to_string(),
            DataRef::String(value) => value,
            DataRef::SharedString(value) => value.into(),
            _ => {
                return Err(format!(
                    "Row {row}: expected a number, not a boolean, date or error"
                ));
            }
        };
        draft.push(&value, row)?;
    }
    normalize(draft.values)
}

pub fn template_bytes() -> Result<Vec<u8>, String> {
    let mut workbook = rust_xlsxwriter::Workbook::new();
    let sheet = workbook.add_worksheet();
    sheet
        .set_name("Frequencies")
        .map_err(|error| error.to_string())?;
    sheet
        .write_string(0, 0, "frequency_hz")
        .map_err(|error| error.to_string())?;
    for (row, hz) in [1_000_000, 10_000_000, 100_000_000].into_iter().enumerate() {
        sheet
            .write_number(row as u32 + 1, 0, hz)
            .map_err(|error| error.to_string())?;
    }
    sheet
        .set_column_width(0, 22)
        .map_err(|error| error.to_string())?;
    workbook.save_to_buffer().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_entries(entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in entries {
            writer
                .start_file(
                    name,
                    zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated),
                )
                .unwrap();
            writer.write_all(&data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn replace_entry(bytes: &[u8], name: &str, data: &[u8]) -> Vec<u8> {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut entries = Vec::new();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let entry_name = entry.name().to_owned();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            entries.push((
                entry_name.clone(),
                if entry_name == name {
                    data.to_vec()
                } else {
                    content
                },
            ));
        }
        zip_entries(entries)
    }

    fn rename_entry(bytes: &[u8], from: &str, to: &str) -> Vec<u8> {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut entries = Vec::new();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let name = if entry.name() == from {
                to
            } else {
                entry.name()
            }
            .to_owned();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            entries.push((name, content));
        }
        zip_entries(entries)
    }

    fn sheet_fixture(body: &str) -> Vec<u8> {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{body}</sheetData></worksheet>"#
        );
        replace_entry(
            &template_bytes().unwrap(),
            "xl/worksheets/sheet1.xml",
            xml.as_bytes(),
        )
    }

    fn numeric_row(row: u32, value: &str) -> String {
        format!(r#"<row r="{row}"><c r="A{row}"><v>{value}</v></c></row>"#)
    }

    #[test]
    fn text_units_headers_sort_and_duplicates_are_explicit() {
        for (unit, expected) in [
            (FrequencyUnit::Hz, 2),
            (FrequencyUnit::KHz, 2000),
            (FrequencyUnit::MHz, 2_000_000),
        ] {
            assert_eq!(
                parse_text("3\n2\n2\n", unit).unwrap(),
                vec![expected, expected, expected / 2 * 3]
            );
            assert_eq!(unit.multiplier() * 2, expected);
        }
        for header in [
            "Frequency(Hz)",
            "frequency_hz",
            "freq_hz",
            " Frequency [ Hz ] ",
            "Hz",
        ] {
            let text = format!(
                "\u{feff}\r\n{header},note\r\n3000000,third\r\n1000000,first\r\n1000000,again\r\n"
            );
            assert_eq!(
                parse_text(&text, FrequencyUnit::MHz).unwrap(),
                [1_000_000, 1_000_000, 3_000_000]
            );
        }
        assert_eq!(
            parse_text("MHz\n1.5\n2e-6\n0.000001", FrequencyUnit::Hz).unwrap(),
            [1, 2, 1_500_000]
        );
        assert_eq!(
            parse_text("3,unused\n1,unused\n2,unused", FrequencyUnit::Hz).unwrap(),
            [1, 2, 3]
        );
    }

    #[test]
    fn exact_decimal_conversion_rejects_fractional_hz_and_overflow() {
        for value in [
            "0.1",
            "1.00000000000000000001",
            "18446744073709551616",
            "NaN",
            "Inf",
            "-1",
            "1e999",
            "0x10",
            "1Hz",
            "1e",
            ".",
            "1.2.3",
        ] {
            let error = parse_text(&format!("1\n2\n{value}"), FrequencyUnit::Hz).unwrap_err();
            assert!(error.contains("Row 3"), "{value}: {error}");
        }
        assert!(
            parse_text("1\n2\n0.0000001", FrequencyUnit::MHz)
                .unwrap_err()
                .contains("whole Hz")
        );
        assert_eq!(
            parse_text("18446744073709551615\n0\n+1.000e0", FrequencyUnit::Hz).unwrap(),
            [0, 1, u64::MAX]
        );
        assert!(
            parse_text("frequency\n1\n2\n3", FrequencyUnit::Hz)
                .unwrap_err()
                .contains("Row 1")
        );
        assert!(
            parse_text("Hz\n1\nMHz\n2\n3", FrequencyUnit::Hz)
                .unwrap_err()
                .contains("Row 3")
        );
    }

    #[test]
    fn text_count_bytes_and_row_limits_are_bounded() {
        for count in [0, 1, 2, 1002] {
            assert!(parse_text(&"1\n".repeat(count), FrequencyUnit::Hz).is_err());
        }
        assert_eq!(
            parse_text(&"1\n".repeat(MAX_POINTS), FrequencyUnit::Hz)
                .unwrap()
                .len(),
            MAX_POINTS
        );
        assert!(parse_text(&" ".repeat(MAX_INPUT_BYTES + 1), FrequencyUnit::Hz).is_err());
        assert!(
            parse_text(
                &format!("{}1\n2\n3", "\n".repeat(MAX_ROWS as usize)),
                FrequencyUnit::Hz
            )
            .is_err()
        );
        assert_eq!(normalize(vec![3, 1, 1]).unwrap(), [1, 1, 3]);
    }

    #[test]
    fn original_template_and_first_numeric_xlsx_row_round_trip() {
        assert_eq!(
            parse_xlsx(&template_bytes().unwrap(), FrequencyUnit::MHz).unwrap(),
            [1_000_000, 10_000_000, 100_000_000]
        );
        let bytes =
            sheet_fixture(&(numeric_row(1, "3") + &numeric_row(3, "1") + &numeric_row(4, "1")));
        assert_eq!(
            parse_xlsx(&bytes, FrequencyUnit::KHz).unwrap(),
            [1000, 1000, 3000]
        );
        let bytes = sheet_fixture(
            &(numeric_row(1, "1.5") + &numeric_row(2, "0.000001") + &numeric_row(3, "2e-6")),
        );
        assert_eq!(
            parse_xlsx(&bytes, FrequencyUnit::MHz).unwrap(),
            [1, 2, 1_500_000]
        );
    }

    #[test]
    fn xlsx_reads_only_first_sheet_first_column_and_checked_text() {
        let mut workbook = rust_xlsxwriter::Workbook::new();
        let sheet = workbook.add_worksheet();
        sheet.write_string(0, 0, "Frequency(MHz)").unwrap();
        sheet.write_string(1, 0, "0.000003").unwrap();
        sheet.write_number(2, 0, 0.000001).unwrap();
        sheet.write_string(3, 0, "2e-6").unwrap();
        sheet.write_boolean(1, 1, false).unwrap();
        workbook
            .add_worksheet()
            .write_string(0, 0, "Ignored worksheet")
            .unwrap();
        let bytes = workbook.save_to_buffer().unwrap();
        assert_eq!(parse_xlsx(&bytes, FrequencyUnit::Hz).unwrap(), [1, 2, 3]);
    }

    #[test]
    fn xlsx_rejects_formulas_cached_results_and_invalid_cell_types() {
        for cell in [
            r#"<c r="A2"><f>1+1</f><v>2</v></c>"#,
            r#"<c r="A2"><f/><v>2</v></c>"#,
            r#"<c r="A2"><f t="shared" si="1000000000" ref="A2:A4">1+1</f><v>2</v></c>"#,
            r#"<c r="A2"><f t="shared" si="1000000000"/><v>2</v></c>"#,
            r#"<c r="A2" t="b"><v>1</v></c>"#,
            r#"<c r="A2" t="e"><v>#VALUE!</v></c>"#,
            r#"<c r="A2" t="d"><v>2026-09-09</v></c>"#,
            r#"<c r="A2"><v>1.5</v></c>"#,
            r#"<c r="A2"><v>1.0000000000000000000001</v></c>"#,
        ] {
            let bytes = sheet_fixture(
                &(numeric_row(1, "1")
                    + &format!(r#"<row r="2">{cell}</row>"#)
                    + &numeric_row(3, "3")),
            );
            let error = parse_xlsx(&bytes, FrequencyUnit::Hz).unwrap_err();
            assert!(error.contains("Row 2"), "{cell}: {error}");
        }
    }

    #[test]
    fn xlsx_numeric_encoding_cannot_bypass_exact_precision_checks() {
        for value in [
            "<![CDATA[1.0000000000000000000001]]>",
            "&#49;.0000000000000000000001",
            "1<!-- ignored -->.0000000000000000000001",
            "<?number ignored?>1",
        ] {
            let body = numeric_row(1, "1") + &numeric_row(2, value) + &numeric_row(3, "3");
            let error = parse_xlsx(&sheet_fixture(&body), FrequencyUnit::Hz).unwrap_err();
            assert!(error.contains("Row 2"), "{value}: {error}");
            assert!(error.contains("plain decimal text"), "{value}: {error}");
        }
        let body = numeric_row(1, "1")
            + r#"<row r="2"><c r="a2"><v>1.0000000000000000000001</v></c></row>"#
            + &numeric_row(3, "3");
        assert!(
            parse_xlsx(&sheet_fixture(&body), FrequencyUnit::Hz)
                .unwrap_err()
                .contains("loses precision")
        );
    }

    #[test]
    fn xlsx_untrusted_references_are_checked_before_calamine_arithmetic() {
        for reference in [
            "A0",
            "A999999999999999999999",
            "ZZZZZZZZZZZZ1",
            "XFE1",
            "A4097",
        ] {
            let body = format!(r#"<row r="1"><c r="{reference}"><v>1</v></c></row>"#);
            assert!(
                parse_xlsx(&sheet_fixture(&body), FrequencyUnit::Hz).is_err(),
                "{reference}"
            );
        }
        let bytes = replace_entry(
            &template_bytes().unwrap(),
            "xl/worksheets/sheet1.xml",
            br#"<worksheet><dimension ref="A1:A999999999999999999"/><sheetData/></worksheet>"#,
        );
        assert!(parse_xlsx(&bytes, FrequencyUnit::Hz).is_err());
        let bytes = sheet_fixture(r#"<row r="999999999999999999"><c><v>1</v></c></row>"#);
        assert!(parse_xlsx(&bytes, FrequencyUnit::Hz).is_err());
        for range in ["A3:A1", "B1:A1", "A1:B2:C3"] {
            let xml = format!(r#"<worksheet><dimension ref="{range}"/><sheetData/></worksheet>"#);
            let bytes = replace_entry(
                &template_bytes().unwrap(),
                "xl/worksheets/sheet1.xml",
                xml.as_bytes(),
            );
            assert!(check_xlsx(&bytes).is_err(), "{range}");
        }
    }

    #[test]
    fn xlsx_case_and_relationship_targets_cannot_skip_xml_preflight() {
        let bytes = replace_entry(
            &template_bytes().unwrap(),
            "xl/sharedStrings.xml",
            br#"<sst uniqueCount="999999999"><si><t>Hz</t></si></sst>"#,
        );
        let bytes = rename_entry(&bytes, "xl/sharedStrings.xml", "xl/sharedStrings.XML");
        assert!(
            check_xlsx(&bytes)
                .unwrap_err()
                .contains("shared string count")
        );

        let template = template_bytes().unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(&template)).unwrap();
        let mut relationships = String::new();
        archive
            .by_name("xl/_rels/workbook.xml.rels")
            .unwrap()
            .read_to_string(&mut relationships)
            .unwrap();
        let bytes = replace_entry(
            &template,
            "xl/_rels/workbook.xml.rels",
            relationships.replace("sheet1.xml", "sheet1.bin").as_bytes(),
        );
        let bytes = rename_entry(
            &bytes,
            "xl/worksheets/sheet1.xml",
            "xl/worksheets/sheet1.bin",
        );
        assert!(check_xlsx(&bytes).unwrap_err().contains(".xml filename"));
    }

    #[test]
    fn xlsx_count_sparse_rows_and_cells_are_bounded() {
        let bytes = sheet_fixture(
            &(numeric_row(1, "1") + &numeric_row(2, "2") + &numeric_row(MAX_ROWS + 1, "3")),
        );
        assert!(
            parse_xlsx(&bytes, FrequencyUnit::Hz)
                .unwrap_err()
                .contains("row")
        );
        let body: String = (1..=MAX_POINTS as u32 + 1)
            .map(|row| numeric_row(row, "1"))
            .collect();
        assert!(
            parse_xlsx(&sheet_fixture(&body), FrequencyUnit::Hz)
                .unwrap_err()
                .contains("1001")
        );
        let body = format!(
            "{}{}{}",
            numeric_row(1, "1"),
            numeric_row(1, "2"),
            numeric_row(2, "3")
        );
        assert!(
            parse_xlsx(&sheet_fixture(&body), FrequencyUnit::Hz)
                .unwrap_err()
                .contains("repeated")
        );
        let body = format!(
            r#"<row r="1">{}</row>"#,
            r#"<c r="B1"><v>1</v></c>"#.repeat(MAX_CELLS + 1)
        );
        assert!(
            parse_xlsx(&sheet_fixture(&body), FrequencyUnit::Hz)
                .unwrap_err()
                .contains("cell limit")
        );
    }

    #[test]
    fn xlsx_zip_and_metadata_limits_precede_workbook_allocation() {
        let oversized = zip_entries(vec![(
            "large.xml".into(),
            vec![b' '; MAX_UNPACKED_BYTES as usize + 1],
        )]);
        assert!(
            check_xlsx(&oversized)
                .unwrap_err()
                .contains("unpacked limit")
        );
        let many = zip_entries(
            (0..=MAX_ZIP_ENTRIES)
                .map(|index| (format!("{index}.xml"), vec![]))
                .collect(),
        );
        assert!(check_xlsx(&many).unwrap_err().contains("128 entries"));
        let bytes = replace_entry(
            &template_bytes().unwrap(),
            "xl/sharedStrings.xml",
            br#"<sst uniqueCount="999999999"><si><t>Hz</t></si></sst>"#,
        );
        assert!(
            parse_xlsx(&bytes, FrequencyUnit::Hz)
                .unwrap_err()
                .contains("shared string count")
        );
        let bad_path = zip_entries(vec![("xl/../sheet.xml".into(), vec![])]);
        assert!(check_xlsx(&bad_path).unwrap_err().contains("ambiguous"));
        let duplicates = zip_entries(vec![("A.xml".into(), vec![]), ("a.xml".into(), vec![])]);
        assert!(check_xlsx(&duplicates).unwrap_err().contains("ambiguous"));
        let mut bytes = template_bytes().unwrap();
        let offset = bytes.len() - 22;
        bytes[offset + 8..offset + 12].copy_from_slice(&[255; 4]);
        assert!(check_xlsx(&bytes).unwrap_err().contains("128 entries"));
        let mut bytes = zip_entries(vec![("small.xml".into(), vec![b' '; 1024])]);
        let central = bytes
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .unwrap();
        bytes[central + 24..central + 28].copy_from_slice(&1_u32.to_le_bytes());
        assert!(check_xlsx(&bytes).is_err());
    }

    #[test]
    fn files_require_supported_extension_utf8_and_size() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("frequency.csv");
        std::fs::write(&path, "3\n1\n2").unwrap();
        assert_eq!(read_file(&path, FrequencyUnit::Hz).unwrap(), [1, 2, 3]);
        std::fs::write(&path, [255]).unwrap();
        assert!(
            read_file(&path, FrequencyUnit::Hz)
                .unwrap_err()
                .contains("UTF-8")
        );
        File::create(&path)
            .unwrap()
            .set_len(MAX_INPUT_BYTES as u64 + 1)
            .unwrap();
        assert!(
            read_file(&path, FrequencyUnit::Hz)
                .unwrap_err()
                .contains("4 MiB")
        );
        assert!(
            read_file(&directory.path().join("frequency.xls"), FrequencyUnit::Hz)
                .unwrap_err()
                .contains("TXT, CSV or XLSX")
        );
        let path = directory.path().join("frequency.XLSX");
        std::fs::write(&path, template_bytes().unwrap()).unwrap();
        assert_eq!(
            read_file(&path, FrequencyUnit::MHz).unwrap(),
            [1_000_000, 10_000_000, 100_000_000]
        );
    }
}
